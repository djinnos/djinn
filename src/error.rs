use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Internal(String),

    #[error("invalid transition: {0}")]
    InvalidTransition(String),
}

pub type Result<T> = std::result::Result<T, Error>;
