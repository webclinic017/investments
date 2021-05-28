pub use chrono::NaiveDate as Date;
pub use chrono::NaiveTime as Time;
pub use chrono::NaiveDateTime as DateTime;

pub use rust_decimal::Decimal as Decimal;
pub const DECIMAL_PRECISION: u32 = 28;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TradeType {
    Buy,
    Sell,
}

macro_rules! date {
    ($year:expr, $month:expr, $day:expr) => (::chrono::NaiveDate::from_ymd($year, $month, $day))
}

#[cfg(test)]
macro_rules! date_time {
    ($year:expr, $month:expr, $day:expr, $hour:expr, $minute:expr, $second:expr) => {
        ::chrono::NaiveDateTime::new(
            date!($year, $month, $day),
            ::chrono::NaiveTime::from_hms($hour, $minute, $second),
        )
    }
}