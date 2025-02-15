mod cash_flows;
mod corporate_actions;
mod dividends;
mod fees;
mod interest;
mod merging;
mod partial;
mod payments;
mod reader;
mod taxes;
mod trades;
mod validators;

mod bcs;
mod firstrade;
mod ib;
mod open;
mod tinkoff;

use std::cmp::Ordering;
use std::collections::{HashMap, BTreeMap, BTreeSet, hash_map::Entry};

use log::warn;

use crate::brokers::{BrokerInfo, Broker};
use crate::commissions::CommissionCalc;
use crate::core::{EmptyResult, GenericResult};
use crate::currency::{Cash, CashAssets, MultiCurrencyCashAccount};
use crate::currency::converter::CurrencyConverter;
use crate::exchanges::Exchanges;
use crate::formatting;
use crate::instruments::{InstrumentInternalIds, InstrumentInfo};
use crate::localities;
use crate::quotes::{Quotes, QuoteQuery};
use crate::taxes::TaxRemapping;
use crate::time::{self, Date, DateOptTime, Period};
use crate::types::{Decimal, TradeType};
use crate::util;

use self::dividends::{DividendAccruals, process_dividend_accruals};
use self::partial::PartialBrokerStatement;
use self::reader::BrokerStatementReader;
use self::taxes::{TaxId, TaxAccruals};
use self::validators::{DateValidator, sort_and_validate_trades};

pub use self::cash_flows::{CashFlow, CashFlowType};
pub use self::corporate_actions::{
    CorporateAction, CorporateActionType, StockSplitController, process_corporate_actions};
pub use self::dividends::Dividend;
pub use self::fees::Fee;
pub use self::interest::IdleCashInterest;
pub use self::merging::StatementsMergingStrategy;
pub use self::reader::ReadingStrictness;
pub use self::taxes::TaxWithholding;
pub use self::trades::{
    ForexTrade, StockBuy, StockSource, StockSell, StockSellType, StockSellSource, StockSourceDetails,
    SellDetails, FifoDetails};

pub struct BrokerStatement {
    pub broker: BrokerInfo,
    pub period: Period,

    pub cash_assets: MultiCurrencyCashAccount,
    pub historical_assets: BTreeMap<Date, NetAssets>,

    pub fees: Vec<Fee>,
    pub cash_flows: Vec<CashFlow>,
    pub deposits_and_withdrawals: Vec<CashAssets>,
    pub idle_cash_interest: Vec<IdleCashInterest>,
    pub tax_agent_withholdings: Vec<TaxWithholding>,

    pub exchanges: Exchanges,
    pub forex_trades: Vec<ForexTrade>,
    pub stock_buys: Vec<StockBuy>,
    pub stock_sells: Vec<StockSell>,
    pub dividends: Vec<Dividend>,

    corporate_actions: Vec<CorporateAction>,
    pub stock_splits: StockSplitController,

    pub open_positions: HashMap<String, Decimal>,
    pub instrument_info: InstrumentInfo,
}

