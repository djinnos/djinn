use std::time::Duration;

use tokio_util::sync::CancellationToken;

use djinn_db::Database;

const INTERVAL: Duration = Duration::from_secs(30);

/// Spawn a background task that runs `PRAGMA wal_checkpoint(PASSIVE)` every 30s.
///
/// On cancellation, runs a final `PRAGMA wal_checkpoint(TRUNCATE)` before exiting
/// to leave the WAL file clean. Errors are logged but never crash the server.
pub fn spawn(db: Database, cancel: CancellationToken) {
    tokio::spawn(checkpoint_loop(db, cancel));
}

async fn checkpoint_loop(db: Database, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; consume it so we don't checkpoint on startup.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let res = async {
                    db.ensure_initialized().await?;
                    sqlx::query("PRAGMA wal_checkpoint(PASSIVE);")
                        .execute(db.pool())
                        .await?;
                    Ok::<(), crate::error::Error>(())
                }
                .await;
                if let Err(e) = res {
                    tracing::error!("WAL checkpoint (PASSIVE) failed: {e}");
                } else {
                    tracing::debug!("WAL checkpoint (PASSIVE) complete");
                }
            }
            () = cancel.cancelled() => {
                tracing::info!("WAL checkpoint task shutting down");
                let res = async {
                    db.ensure_initialized().await?;
                    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
                        .execute(db.pool())
                        .await?;
                    Ok::<(), crate::error::Error>(())
                }
                .await;
                if let Err(e) = res {
                    tracing::error!("WAL checkpoint (TRUNCATE) on shutdown failed: {e}");
                }
                break;
            }
        }
    }
}
