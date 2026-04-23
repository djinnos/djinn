use std::fs;
use std::path::Path;

fn main() {
    // rust-embed (server/src/server/static_ui.rs) hard-errors at compile
    // time if `../ui/dist/` is missing. On a fresh clone that hasn't run
    // `pnpm --dir ui build` yet, the server binary would then refuse to
    // compile. Drop a placeholder so cargo build always succeeds; the real
    // Vite output will overwrite it on the next UI build.
    let dist = Path::new("../ui/dist");
    if !dist.exists() {
        let _ = fs::create_dir_all(dist);
    }
    let index = dist.join("index.html");
    if !index.exists() {
        let _ = fs::write(
            &index,
            "<!doctype html><title>djinn</title>\
             <p>UI not built. Run <code>pnpm --dir ui build</code> and \
             rebuild <code>djinn-server</code>.</p>",
        );
    }
}