impl BrokerStatement {
    pub fn read(
        broker: BrokerInfo, statement_dir_path: &str,
        symbol_remapping: &HashMap<String, String>, instrument_internal_ids: &InstrumentInternalIds,
        instrument_names: &HashMap<String, String>, tax_remapping: TaxRemapping,
        corporate_actions: &[CorporateAction], strictness: ReadingStrictness,
    ) -> GenericResult<BrokerStatement> {
        let broker_jurisdiction = broker.type_.jurisdiction();

        let mut statements = reader::read(broker.type_, statement_dir_path, tax_remapping, strictness)?;
        statements.sort_by_key(|statement| statement.period.unwrap());

        let last_index = statements.len() - 1;
        let last_date = statements.last().unwrap().period.unwrap().last_date();

        let mut statement = BrokerStatement::new_empty_from(broker, statements.first().unwrap())?;
        statement.instrument_info.set_internal_ids(instrument_internal_ids.clone());

        let mut dividend_accruals = HashMap::new();
        let mut tax_accruals = HashMap::new();

        for (index, mut partial) in statements.into_iter().enumerate() {
            for (dividend_id, accruals) in partial.dividend_accruals.drain() {
                dividend_accruals.entry(dividend_id)
                    .and_modify(|existing: &mut DividendAccruals| existing.merge(&accruals))
                    .or_insert(accruals);
            }

            for (tax_id, accruals) in partial.tax_accruals.drain() {
                tax_accruals.entry(tax_id)
                    .and_modify(|existing: &mut TaxAccruals| existing.merge(&accruals))
                    .or_insert(accruals);
            }

            statement.merge(partial, last_date, index == 0, index == last_index).map_err(|e| format!(
                "Failed to merge broker statements: {}", e))?;
        }

        for (dividend_id, accruals) in dividend_accruals {
            let instrument = statement.instrument_info.get_or_add_by_id(&dividend_id.issuer)?;
            let taxation_type = instrument.get_taxation_type(broker_jurisdiction)?;

            let (dividend, cash_flows) = process_dividend_accruals(
                dividend_id, &instrument.symbol, taxation_type, accruals, &mut tax_accruals, true)?;

            if let Some(dividend) = dividend {
                statement.dividends.push(dividend);
            }

            statement.cash_flows.extend(cash_flows.into_iter());
        }

        if !tax_accruals.is_empty() {
            let taxes = tax_accruals.keys()
                .map(|tax: &TaxId| format!(
                    "* {date}: {issuer}", date=formatting::format_date(tax.date),
                    issuer=tax.issuer))
                .collect::<Vec<_>>()
                .join("\n");

            let mut hint = String::new();
            if statement.broker.type_ == Broker::InteractiveBrokers {
                // https://github.com/KonishchevDmitry/investments/blob/master/docs/brokers.md#ib-tax-remapping
                let url = "http://bit.ly/investments-ib-tax-remapping";
                hint = format!("\n\nProbably manual tax remapping rules are required (see {})", url);
            }

            return Err!("Unable to find origin operations for the following taxes:\n{}{}", taxes, hint);
        }

        for (symbol, new_symbol) in symbol_remapping.iter() {
            statement.rename_symbol(symbol, new_symbol, None).map_err(|e| format!(
                "Failed to remap {} to {}: {}", symbol, new_symbol, e))?;
        }

        statement.corporate_actions.extend(corporate_actions.iter().cloned());

        for (symbol, name) in instrument_names {
            statement.instrument_info.get_or_add(symbol).set_name(name);
        }

        statement.validate(strictness)?;

        process_corporate_actions(&mut statement)?;
        statement.process_trades(None)?;

        Ok(statement)
    }

    fn new_empty_from(broker: BrokerInfo, statement: &PartialBrokerStatement) -> GenericResult<BrokerStatement> {
        let period = statement.get_period()?;

        if statement.get_has_starting_assets()? {
            return Err!(concat!(
                "The first broker statement ({}) has a non-zero starting assets. ",
                "Make sure that broker statements directory contains statements for all periods ",
                "starting from account opening",
            ), period.format());
        }

        Ok(BrokerStatement {
            broker, period,

            cash_assets: MultiCurrencyCashAccount::new(),
            historical_assets: BTreeMap::new(),

            fees: Vec::new(),
            cash_flows: Vec::new(),
            deposits_and_withdrawals: Vec::new(),
            idle_cash_interest: Vec::new(),
            tax_agent_withholdings: Vec::new(),

            exchanges: Exchanges::new_empty(),
            forex_trades: Vec::new(),
            stock_buys: Vec::new(),
            stock_sells: Vec::new(),
            dividends: Vec::new(),

            corporate_actions: Vec::new(),
            stock_splits: StockSplitController::default(),

            open_positions: HashMap::new(),
            instrument_info: InstrumentInfo::new(),
        })
    }

    pub fn check_date(&self) {
        let days = (time::today() - self.period.last_date()).num_days();
        let months = Decimal::from(days) / dec!(30);

        if months >= dec!(1) {
            warn!("{} broker statement is {} months old and may be outdated.",
                  self.broker.brief_name, util::round(months, 1));
        }
    }

    pub fn check_period_against_tax_year(&self, year: i32) -> GenericResult<Period> {
        let tax_period_start = date!(year, 1, 1);
        let tax_period_end = date!(year, 12, 31);

        if tax_period_end < self.period.first_date() || self.period.last_date() < tax_period_start {
            return Err!(concat!(
                "Period of the specified broker statement ({}) ",
                "doesn't overlap with the requested tax year ({})"),
                self.period.format(), year);
        }

        if self.period.last_date() < tax_period_end {
            warn!(concat!(
                "Period of the specified broker statement ({}) ",
                "doesn't fully overlap with the requested tax year ({})."
            ), self.period.format(), year);
        }

        Period::new(
            std::cmp::max(tax_period_start, self.period.first_date()),
            std::cmp::min(tax_period_end, self.period.last_date()),
        )
    }

