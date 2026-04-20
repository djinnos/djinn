//! Deliberately larger than the TS file so Rust wins the byte-share
//! race and becomes the primary language.
pub fn hello() -> String {
    "hello from the polyglot fixture — this line exists only to add bytes".into()
}
pub fn farewell() -> String {
    "goodbye from the polyglot fixture — and this one too".into()
}
pub fn maybe_primary() -> &'static str {
    "Rust wins"
}
