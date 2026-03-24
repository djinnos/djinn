use std::path::Path;

mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations");
}

/// Run migrations using refinery's built-in rusqlite runner.
///
/// Refinery handles checksum validation (rejects modified migrations) and
/// ordering enforcement (rejects out-of-order versions) automatically.
pub fn run(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = rusqlite::Connection::open(path)?;
    embedded::migrations::runner().run(&mut conn)?;
    Ok(())
}
