//! rust-only fixture — non-trivial body so the byte total is stable.
pub fn greeting() -> &'static str {
    "hello from the rust-only fixture"
}
