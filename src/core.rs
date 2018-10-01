pub type EmptyResult = GenericResult<()>;
pub type GenericResult<T> = Result<T, GenericError>;
pub type GenericError = Box<::std::error::Error + Send + Sync>;

macro_rules! Err {
    ($($arg:tt)*) => (::std::result::Result::Err(format!($($arg)*).into()))
}
