//! ij6g: record build/controller-published catalog wrapper image digests.
//!
//! The three catalog wrapper images (Postgres/Redis/RabbitMQ) are built and
//! pushed by `server/docker/build-wrapper-images.sh`, which captures each
//! image's REAL immutable digest from the registry push and writes a manifest.
//! This module consumes that manifest and records the identities into
//! `service_presets`, so strict canonical verification can resolve
//! `{wrapper_image}@{digest}`.
//!
//! The manifest is the only supported channel for wrapper digests: fabricated or
//! mutable identities are rejected by [`ServicePresetRepository::set_wrapper_identity`]
//! before any database write, so a placeholder literal can never become
//! deployable.

use std::path::Path;

use djinn_db::{Database, ServicePresetRepository};
use serde::Deserialize;

/// One catalog wrapper image identity published by the build.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WrapperImageEntry {
    /// The `service_presets.id` this identity belongs to.
    pub preset_id: String,
    /// The wrapper repository reference (no digest suffix).
    pub wrapper_image: String,
    /// The immutable `sha256:<64 hex>` digest of the pushed wrapper image.
    pub image_digest: String,
}

/// The manifest `build-wrapper-images.sh` emits after pushing every wrapper.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WrapperImageManifest {
    pub entries: Vec<WrapperImageEntry>,
}

/// Outcome of a reconcile pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WrapperCatalogReconcileStats {
    /// Presets whose wrapper identity was recorded (row updated).
    pub recorded: Vec<String>,
    /// Manifest entries whose preset id matched no catalog row.
    pub unknown_presets: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WrapperCatalogError {
    #[error("read wrapper image manifest {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parse wrapper image manifest {path}: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("record wrapper identity for preset {preset_id}: {detail}")]
    Record { preset_id: String, detail: String },
}

/// Environment variable naming the wrapper image manifest path the deploy
/// pipeline writes. Unset ⇒ no wrapper digests to reconcile (strict resolution
/// stays fail-closed).
pub const WRAPPER_IMAGE_MANIFEST_ENV: &str = "DJINN_WRAPPER_IMAGE_MANIFEST";

/// Parse a manifest from JSON bytes.
pub fn parse_wrapper_image_manifest(json: &str) -> Result<WrapperImageManifest, serde_json::Error> {
    serde_json::from_str(json)
}

/// Load and parse the manifest at `path`.
pub fn load_wrapper_image_manifest(
    path: &Path,
) -> Result<WrapperImageManifest, WrapperCatalogError> {
    let raw = std::fs::read_to_string(path).map_err(|source| WrapperCatalogError::Read {
        path: path.display().to_string(),
        source,
    })?;
    parse_wrapper_image_manifest(&raw).map_err(|source| WrapperCatalogError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Record every manifest entry's wrapper identity into `service_presets`. Each
/// write is validated (digest-free wrapper ref + `sha256:` digest) before it
/// touches the database. An entry whose preset id is unknown is reported rather
/// than failing the whole pass, so a manifest can safely list presets a given
/// deployment has not seeded.
pub async fn reconcile_wrapper_catalog(
    db: &Database,
    manifest: &WrapperImageManifest,
) -> Result<WrapperCatalogReconcileStats, WrapperCatalogError> {
    let presets = ServicePresetRepository::new(db.clone());
    let mut stats = WrapperCatalogReconcileStats::default();
    for entry in &manifest.entries {
        let updated = presets
            .set_wrapper_identity(&entry.preset_id, &entry.wrapper_image, &entry.image_digest)
            .await
            .map_err(|error| WrapperCatalogError::Record {
                preset_id: entry.preset_id.clone(),
                detail: error.to_string(),
            })?;
        if updated == 0 {
            stats.unknown_presets.push(entry.preset_id.clone());
        } else {
            stats.recorded.push(entry.preset_id.clone());
        }
    }
    Ok(stats)
}

/// Load the manifest named by [`WRAPPER_IMAGE_MANIFEST_ENV`] (if set) and
/// reconcile it. Returns `Ok(None)` when the env var is unset — the controller
/// then leaves wrapper digests unpopulated and strict resolution stays
/// fail-closed.
pub async fn reconcile_wrapper_catalog_from_env(
    db: &Database,
) -> Result<Option<WrapperCatalogReconcileStats>, WrapperCatalogError> {
    let Some(path) = std::env::var_os(WRAPPER_IMAGE_MANIFEST_ENV) else {
        return Ok(None);
    };
    let manifest = load_wrapper_image_manifest(Path::new(&path))?;
    let stats = reconcile_wrapper_catalog(db, &manifest).await?;
    tracing::info!(
        recorded = stats.recorded.len(),
        unknown = stats.unknown_presets.len(),
        "wrapper_catalog: reconciled catalog wrapper image digests"
    );
    Ok(Some(stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_from_build_output() {
        let json = r#"{"entries":[
            {"preset_id":"preset-redis-7","wrapper_image":"ghcr.io/djinnos/djinn-redis-wrapper","image_digest":"sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"}
        ]}"#;
        let manifest = parse_wrapper_image_manifest(json).expect("parse");
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].preset_id, "preset-redis-7");
    }

    #[tokio::test]
    async fn reconcile_records_real_digests_and_reports_unknown() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let digest = "sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        let manifest = WrapperImageManifest {
            entries: vec![
                WrapperImageEntry {
                    preset_id: "preset-redis-7".into(),
                    wrapper_image: "ghcr.io/djinnos/djinn-redis-wrapper".into(),
                    image_digest: digest.into(),
                },
                WrapperImageEntry {
                    preset_id: "preset-does-not-exist".into(),
                    wrapper_image: "ghcr.io/djinnos/other".into(),
                    image_digest: digest.into(),
                },
            ],
        };
        let stats = reconcile_wrapper_catalog(&db, &manifest).await.unwrap();
        assert_eq!(stats.recorded, vec!["preset-redis-7"]);
        assert_eq!(stats.unknown_presets, vec!["preset-does-not-exist"]);

        let redis = ServicePresetRepository::new(db.clone())
            .get("preset-redis-7")
            .await
            .unwrap()
            .expect("preset");
        assert_eq!(redis.image_digest.as_deref(), Some(digest));
        assert_eq!(
            redis.wrapper_image.as_deref(),
            Some("ghcr.io/djinnos/djinn-redis-wrapper")
        );
    }

    #[tokio::test]
    async fn reconcile_rejects_a_fabricated_digest() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let manifest = WrapperImageManifest {
            entries: vec![WrapperImageEntry {
                preset_id: "preset-redis-7".into(),
                wrapper_image: "ghcr.io/djinnos/djinn-redis-wrapper".into(),
                image_digest: "not-a-digest".into(),
            }],
        };
        assert!(matches!(
            reconcile_wrapper_catalog(&db, &manifest).await,
            Err(WrapperCatalogError::Record { .. })
        ));
    }
}
