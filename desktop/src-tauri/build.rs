fn main() {
    // Create a placeholder sidecar binary when the real one is absent so that
    // `cargo clippy` and `cargo check` succeed in CI and agent worktrees where
    // the full sidecar hasn't been compiled.  The real binary is produced by
    // `scripts/sync-server-sidecar.sh` and only needed for `tauri dev/build`.
    let target_triple = std::env::var("TARGET").unwrap_or_default();
    if !target_triple.is_empty() {
        let bin_dir = std::path::Path::new("binaries");
        let sidecar = bin_dir.join(format!("djinn-server-{target_triple}"));
        if !sidecar.exists() {
            let _ = std::fs::create_dir_all(bin_dir);
            let _ = std::fs::write(&sidecar, "placeholder");
        }
    }

    tauri_build::build()
}
