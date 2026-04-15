//! GitHub **App** integration — installation tokens + bot identity.
//!
//! This module is the proper counterpart to the legacy
//! [`crate::oauth::github_oauth_app_legacy`] (which implements the classic
//! OAuth App device-code flow). Here we mint App-level JWTs from the App's
//! private key, exchange them for installation access tokens, and expose a
//! thin client that wraps those tokens for all repo operations.
//!
//! Commits made through the resulting clone URLs are attributed to the App's
//! bot identity (`djinn-bot[bot]`).
//!
//! See `docs/GITHUB_APP_SETUP.md` for the one-time setup steps (creating the
//! App, copying the private key, filling in env vars).
//!
//! # Environment
//! - `GITHUB_APP_ID` — numeric App ID (required).
//! - `GITHUB_APP_SLUG` — slug used to build the install URL.
//! - `GITHUB_APP_PRIVATE_KEY` *or* `GITHUB_APP_PRIVATE_KEY_PATH` — RSA PEM.
//! - `GITHUB_APP_CLIENT_ID` / `GITHUB_APP_CLIENT_SECRET` — user-to-server OAuth.
//!
//! # Submodules
//! - [`jwt`] — mint App-level JWTs (`iss=app_id`, RS256).
//! - [`installations`] — list installations, exchange JWT for installation tokens.
//! - [`client`] — reqwest wrapper that injects installation bearer tokens.

pub mod client;
pub mod installations;
pub mod jwt;

pub use client::{GitHubAppClient, InstallationRepo, find_installation_for_repo, install_url};
pub use installations::{
    Installation, InstallationToken, get_installation_token, list_installations_for_user,
};
pub use jwt::{AppJwtError, app_id, mint_app_jwt, private_key_pem};

/// Env var: numeric GitHub App ID.
pub const ENV_APP_ID: &str = "GITHUB_APP_ID";
/// Env var: GitHub App slug (for `https://github.com/apps/<slug>/installations/new`).
pub const ENV_APP_SLUG: &str = "GITHUB_APP_SLUG";
/// Env var: RSA private key PEM (multi-line) for the GitHub App.
pub const ENV_PRIVATE_KEY: &str = "GITHUB_APP_PRIVATE_KEY";
/// Env var: path to the RSA private key PEM file.
pub const ENV_PRIVATE_KEY_PATH: &str = "GITHUB_APP_PRIVATE_KEY_PATH";
/// Env var: user-to-server OAuth client id for the GitHub App.
pub const ENV_CLIENT_ID: &str = "GITHUB_APP_CLIENT_ID";
/// Env var: user-to-server OAuth client secret for the GitHub App.
pub const ENV_CLIENT_SECRET: &str = "GITHUB_APP_CLIENT_SECRET";

/// Legacy env var aliases kept for one release. Consuming code should log
/// a deprecation warning when it falls back to these.
pub const LEGACY_ENV_CLIENT_ID: &str = "GITHUB_OAUTH_CLIENT_ID";
pub const LEGACY_ENV_CLIENT_SECRET: &str = "GITHUB_OAUTH_CLIENT_SECRET";
