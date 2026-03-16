use std::fmt;

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug)]
pub enum DbError {
    Sqlx(sqlx::Error),
    Json(serde_json::Error),
    InvalidData(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlx(err) => write!(f, "database error: {err}"),
            Self::Json(err) => write!(f, "json error: {err}"),
            Self::InvalidData(msg) => write!(f, "invalid data: {msg}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlx(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::InvalidData(_) => None,
        }
    }
}

impl From<sqlx::Error> for DbError {
    fn from(value: sqlx::Error) -> Self { Self::Sqlx(value) }
}

impl From<serde_json::Error> for DbError {
    fn from(value: serde_json::Error) -> Self { Self::Json(value) }
}
