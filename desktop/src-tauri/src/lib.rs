use std::sync::Mutex;
use tauri::Manager;

mod auth;
mod commands;
mod connection_mode;
mod deploy;
mod server;
mod ssh_hosts;
mod ssh_tunnel;
mod token_refresh;
mod token_sync;
mod wsl;

pub use server::ServerState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = app
                .get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Mutex::new(server::init_server_state()))
        .setup(move |app| {
            // Show window immediately — the frontend drives server connection
            // and onboarding flow.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
            }

            // On macOS, prevent window close — exit the whole app instead.
            #[cfg(target_os = "macos")]
            {
                let window = app.get_webview_window("main").expect("no main window");
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        app_handle.exit(0);
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_github_login,
            commands::greet,
            commands::get_server_port,
            commands::get_server_url,
            commands::get_server_status,
            commands::retry_server_connection,
            commands::get_connection_mode,
            commands::set_connection_mode,
            commands::get_auth_token,
            commands::set_auth_token,
            commands::clear_auth_token,
            commands::get_refresh_token,
            commands::set_refresh_token,
            commands::clear_refresh_token,
            commands::auth_get_state,
            commands::auth_login,
            commands::auth_logout,
            commands::perform_token_refresh,
            commands::get_auth_state,
            commands::is_token_expired,
            commands::logout,
            commands::select_directory,
            commands::check_git_remote,
            commands::list_git_branches,
            commands::setup_git_remote,
            commands::sync_github_tokens,
            commands::get_ssh_hosts,
            commands::save_ssh_host,
            commands::remove_ssh_host,
            commands::test_ssh_connection,
            commands::get_tunnel_status,
            commands::deploy_server_to_host,
            commands::check_wsl_available,
            commands::download_server_binary,
            commands::has_saved_connection_mode,
            commands::attempt_silent_auth,
        ])
        .build(tauri::generate_context!())
        .expect("error building tauri application")
        .run(|_app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                ssh_tunnel::stop_active_tunnel();
            }
        });
}
