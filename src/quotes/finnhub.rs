use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
#[cfg(test)] use indoc::indoc;
use log::{debug, trace};
#[cfg(test)] use mockito::{self, Mock, mock};
use rayon::prelude::*;
use reqwest::Url;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::core::{GenericResult, EmptyResult};
use crate::currency::Cash;
use crate::exchanges::Exchange;
use crate::rate_limiter::RateLimiter;
use crate::util::{self, DecimalRestrictions};
use crate::types::Decimal;

use super::{QuotesMap, QuotesProvider};

pub struct Finnhub {
    token: String,
    client: Client,
    rate_limiter: RateLimiter,
}

impl Finnhub {
    pub fn new(token: &str) -> Finnhub {
        Finnhub {
            token: token.to_owned(),
            client: Client::new(),
            rate_limiter: RateLimiter::new()
                .with_limit(60 / 2, Duration::from_secs(60))
                .with_limit(30 / 2, Duration::from_secs(1)),
        }
    }

    fn get_quote(&self, symbol: &str) -> GenericResult<Option<Cash>> {
        #[derive(Deserialize)]
        struct Quote {
            #[serde(rename = "t")]
            day_start_time: Option<i64>,

            #[serde(rename = "c")]
            current_price: Option<Decimal>,
        }

        let (time, price) = match self.query::<Quote>("quote", symbol)? {
            Some(Quote{
                day_start_time: Some(time),
                current_price: Some(price),
            }) if !price.is_zero() => (time, price),
            _ => return Ok(None),
        };

        if is_outdated(time)? {
            let time = DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(time, 0), Utc);
            debug!("{}: Got outdated quotes: {}.", symbol, time);
            return Ok(None);
        }

        let price = util::validate_decimal(price, DecimalRestrictions::StrictlyPositive)
            .map_err(|_| format!("Got an invalid {} price: {:?}", symbol, price))?;

        // Profile API has too expensive rate limit weight, so try to avoid using it
        let currency = if symbol.contains('.') {
            #[derive(Deserialize)]
            struct Profile {
                currency: String,
            }

            let profile = match self.query::<Profile>("stock/profile2", symbol)? {
                Some(profile) => profile,
                None => return Ok(None),
            };

            profile.currency
        } else {
            s!("USD")
        };

        Ok(Some(Cash::new(&currency, price)))
    }

    fn query<T: DeserializeOwned>(&self, method: &str, symbol: &str) -> GenericResult<Option<T>> {
        #[cfg(not(test))] let base_url = "https://finnhub.io";
        #[cfg(test)] let base_url = mockito::server_url();

        let url = Url::parse_with_params(&format!("{}/api/v1/{}", base_url, method), &[
            ("symbol", symbol),
            ("token", self.token.as_ref()),
        ])?;

        let get = |url| -> GenericResult<Option<T>> {
            self.rate_limiter.wait(&format!("request to {}", url));

            trace!("Sending request to {}...", url);
            let response = self.client.get(url).send()?;
            trace!("Got response from {}.", url);

            if !response.status().is_success() {
                return Err!("Server returned an error: {}", response.status());
            }
            let reply = response.text()?;

            if reply.trim() == "Symbol not supported" {
                return Ok(None);
            }

            Ok(serde_json::from_str(&reply)?)
        };

        Ok(get(url.as_str()).map_err(|e| format!(
            "Failed to get quotes from {}: {}", url, e))?)
    }
}

impl QuotesProvider for Finnhub {
    fn name(&self) -> &'static str {
        "Finnhub"
    }

    fn supports_stocks(&self) -> Option<Exchange> {
        Some(Exchange::Us)
    }

    fn high_precision(&self) -> bool {
        true
    }

    fn get_quotes(&self, symbols: &[&str]) -> GenericResult<QuotesMap> {
        let quotes = Mutex::new(HashMap::new());

        if let Some(error) = symbols.par_iter().map(|&symbol| -> EmptyResult {
            if let Some(price) = self.get_quote(symbol)? {
                let mut quotes = quotes.lock().unwrap();
                quotes.insert(symbol.to_owned(), price);
            }
            Ok(())
        }).find_map_any(|result| match result {
            Err(error) => Some(error),
            Ok(()) => None,
        }) {
            return Err(error);
        }

        Ok(quotes.into_inner().unwrap())
    }
}

