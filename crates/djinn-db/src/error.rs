use std::fmt;

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug)]
pub enum DbError {
    Sqlx(sqlx::Error),
    Json(serde_json::Error),
    InvalidData(String),
    Internal(String),
    InvalidTransition(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlx(err) => write!(f, "database error: {err}"),
            Self::Json(err) => write!(f, "json error: {err}"),
            Self::InvalidData(msg) => write!(f, "invalid data: {msg}"),
            Self::Internal(msg) => write!(f, "{msg}"),
            Self::InvalidTransition(msg) => write!(f, "invalid transition: {msg}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlx(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::InvalidData(_) | Self::Internal(_) | Self::InvalidTransition(_) => None,
        }
    }
}

impl From<sqlx::Error> for DbError {
    fn from(value: sqlx::Error) -> Self {
        match value {
            sqlx::Error::RowNotFound => Self::Internal("query returned no rows".to_owned()),
            other => Self::Sqlx(other),
        }
    }
}

impl From<serde_json::Error> for DbError {
    fn from(value: serde_json::Error) -> Self { Self::Json(value) }
}

impl From<djinn_core::error::Error> for DbError {
    fn from(value: djinn_core::error::Error) -> Self {
        match value {
            djinn_core::error::Error::Internal(msg) => Self::Internal(msg),
            djinn_core::error::Error::InvalidTransition(msg) => Self::InvalidTransition(msg),
        }
    }
}
