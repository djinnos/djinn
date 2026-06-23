//! Test utilities for djinn-lsp tests.

use std::path::PathBuf;

pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = test_tmp_base();
    std::fs::create_dir_all(&base).expect("create test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create test tempdir")
}

fn test_tmp_base() -> PathBuf {
    if let Ok(base) = std::env::var("CARGO_TARGET_TMPDIR") {
        let base = PathBuf::from(base).join("djinn-lsp");
        if base.is_relative() {
            std::env::current_dir().expect("current dir").join(base)
        } else {
            base
        }
    } else {
        std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("test-tmp")
    }
}
