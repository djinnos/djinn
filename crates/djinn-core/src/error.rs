use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Internal(String),

    #[error("invalid transition: {0}")]
    InvalidTransition(String),
}

pub type Result<T> = std::result::Result<T, Error>;
