//! Force cargo to re-run compilation when a migration file is added,
//! removed, or modified. `sqlx::migrate!` is a proc macro that reads the
//! migrations directory at compile time; cargo has no way to know that
//! without an explicit `rerun-if-changed` hint.

// Build scripts communicate with cargo via stdout; `println!("cargo:...")` is
// the correct mechanism, not a lint violation.
#[allow(clippy::print_stdout)]
fn main() {
    println!("cargo:rerun-if-changed=migrations_postgres");
}