#[cfg(not(test))]
fn is_outdated(time: i64) -> GenericResult<bool> {
    let date_time = NaiveDateTime::from_timestamp_opt(time, 0).ok_or_else(|| format!(
        "Got an invalid UNIX time: {}", time))?;
    Ok(super::is_outdated_quote::<Utc>(DateTime::from_utc(date_time, Utc)))
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
fn is_outdated(time: i64) -> GenericResult<bool> {
    #![allow(clippy::unreadable_literal)]
    Ok(time < 1582295400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes() {
        let _bnd_profile_mock = mock_response("/api/v1/stock/profile2?symbol=BND&token=mock", indoc!(r#"
            {
                "country": "US",
                "currency": "USD",
                "exchange": "NASDAQ NMS - GLOBAL MARKET",
                "finnhubIndustry": "N/A",
                "ipo": "",
                "logo": "https://static.finnhub.io/logo/fad711b8-80e5-11ea-bacd-00000000092a.png",
                "marketCapitalization": 0,
                "name": "Vanguard Total Bond Market Index Fund",
                "phone": "",
                "shareOutstanding": 0,
                "ticker": "BND",
                "weburl": "http://www.vanguard.com/"
            }
        "#));
        let _bnd_quote_mock = mock_response("/api/v1/quote?symbol=BND&token=mock", indoc!(r#"
            {
                "c": 85.80000305175781,
                "h": 85.93000030517578,
                "l": 85.7300033569336,
                "o": 85.76000213623047,
                "pc": 85.58999633789062,
                "t": 1582295400
            }
        "#));

        let _outdated_profile_mock = mock_response("/api/v1/stock/profile2?symbol=AMZN&token=mock", indoc!(r#"
             {
                "country": "US",
                "currency": "USD",
                "exchange": "NASDAQ NMS - GLOBAL MARKET",
                "finnhubIndustry": "Retail",
                "ipo": "1997-05-01",
                "logo": "https://static.finnhub.io/logo/967bf7b0-80df-11ea-abb4-00000000092a.png",
                "marketCapitalization": 1220375,
                "name": "Amazon.com Inc",
                "phone": "12062661000",
                "shareOutstanding": 498.776032,
                "ticker": "AMZN",
                "weburl": "http://www.amazon.com/"
            }
       "#));
        let _outdated_quote_mock = mock_response("/api/v1/quote?symbol=AMZN&token=mock", indoc!(r#"
            {
                "c": 2095.969970703125,
                "h": 2144.550048828125,
                "l": 2088,
                "o": 2142.14990234375,
                "pc": 2153.10009765625,
                "t": 1
            }
        "#));

        let _unknown_profile_mock = mock_response("/api/v1/stock/profile2?symbol=UNKNOWN&token=mock", "{}");
        let _unknown_quote_mock = mock_response("/api/v1/quote?symbol=UNKNOWN&token=mock", "{}");

        // Old response for unknown symbols
        let _unknown_old_1_profile_mock = mock_response("/api/v1/stock/profile2?symbol=UNKNOWN_OLD_1&token=mock", "{}");
        let _unknown_old_1_quote_mock = mock_response("/api/v1/quote?symbol=UNKNOWN_OLD_1&token=mock", indoc!(r#"
            {
                "c": 0,
                "h": 0,
                "l": 0,
                "o": 0,
                "pc": 0
            }
        "#));
        let _unknown_old_2_profile_mock = mock_response("/api/v1/stock/profile2?symbol=UNKNOWN_OLD_2&token=mock", "{}");
        let _unknown_old_2_quote_mock = mock_response("/api/v1/quote?symbol=UNKNOWN_OLD_2&token=mock", "Symbol not supported");

        let _fxrl_profile_mock = mock_response("/api/v1/stock/profile2?symbol=FXRL.ME&token=mock", indoc!(r#"
            {
                "country": "IE",
                "currency": "RUB",
                "exchange": "MOSCOW EXCHANGE",
                "finnhubIndustry": "N/A",
                "ipo": "",
                "logo": "",
                "marketCapitalization": 0,
                "name": "FinEx Russian RTS Equity UCITS ETF (USD)",
                "phone": "",
                "shareOutstanding": 0,
                "ticker": "FXRL.ME",
                "weburl": ""
            }
        "#));
        let _fxrl_quote_mock = mock_response("/api/v1/quote?symbol=FXRL.ME&token=mock", indoc!(r#"
            {
                "c": 2758.5,
                "h": 2796,
                "l": 2734,
                "o": 2796,
                "pc": 2764,
                "t": 1582295400
            }
        "#));

        let client = Finnhub::new("mock");

        let mut quotes = HashMap::new();
        quotes.insert(s!("BND"), Cash::new("USD", dec!(85.80000305175781)));
        quotes.insert(s!("FXRL.ME"), Cash::new("RUB", dec!(2758.5)));
        assert_eq!(client.get_quotes(&[
            "BND", "AMZN", "UNKNOWN", "UNKNOWN_OLD_1", "UNKNOWN_OLD_2", "FXRL.ME",
        ]).unwrap(), quotes);
    }

    fn mock_response(path: &str, data: &str) -> Mock {
        // All responses are always 200 OK, some of them are returned with application/json content
        // type, some - with text/plain even for JSON payload.
        mock("GET", path)
            .with_status(200)
            .with_body(data)
            .create()
    }
}