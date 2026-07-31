//! Shared test-support helpers for `djinn-k8s`'s integration test binaries.
//!
//! This directory is a subdirectory of `tests/`, so cargo does NOT compile it
//! as its own test binary. Pull it in with `mod support;` from a file directly
//! under `tests/`.

use std::sync::Once;

/// Install the process-level rustls [`CryptoProvider`] that this test binary
/// needs before it constructs its first `kube::Client`.
///
/// # Why this is needed at all
///
/// rustls 0.23 will only pick a provider by itself when exactly one of its
/// `ring` / `aws_lc_rs` features is enabled. This workspace enables **both** on
/// `x86_64-unknown-linux-gnu`: `workspace-hack`'s `[dependencies]` block asks
/// for `rustls … features = ["ring", …]` and its
/// `[target.x86_64-unknown-linux-gnu.dependencies]` block asks for
/// `rustls … features = ["aws-lc-rs", "aws_lc_rs"]`, and cargo unions the two.
/// `CryptoProvider::get_default_or_install_from_crate_features()` therefore
/// finds an ambiguous build and panics with
///
/// ```text
/// Could not automatically determine the process-level CryptoProvider from Rustls crate features
/// ```
///
/// `kube::Client` construction is where that bites: it builds a TLS config
/// eagerly, for an `http://` API-server URL as readily as an `https://` one, so
/// there is no TLS-free route around it.
///
/// # Why production is not affected
///
/// `server/src/main.rs` calls `rustls::crypto::ring::default_provider()
/// .install_default()` before it builds the tokio runtime. An explicit install
/// short-circuits the crate-feature sniffing entirely, so the ambiguity above
/// is never consulted in the server binary. Test binaries have no such `main`,
/// which is the whole of the defect — nothing else installs one for them.
///
/// # Why this fails loudly
///
/// `install_default()` returns `Err` when a provider is already installed.
/// Swallowing that (`let _ = …`) would paper over a half-initialised process in
/// which some *other* provider — aws-lc-rs, say — is the live default and this
/// call silently did nothing, which is a far more confusing failure than the
/// one it replaces. So the install runs exactly once per process under a
/// [`Once`] and `expect`s its result: a second *install* is never attempted, an
/// install that loses a race against a foreign installer panics and names
/// itself, and a poisoned `Once` re-panics on every later call. Calling this
/// from every test in a binary is consequently both correct and cheap.
///
/// [`CryptoProvider`]: rustls::crypto::CryptoProvider
pub fn install_crypto_provider() {
    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect(
                "install the rustls `ring` CryptoProvider for this test binary: a provider was \
                 already installed by something else in this process, so it is half-initialised \
                 — find the other installer instead of ignoring this",
            );
    });

    assert!(
        rustls::crypto::CryptoProvider::get_default().is_some(),
        "no process-level rustls CryptoProvider after install_crypto_provider()",
    );
}
