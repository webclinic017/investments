use std::io::{BufWriter, Write};
use std::fs::{self, File};

use lazy_static::lazy_static;
use num_traits::ToPrimitive;
use prometheus::{self, TextEncoder, Encoder, GaugeVec, register_gauge_vec};

use crate::analyse::{CurrencyStatistics, analyse};
use crate::config::Config;
use crate::core::{EmptyResult, GenericError};
use crate::currency::converter::CurrencyConverter;
use crate::types::Decimal;

lazy_static! {
    // FIXME(konishchev): Check all
    // FIXME(konishchev): Update timestamp
    // FIXME(konishchev): Portfolio label

    static ref ASSETS: GaugeVec = register_instrument_metric(
        "assets", "Open positions value.");

    static ref PERFORMANCE: GaugeVec = register_instrument_metric(
        "performance", "Instrument performance.");

    static ref INCOME_STRUCTURE: GaugeVec = register_metric(
        "income_structure", "Income structure.", &["currency", "type"]);

    static ref EXPENCES_STRUCTURE: GaugeVec = register_metric(
        "expenses_structure", "Expenses structure.", &["currency", "type"]);

    static ref PROFIT: GaugeVec = register_currency_metric(
        "profit", "Profit.");

    static ref NET_PROFIT: GaugeVec = register_currency_metric(
        "net_profit", "Pure profit.");

    static ref EXPECTED_TAXES: GaugeVec = register_currency_metric(
        "expected_taxes", "Expected taxes to pay.");

    static ref EXPECTED_COMMISSIONS: GaugeVec = register_currency_metric(
        "expected_commissions", "Expected commissions to pay.");

    static ref FOREX_PAIRS: GaugeVec = register_metric(
        "forex", "Forex quotes.", &["base", "quote"]);
}

// FIXME(konishchev): Regression tests
pub fn collect(config: &Config, path: &str) -> EmptyResult {
    let (statistics, converter) = analyse(
        config, None, false, Some(&config.metrics.merge_performance), false)?;

    for statistics in statistics.currencies {
        collect_currency_metrics(&statistics);
    }

    collect_forex_quotes(&converter, "USD", "RUB")?;

    save(path)
}

fn collect_currency_metrics(statistics: &CurrencyStatistics) {
    let currency = &statistics.currency;

    for (instrument, &value) in &statistics.assets {
        set_instrument_metric(&ASSETS, currency, &instrument, value);
    }

    for (instrument, &interest) in &statistics.performance {
        set_instrument_metric(&PERFORMANCE, currency, &instrument, interest);
    }

    let profit = statistics.income_structure.net_profit + statistics.income_structure.commissions + statistics.income_structure.taxes;
    set_currency_metric(&PROFIT, currency, profit);
    set_currency_metric(&NET_PROFIT, currency, statistics.income_structure.net_profit);

    set_metric(&INCOME_STRUCTURE, &[currency, "trading-profit"], statistics.income_structure.trading_profit);
    set_metric(&INCOME_STRUCTURE, &[currency, "dividends"], statistics.income_structure.dividends);
    set_metric(&INCOME_STRUCTURE, &[currency, "interest"], statistics.income_structure.interest);
    set_metric(&INCOME_STRUCTURE, &[currency, "tax-deductions"], statistics.income_structure.tax_deductions);

    set_metric(&EXPENCES_STRUCTURE, &[currency, "commissions"], statistics.income_structure.commissions);
    set_metric(&EXPENCES_STRUCTURE, &[currency, "taxes"], statistics.income_structure.taxes);

    set_currency_metric(&EXPECTED_TAXES, currency, statistics.expected_taxes);
    set_currency_metric(&EXPECTED_COMMISSIONS, currency, statistics.expected_commissions);
}

fn collect_forex_quotes(converter: &CurrencyConverter, base: &str, quote: &str) -> EmptyResult {
    Ok(set_metric(&FOREX_PAIRS, &[base, quote], converter.real_time_currency_rate(base, quote)?))
}

fn save(path: &str) -> EmptyResult {
    let encoder = TextEncoder::new();
    let metrics = prometheus::gather();

    let temp_path = format!("{}.tmp", path);
    let mut file = BufWriter::new(File::create(&temp_path)?);

    encoder.encode(&metrics, &mut file)
        .map_err(Into::into)
        .and_then(|_| {
            Ok(file.flush()?)
        })
        .or_else(|err: GenericError| {
            fs::remove_file(&temp_path)?;
            Err(err)
        })?;

    Ok(fs::rename(&temp_path, path)?)
}

fn register_currency_metric(name: &str, help: &str) -> GaugeVec {
    register_metric(name, help, &["currency"])
}

fn register_instrument_metric(name: &str, help: &str) -> GaugeVec {
    register_metric(name, help, &["currency", "instrument"])
}

fn register_metric(name: &str, help: &str, labels: &[&str]) -> GaugeVec {
    register_gauge_vec!(&format!("investments_{}", name), help, labels).unwrap()
}

fn set_currency_metric(collector: &GaugeVec, currency: &str, value: Decimal) {
    set_metric(collector, &[currency], value)
}

fn set_instrument_metric(collector: &GaugeVec, currency: &str, instrument: &str, value: Decimal) {
    set_metric(collector, &[currency, instrument], value)
}

fn set_metric(collector: &GaugeVec, labels: &[&str], value: Decimal) {
    collector.with_label_values(labels).set(value.to_f64().unwrap())
}