//! Reply loop error handling.
pub(crate) fn classify_error(_error: &str) -> ErrorClass {
    ErrorClass::Transient
}

pub(crate) enum ErrorClass {
    Transient,
    Fatal,
    ProviderAuth,
}
