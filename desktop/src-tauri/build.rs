fn main() {
    // Make the target triple available at compile time for platform detection
    // when downloading the server binary.
    let target_triple = std::env::var("TARGET").unwrap_or_default();
    if !target_triple.is_empty() {
        println!("cargo:rustc-env=DJINN_TARGET_TRIPLE={target_triple}");
    }

    tauri_build::build()
}
