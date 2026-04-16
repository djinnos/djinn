//! Force cargo to re-run compilation when a migration file is added,
//! removed, or modified. `sqlx::migrate!` is a proc macro that reads the
//! migrations directory at compile time; cargo has no way to know that
//! without an explicit `rerun-if-changed` hint.
fn main() {
    println!("cargo:rerun-if-changed=migrations_mysql");
}
