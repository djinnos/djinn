//! Dev-mode ephemeral HTTP server for OAuth callback
//!
//! Runs on localhost:19876 to handle http://localhost:19876/auth/callback
//! for development mode. Custom protocols are unreliable in dev builds.

use std::sync::Arc;
use axum::{
    extract::Query,
    response::Html,
    routing::get,
    Router,
};
use serde::Deserialize;
use tauri::AppHandle;
use tokio::sync::mpsc;
use tokio::net::TcpListener;

use crate::auth_callback::{AuthCallbackManager, OAuthCallbackData, DEV_CALLBACK_PORT};

/// Query parameters for auth callback
#[derive(Debug, Deserialize)]
struct AuthCallbackQuery {
    code: String,
    state: String,
}

/// Error query parameters
#[derive(Debug, Deserialize)]
struct AuthErrorQuery {
    error: String,
    error_description: Option<String>,
}

/// Success HTML response
const SUCCESS_HTML: &str = "<!DOCTYPE html><html><head><title>Authentication Successful</title><style>body{font-family:sans-serif;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#f5f5f5;}.container{text-align:center;padding:2rem;background:white;border-radius:8px;box-shadow:0 2px 10px rgba(0,0,0,0.1);}h1{color:#4CAF50;}p{color:#666;}</style></head><body><div class=\"container\"><h1>Authentication Successful</h1><p>You can close this window and return to DjinnOS.</p></div></body></html>";

/// Spawn the ephemeral HTTP server for dev-mode OAuth callbacks
///
/// This server runs only in dev mode and handles the /auth/callback route
/// to receive the OAuth authorization code from Clerk.
pub async fn spawn_dev_server(
    app_handle: AppHandle,
    callback_manager: Arc<AuthCallbackManager>,
) -> Result<(), String> {
    // Create channel for communication between HTTP handler and main thread
    let (tx, mut rx) = mpsc::unbounded_channel::<OAuthCallbackData>();

    // Store sender in callback manager
    callback_manager.set_callback_sender(tx.clone());

    // Clone for the HTTP handler
    let manager = callback_manager.clone();
    let app_for_handler = app_handle.clone();

    // Build router
    let app = Router::new()
        .route("/auth/callback", get(move |query| handle_auth_callback(query, manager.clone(), app_for_handler.clone())))
        .route("/", get(|| async { "DjinnOS Dev Server" }));

    // Try to bind to the port
    let addr = format!("127.0.0.1:{}", DEV_CALLBACK_PORT);
    let listener = TcpListener::bind(&addr).await
        .map_err(|e| format!("Failed to bind dev server to {}: {}", addr, e))?;

    log::info!("Dev-mode OAuth callback server listening on {}", addr);

    // Spawn server in background
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("Dev server error: {}", e);
        }
    });

    // Spawn receiver task to handle callbacks
    let manager_for_receiver = callback_manager.clone();
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if let Err(e) = manager_for_receiver.process_callback(&app_handle, data) {
                log::error!("Failed to process callback: {}", e);
            }
        }
    });

    Ok(())
}

/// Handle the auth callback request
async fn handle_auth_callback(
    query: Query<AuthCallbackQuery>,
    manager: Arc<AuthCallbackManager>,
    app: AppHandle,
) -> Html<&'static str> {
    let data = OAuthCallbackData {
        code: query.code.clone(),
        state: query.state.clone(),
    };

    // Process the callback
    if let Err(e) = manager.process_callback(&app, data) {
        log::error!("Failed to process callback: {}", e);
        return Html("<h1>Authentication Error</h1><p>Failed to process callback. Please try again.</p>");
    }

    Html(SUCCESS_HTML)
}
