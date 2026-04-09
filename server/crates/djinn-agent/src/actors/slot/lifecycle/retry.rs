pub(super) fn is_database_locked(error: &djinn_db::Error) -> bool {
    match error {
        djinn_db::Error::Sqlx(sqlx_err) => sqlx_err
            .as_database_error()
            .and_then(|db_err| db_err.code())
            .map(|code| matches!(code.as_ref(), "5" | "6" | "517"))
            .unwrap_or_else(|| {
                let msg = sqlx_err.to_string().to_ascii_lowercase();
                msg.contains("database is locked") || msg.contains("database table is locked")
            }),
        other => {
            let msg = other.to_string().to_ascii_lowercase();
            msg.contains("database is locked") || msg.contains("database table is locked")
        }
    }
}

pub(super) async fn retry_task_transition_on_locked<F, Fut, T>(
    mut op: F,
) -> Result<T, djinn_db::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, djinn_db::Error>>,
{
    const MAX_RETRIES: u32 = 5;
    const BASE_DELAY_MS: u64 = 200;

    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if is_database_locked(&err) && attempt < MAX_RETRIES => {
                attempt += 1;
                let delay = BASE_DELAY_MS * 2u64.pow(attempt - 1);
                tracing::debug!(
                    attempt,
                    delay_ms = delay,
                    "Lifecycle: database locked during task transition, retrying after backoff"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            Err(err) => return Err(err),
        }
    }
}
