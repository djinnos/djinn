#[macro_export]
macro_rules! ensure_db {
    ($db:expr) => {
        $db.ensure_initialized().await?;
    };
}
