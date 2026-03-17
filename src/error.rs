use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(sqlx::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Internal(String),

    #[error("invalid transition: {0}")]
    InvalidTransition(String),
}

impl From<sqlx::Error> for Error {
    fn from(value: sqlx::Error) -> Self {
        match value {
            sqlx::Error::RowNotFound => Self::Internal("query returned no rows".to_owned()),
            other => Self::Database(other),
        }
    }
}

impl From<djinn_db::Error> for Error {
    fn from(value: djinn_db::Error) -> Self {
        match value {
            djinn_db::Error::Sqlx(err) => Self::from(err),
            djinn_db::Error::Json(err) => Self::Internal(err.to_string()),
            djinn_db::Error::InvalidData(msg) => Self::Internal(msg),
        }
    }
}

impl From<djinn_core::error::Error> for Error {
    fn from(value: djinn_core::error::Error) -> Self {
        match value {
            djinn_core::error::Error::Internal(msg) => Self::Internal(msg),
            djinn_core::error::Error::InvalidTransition(msg) => Self::InvalidTransition(msg),
        }
    }
}

impl Error {
    pub fn is_database_locked(&self) -> bool {
        let Self::Database(err) = self else {
            return false;
        };

        if let Some(db_err) = err.as_database_error() {
            let code_match = db_err
                .code()
                .map(|code| matches!(code.as_ref(), "5" | "6"))
                .unwrap_or(false);
            if code_match {
                return true;
            }
        }

        // Fallback: check the error message for SQLite lock indicators.
        let msg = err.to_string().to_ascii_lowercase();
        msg.contains("database is locked") || msg.contains("database table is locked")
    }

    pub fn is_sqlx_constraint_violation(&self) -> bool {
        let Self::Database(err) = self else {
            return false;
        };

        let Some(db_err) = err.as_database_error() else {
            return false;
        };

        db_err
            .code()
            .map(|code| {
                let code = code.as_ref();
                matches!(code, "1555" | "2067" | "787" | "1299" | "275")
            })
            .unwrap_or_else(|| db_err.message().to_ascii_lowercase().contains("constraint"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
