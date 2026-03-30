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

#[cfg(test)]
pub(crate) fn run_until(
    path: &Path,
    migration_name_exclusive: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = rusqlite::Connection::open(path)?;
    let runner = embedded::migrations::runner();
    let migrations = runner.get_migrations();
    let stop_version = migrations
        .iter()
        .find(|migration| migration.name() == migration_name_exclusive)
        .map(|migration| migration.version() - 1)
        .unwrap_or_else(|| {
            migrations
                .last()
                .map(|migration| migration.version())
                .unwrap_or(0)
        });

    runner
        .set_target(refinery::Target::Version(stop_version))
        .run(&mut conn)?;
    Ok(())
}