    pub fn batch_quotes(&self, quotes: &Quotes) -> EmptyResult {
        for symbol in self.open_positions.keys() {
            quotes.batch(self.get_quote_query(symbol))?;
        }
        Ok(())
    }

    pub fn get_quote_query(&self, symbol: &str) -> QuoteQuery {
        let exchanges = match self.instrument_info.get(symbol) {
            Some(instrument) if !instrument.exchanges.is_empty() => &instrument.exchanges,
            _ => &self.exchanges,
        };
        QuoteQuery::Stock(symbol.to_owned(), exchanges.get_prioritized())
    }

    pub fn net_value(&self, converter: &CurrencyConverter, quotes: &Quotes, currency: &str) -> GenericResult<Cash> {
        self.batch_quotes(quotes)?;

        let mut net_value = self.cash_assets.total_assets_real_time(currency, converter)?;

        for (symbol, quantity) in &self.open_positions {
            let price = quotes.get(self.get_quote_query(symbol))?;
            let price = converter.real_time_convert_to(price, currency)?;
            net_value += quantity * price;
        }

        Ok(Cash::new(currency, net_value))
    }

    pub fn emulate_sell(
        &mut self, symbol: &str, quantity: Decimal, price: Cash,
        commission_calc: &mut CommissionCalc,
    ) -> EmptyResult {
        let conclusion_time = time::today_trade_conclusion_time();
        let mut execution_date = time::today_trade_execution_date();

        for trade in self.stock_sells.iter().rev() {
            if trade.execution_date > execution_date {
                execution_date = trade.execution_date;
            }

            if trade.symbol == symbol {
                break
            }
        }

        let volume = price * quantity;
        let commission = commission_calc.add_trade(
            conclusion_time.date, TradeType::Sell, quantity, price)?;

        let stock_sell = StockSell::new_trade(
            symbol, quantity, price, volume, commission,
            conclusion_time, execution_date, true);

        if let Entry::Occupied(mut open_position) = self.open_positions.entry(symbol.to_owned()) {
            let available = open_position.get_mut();

            match quantity.cmp(available) {
                Ordering::Equal => {
                    open_position.remove();
                },
                Ordering::Less => {
                    *available = (*available - quantity).normalize();
                },
                Ordering::Greater => {
                    return Err!("The portfolio has not enough open positions for {}", symbol);
                },
            }
        } else {
            return Err!("The portfolio has no open {} position", symbol);
        }

        self.cash_assets.deposit(volume);
        self.cash_assets.withdraw(commission);
        self.stock_sells.push(stock_sell);

        Ok(())
    }

    pub fn emulate_commissions(&mut self, commission_calc: CommissionCalc) -> GenericResult<MultiCurrencyCashAccount> {
        let mut total = MultiCurrencyCashAccount::new();

        for commissions in commission_calc.calculate()?.values() {
            for commission in commissions.iter() {
                self.cash_assets.withdraw(commission);
                total.deposit(commission);
            }
        }

        Ok(total)
    }

    pub fn process_trades(&mut self, until: Option<DateOptTime>) -> EmptyResult {
        let mut unsold_buys: HashMap<String, Vec<usize>> = HashMap::new();

        for (index, stock_buy) in self.stock_buys.iter().enumerate().rev() {
            if let Some(time) = until {
                if stock_buy.conclusion_time >= time {
                    continue;
                }
            }

            if stock_buy.is_sold() {
                continue;
            }

            let symbol_buys = match unsold_buys.get_mut(&stock_buy.symbol) {
                Some(symbol_buys) => symbol_buys,
                None => unsold_buys.entry(stock_buy.symbol.clone()).or_insert_with(Vec::new),
            };

            symbol_buys.push(index);
        }

        for stock_sell in &mut self.stock_sells {
            if let Some(time) = until {
                if stock_sell.conclusion_time >= time {
                    continue;
                }
            }

            if stock_sell.is_processed() {
                continue;
            }

            let mut remaining_quantity = stock_sell.quantity;
            let mut sources = Vec::new();

            let symbol_buys = unsold_buys.get_mut(&stock_sell.symbol).ok_or_else(|| format!(
                "Error while processing {} position closing: There are no open positions for it",
                stock_sell.original_symbol
            ))?;

            while !remaining_quantity.is_zero() {
                let index = symbol_buys.last().copied().ok_or_else(|| format!(
                    "Error while processing {} position closing: There are no open positions for it",
                    stock_sell.original_symbol
                ))?;

                let stock_buy = &mut self.stock_buys[index];
                let multiplier = self.stock_splits.get_multiplier(
                    &stock_sell.symbol, stock_buy.conclusion_time, stock_sell.conclusion_time);

                let unsold_quantity = multiplier * stock_buy.get_unsold();
                let sell_quantity = std::cmp::min(remaining_quantity, unsold_quantity);
                assert!(sell_quantity > dec!(0));

                let source_quantity = (sell_quantity / multiplier).normalize();
                assert_eq!(source_quantity * multiplier, sell_quantity);

                sources.push(stock_buy.sell(source_quantity, multiplier));
                remaining_quantity -= sell_quantity;

                if stock_buy.is_sold() {
                    symbol_buys.pop();
                }
            }

            stock_sell.process(sources);
        }

        if until.is_none() {
            self.validate_open_positions()?;
        }

        Ok(())
    }

