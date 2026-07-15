//! Reusable narrow seams for worker-owned Cargo warm-base operations.

pub mod cargo_incremental_prune;
pub mod cargo_target_seed;

#[cfg(test)]
mod tests {
    static SEED_TELEMETRY_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn seed_telemetry_guard() -> std::sync::MutexGuard<'static, ()> {
        SEED_TELEMETRY_MUTEX
            .lock()
            .expect("seed telemetry test mutex poisoned")
    }
}
