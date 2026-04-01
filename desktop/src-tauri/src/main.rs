// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Work around WebKitGTK EGL/compositing crashes on Linux.
    // WEBKIT_DISABLE_DMABUF_RENDERER alone is insufficient — WebKitGTK still
    // falls back to hardware-accelerated compositing via EGL, which can crash
    // with "EGL_BAD_PARAMETER" on Wayland, NVIDIA, or AppImage environments.
    // WEBKIT_DISABLE_COMPOSITING_MODE disables that path entirely.
    // See: WebKit bugs #202362, #238513; tauri-apps/tauri#9394, #11988.
    #[cfg(target_os = "linux")]
    {
        // Safety: called before any threads are spawned (single-threaded main).
        unsafe {
            if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
                std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            }
            if std::env::var("WEBKIT_DISABLE_COMPOSITING_MODE").is_err() {
                std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            }
        }
    }

    djinnos_desktop_lib::run()
}