    fn merge(
        &mut self, statement: PartialBrokerStatement, last_date: Date, first: bool, last: bool,
    ) -> EmptyResult {
        if !first {
            let period = statement.get_period()?;
            self.broker.statements_merging_strategy.validate(self.period, period, last_date)?;
            self.period = Period::new(self.period.first_date(), period.last_date()).unwrap();
        }

        if let partial::NetAssets{cash: Some(cash), other} = statement.assets {
            self.cash_assets = cash.clone();

            let assets = NetAssets{cash, other};
            assert!(self.historical_assets.insert(self.period.last_date(), assets).is_none());
        } else if last {
            return Err!("Unable to find any information about current cash assets");
        }

        self.fees.extend(statement.fees.into_iter());
        self.cash_flows.extend(statement.cash_flows.into_iter());
        self.deposits_and_withdrawals.extend(statement.deposits_and_withdrawals.into_iter());
        self.idle_cash_interest.extend(statement.idle_cash_interest.into_iter());
        self.tax_agent_withholdings.extend(statement.tax_agent_withholdings.into_iter());

        self.exchanges.merge(statement.exchanges);
        self.forex_trades.extend(statement.forex_trades.into_iter());
        self.stock_buys.extend(statement.stock_buys.into_iter());
        self.stock_sells.extend(statement.stock_sells.into_iter());

        self.corporate_actions.extend(statement.corporate_actions.into_iter());
        self.open_positions = statement.open_positions;
        self.instrument_info.merge(statement.instrument_info);

        Ok(())
    }

    fn rename_symbol(&mut self, symbol: &str, new_symbol: &str, time: Option<DateOptTime>) -> EmptyResult {
        // For now don't introduce any enums here:
        // * When date is set - it's always a corporate action.
        // * In other case it's a manual remapping.
        let remapping = time.is_none();

        let mut found = false;
        let mut rename = |operation_time: DateOptTime, operation_symbol: &mut String, operation_original_symbol: &mut String| {
            if let Some(time) = time {
                if operation_time > time {
                    return;
                }
            }

            if *operation_symbol == symbol {
                *operation_symbol = new_symbol.to_owned();
                found = true;
            }

            if remapping {
                if *operation_original_symbol == symbol {
                    *operation_original_symbol = new_symbol.to_owned();
                    found = true;
                }
            }
        };

        if remapping {
            if self.open_positions.contains_key(new_symbol) {
                return Err!("The portfolio already has {} symbol", new_symbol);
            }

            if let Some(quantity) = self.open_positions.remove(symbol) {
                self.open_positions.insert(new_symbol.to_owned(), quantity);
            }

            self.instrument_info.remap(symbol, new_symbol)?;
        } else {
            self.stock_splits.rename(symbol, new_symbol)?;
        }

        for trade in &mut self.stock_buys {
            rename(trade.conclusion_time, &mut trade.symbol, &mut trade.original_symbol);
        }

        for trade in &mut self.stock_sells {
            rename(trade.conclusion_time, &mut trade.symbol, &mut trade.original_symbol);
        }

        for dividend in &mut self.dividends {
            rename(dividend.date.into(), &mut dividend.issuer, &mut dividend.original_issuer);
        }

        if remapping {
            for cash_flow in &mut self.cash_flows {
                if let Some(original_symbol) = cash_flow.mut_symbol() {
                    if *original_symbol == symbol {
                        *original_symbol = new_symbol.to_owned();
                    }
                }
            }
        }

        if !found {
            return Err!("Unable to find any operation with it in the broker statement");
        }

        Ok(())
    }

