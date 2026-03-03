use rusqlite::Connection;

use crate::error::{Error, Result};

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}

/// Run all pending migrations against `conn`.
///
/// Called synchronously during `Database::open` and `Database::open_in_memory`,
/// before the connection is wrapped in its Mutex.
pub fn run(conn: &mut Connection) -> Result<()> {
    embedded::migrations::runner()
        .run(conn)
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::run;

    #[test]
    fn tables_exist_after_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();

        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
                )
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };

        assert!(tables.contains(&"settings".to_string()));
        assert!(tables.contains(&"projects".to_string()));
        assert!(tables.contains(&"epics".to_string()));
        assert!(tables.contains(&"tasks".to_string()));
        assert!(tables.contains(&"blockers".to_string()));
        assert!(tables.contains(&"activity_log".to_string()));
        assert!(tables.contains(&"notes".to_string()));
        assert!(tables.contains(&"note_links".to_string()));
    }
}
