#![cfg(target_os = "linux")]

use std::process::{Command, Output};

const HELM_MALLOC_CONF: &str = "background_thread:true,dirty_decay_ms:10000,muzzy_decay_ms:10000";

fn allocator_settings_command(malloc_conf: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_djinn-server"))
        .arg("--allocator-settings")
        .env("MALLOC_CONF", malloc_conf)
        .output()
        .expect("djinn-server allocator diagnostic should start")
}

#[test]
fn malformed_malloc_conf_fails_at_the_binary_startup_boundary() {
    let output = allocator_settings_command("unsupported_option:true");

    assert!(
        !output.status.success(),
        "invalid MALLOC_CONF unexpectedly succeeded: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("invalid MALLOC_CONF: unsupported MALLOC_CONF key `unsupported_option`"),
        "unexpected allocator validation error: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn helm_default_reports_effective_jemalloc_settings_from_the_binary() {
    let output = allocator_settings_command(HELM_MALLOC_CONF);

    assert!(
        output.status.success(),
        "allocator diagnostic failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("diagnostic output should be UTF-8"),
        "background_thread=true\ndirty_decay_ms=10000\nmuzzy_decay_ms=10000\n"
    );
}