    fn validate(&mut self, strictness: ReadingStrictness) -> EmptyResult {
        let validator = DateValidator::new(self.period);

        validator.sort_and_validate(
            "a deposit of withdrawal", &mut self.deposits_and_withdrawals,
            |cash_flow| cash_flow.date)?;

        self.sort_and_alter_fees(self.period.last_date());
        validator.validate("a fee", &self.fees, |fee| fee.date)?;

        if
            strictness.contains(ReadingStrictness::REPO_TRADES) &&
            self.cash_flows.iter().any(|cash_flow| matches!(cash_flow.type_, CashFlowType::Repo{..}))
        {
            warn!(concat!(
                "Broker statement contains repo trades which aren't supported yet. ",
                "All repo trades will be ignored during the calculations."
            ));
        }

        self.cash_flows.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        validator.validate("a cash flow", &self.cash_flows, |cash_flow| cash_flow.date)?;

        validator.sort_and_validate(
            "an idle cash interest", &mut self.idle_cash_interest, |interest| interest.date)?;

        validator.sort_and_validate(
            "a tax agent withholding", &mut self.tax_agent_withholdings,
            |withholding| withholding.date)?;

        validator.sort_and_validate(
            "a forex trade", &mut self.forex_trades, |trade| trade.conclusion_time)?;

        self.sort_and_validate_stock_buys()?;
        self.sort_and_validate_stock_sells()?;

        self.dividends.sort_by(|a, b| (a.date, &a.issuer).cmp(&(b.date, &b.original_issuer)));
        validator.validate("a dividend", &self.dividends, |dividend| dividend.date)?;

        validator.sort_and_validate(
            "a corporate action", &mut self.corporate_actions, |action| action.time)?;

        Ok(())
    }

    fn sort_and_alter_fees(&mut self, max_date: Date) {
        if self.broker.allow_future_fees {
            for fee in &mut self.fees {
                if fee.date > max_date && localities::is_valid_execution_date(max_date, fee.date) {
                    fee.date = max_date;
                }
            }
        }

        self.fees.sort_by_key(|fee| fee.date);
    }

    fn sort_and_validate_stock_buys(&mut self) -> EmptyResult {
        let date_validator = DateValidator::new(self.period);
        sort_and_validate_trades("buy", &mut self.stock_buys)?;
        date_validator.validate("a stock buy", &self.stock_buys, |trade| trade.conclusion_time)
    }

    fn sort_and_validate_stock_sells(&mut self) -> EmptyResult {
        let date_validator = DateValidator::new(self.period);
        sort_and_validate_trades("sell", &mut self.stock_sells)?;
        date_validator.validate("a stock sell", &self.stock_sells, |trade| trade.conclusion_time)
    }

    fn validate_open_positions(&self) -> EmptyResult {
        let mut open_positions: HashMap<&str, Decimal> = HashMap::new();

        for stock_buy in &self.stock_buys {
            if stock_buy.is_sold() {
                continue;
            }

            let multiplier = self.stock_splits.get_multiplier(
                &stock_buy.symbol, stock_buy.conclusion_time,
                DateOptTime::new_max_time(self.period.last_date()));

            let quantity = multiplier * stock_buy.get_unsold();

            open_positions.entry(&stock_buy.symbol)
                .and_modify(|position| *position += quantity)
                .or_insert(quantity);
        }

        let symbols: BTreeSet<&str> = self.open_positions.keys().map(String::as_str)
            .chain(open_positions.keys().copied())
            .collect();

        for &symbol in &symbols {
            let calculated = open_positions.get(symbol);
            let actual = self.open_positions.get(symbol);

            if calculated != actual {
                let calculated = calculated.copied().unwrap_or_default();
                let actual = actual.copied().unwrap_or_default();

                return Err!(concat!(
                    "Calculated open positions don't match declared ones in the statement: ",
                    "{}: {} vs {}"
                ), symbol, calculated, actual);
            }
        }

        Ok(())
    }
}

pub struct NetAssets {
    pub cash: MultiCurrencyCashAccount,
    pub other: Option<Cash>, // Supported only for some brokers
}