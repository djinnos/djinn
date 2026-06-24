//! Retry utility for task transitions on locked database.
pub(super) fn is_database_locked(error: &djinn_db::Error) -> bool {
    match error {
        djinn_db::Error::Sqlx(sqlx_err) => sqlx_err
            .as_database_error()
            .and_then(|db_err| db_err.code())
            .map(|code| code == "40001" || code == "40P01")
            .unwrap_or(false),
        _ => false,
    }
}

pub(crate) async fn retry_task_transition_on_locked<F, Fut, T>(f: F) -> Result<T, djinn_db::Error>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, djinn_db::Error>>,
{
    const MAX_RETRIES: u32 = 3;
    let mut last_err = None;
    for _ in 0..MAX_RETRIES {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if is_database_locked(&e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap())
}
