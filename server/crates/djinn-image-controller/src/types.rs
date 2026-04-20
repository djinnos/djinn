//! Typed state shared between the controller, its consumers, and the DB.
//!
//! `ProjectImageView` / `BuildStatus` live here so downstream consumers
//! (UI banner in PR 6, `KubernetesRuntime::prepare` in this PR) can convert
//! the raw `djinn_db::ProjectImage` row into a typed value once.

use djinn_db::{ProjectImage, ProjectImageStatus};

/// Typed reflection of [`djinn_db::ProjectImage`].
///
/// The DB crate keeps `image_status` as a raw string because migrations +
/// SQL literals don't round-trip through a Rust enum cleanly. This enum
/// is the controller-side representation and is one-to-one with the
/// migration-8 vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildStatus {
    /// No build is known — initial state from migration 8's column default.
    None,
    /// A build Job has been submitted and is running or queued.
    Building,
    /// A build succeeded; the tag / hash fields of the associated
    /// [`ProjectImageView`] are populated.
    Ready,
    /// The most recent build failed. The error message is surfaced to the
    /// UI banner verbatim.
    Failed(String),
}

impl BuildStatus {
    /// Render as the raw string stored in `projects.image_status`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => ProjectImageStatus::NONE,
            Self::Building => ProjectImageStatus::BUILDING,
            Self::Ready => ProjectImageStatus::READY,
            Self::Failed(_) => ProjectImageStatus::FAILED,
        }
    }

    /// Parse from the raw string stored in `projects.image_status`.
    ///
    /// Unknown values default to [`BuildStatus::None`] so a forward-compatible
    /// migration (e.g. adding `cancelled`) doesn't break the running
    /// controller — the value just reads back as `None` until the binary
    /// ships a matching variant.
    pub fn from_row(status: &str, last_error: Option<&str>) -> Self {
        match status {
            ProjectImageStatus::BUILDING => Self::Building,
            ProjectImageStatus::READY => Self::Ready,
            ProjectImageStatus::FAILED => {
                Self::Failed(last_error.unwrap_or_default().to_string())
            }
            _ => Self::None,
        }
    }
}

/// Typed reflection of a full migration-8 row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectImageView {
    pub tag: Option<String>,
    pub hash: Option<String>,
    pub status: BuildStatus,
}

impl ProjectImageView {
    /// Adapt a raw DB row into the typed view.
    pub fn from_row(row: &ProjectImage) -> Self {
        Self {
            tag: row.tag.clone(),
            hash: row.hash.clone(),
            status: BuildStatus::from_row(&row.status, row.last_error.as_deref()),
        }
    }
}

/// Input to [`crate::ImageController::enqueue`] internal bookkeeping.
///
/// Emitted by the controller once it has decided the project's current
/// devcontainer hash differs from the persisted one — carries the data the
/// Job builder needs without re-reading the mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    pub project_id: String,
    pub devcontainer_hash: String,
    pub image_tag: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_string() {
        for status in [
            BuildStatus::None,
            BuildStatus::Building,
            BuildStatus::Ready,
            BuildStatus::Failed("boom".into()),
        ] {
            let rendered = status.as_str();
            let parsed = BuildStatus::from_row(
                rendered,
                matches!(status, BuildStatus::Failed(_)).then_some("boom"),
            );
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn unknown_status_string_falls_back_to_none() {
        assert_eq!(BuildStatus::from_row("unheard-of", None), BuildStatus::None);
    }
}
