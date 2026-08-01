//! Launcher-authority-protocol and digest-capture tests for
//! [`crate::watcher`].
//!
//! A sibling file behind the `#[path]` convention `run_dirs_tests.rs` and
//! `graph_warmer_tests.rs` already use, for two reasons: it keeps
//! `watcher.rs` inside the repository's Rust source-size guard, and this
//! group touches no Kubernetes API types at all, so it stays outside the
//! `k8s` capability-boundary exception the watcher itself carries.
//!
//! Still a child module of `watcher`, so the private sentinel parsers and
//! the `classify_ready` decision are reachable directly.

use super::*;

/// Logs of a build that declared `protocol` and pushed `digest`.
fn build_logs(digest: Option<&str>, protocol: Option<&str>) -> String {
    let mut logs = String::from("waiting for buildkitd at tcp://buildkitd:1234 ...\n");
    logs.push_str("buildkitd reachable after 1 attempt(s)\n");
    logs.push_str("#12 exporting to image\n");
    if let Some(digest) = digest {
        logs.push_str(&format!("{DIGEST_SENTINEL}{digest}\n"));
    }
    if let Some(protocol) = protocol {
        logs.push_str(&format!("{PROTOCOL_SENTINEL}{protocol}\n"));
    }
    logs
}

#[test]
fn parse_protocol_sentinel_reads_the_declaration_and_treats_empty_as_undeclared() {
    for protocol in LauncherAuthorityProtocol::ALL {
        let metadata = parse_build_metadata(&build_logs(
            Some("sha256:deadbeefcafef00d"),
            Some(protocol.as_wire()),
        ));
        assert_eq!(metadata.protocol().unwrap(), Some(protocol));
    }

    // No sentinel at all, and a sentinel whose value never got set, are
    // both "declared nothing" — not "unknown protocol".
    assert_eq!(
        parse_build_metadata(&build_logs(Some("sha256:abc123"), None))
            .protocol()
            .unwrap(),
        None
    );
    assert_eq!(
        parse_build_metadata(&build_logs(Some("sha256:abc123"), Some("")))
            .protocol()
            .unwrap(),
        None
    );
}

#[test]
fn an_unrecognised_declaration_is_refused_rather_than_defaulted() {
    for bogus in ["resize-v3", "LEAF-V1", "leafv1", "true"] {
        let metadata = parse_build_metadata(&build_logs(Some("sha256:abc123"), Some(bogus)));
        assert!(metadata.protocol().is_err(), "{bogus} must not parse");
        let ReadyOutcome::Refuse(reason) = classify_ready("i1", "job", &metadata) else {
            panic!("{bogus} must not be marked ready under a guessed protocol");
        };
        assert!(reason.contains(bogus), "the refusal must name {bogus}");
    }
}

/// A build that declared nothing keeps the pre-existing behavior: ready on
/// the content-addressed tag, digest or not.
#[test]
fn an_undeclared_build_still_goes_ready_without_a_digest() {
    let metadata = parse_build_metadata(&build_logs(None, None));
    assert_eq!(
        classify_ready("legacy", "djinn-build-legacy-abc", &metadata),
        ReadyOutcome::Ready {
            digest: None,
            protocol: None
        },
    );
}

/// **AC2.** The declaration comes from the build's own metadata. The tag is
/// not an input to the decision — and this fixture's tag is a deliberate
/// lie: the image id, the repository segment, and the tag suffix all read
/// `resize-v2` while the artifact declares `leaf-v1`.
#[tokio::test]
async fn the_declared_protocol_comes_from_build_metadata_not_a_misleading_tag() {
    let db = Database::open_in_memory().unwrap();
    let images = ImageRepository::new(db.clone());
    images
        .create("resize-v2", "Misleading", None, "{}")
        .await
        .unwrap();

    // → reg:5000/djinn-image-resize-v2:resize-v2 — the repository segment
    // and the tag suffix both claim a protocol the artifact does not speak.
    let image_tag = format_catalog_image_tag("reg:5000", "resize-v2", "resize-v2");
    assert_eq!(image_tag, "reg:5000/djinn-image-resize-v2:resize-v2");

    let metadata = parse_build_metadata(&build_logs(
        Some("sha256:deadbeefcafef00d"),
        Some(LauncherAuthorityProtocol::LeafV1.as_wire()),
    ));
    let ReadyOutcome::Ready { digest, protocol } =
        classify_ready("resize-v2", "djinn-build-resize-v2-resize-v2", &metadata)
    else {
        panic!("a declaring build with a digest must be marked ready");
    };

    images
        .mark_ready("resize-v2", &image_tag, digest.as_deref(), protocol)
        .await
        .unwrap();

    let row = images.get("resize-v2").await.unwrap().expect("row");
    assert!(row.tag.as_deref().unwrap().contains("resize-v2"));
    assert_eq!(
        row.effective_launcher_protocol().unwrap(),
        LauncherAuthorityProtocol::LeafV1,
        "the persisted protocol must be the one the artifact declared, not the one its \
         name claims — deriving it from the tag records resize-v2 here"
    );
}

/// **AC1.** A build that succeeded but emitted no `sha256:` sentinel, from
/// an image that declares a protocol, is left out of `ready` with a
/// specific diagnostic, and the project it backs cannot dispatch.
///
/// Restoring the old `mark_ready(.., None)` behavior — i.e. making
/// [`classify_ready`] return `Ready` for a declaring build with no digest —
/// fails at the `else` below, and the `assert_ne!` on the persisted status
/// fails too.
#[tokio::test]
async fn a_declaring_build_with_no_digest_is_left_not_ready_and_non_dispatchable() {
    let db = Database::open_in_memory().unwrap();
    let projects = ProjectRepository::new(db.clone(), EventBus::noop());
    projects
        .create_with_id("p1", "p-p1", "test", "p1")
        .await
        .unwrap();
    let images = ImageRepository::new(db.clone());
    images.create("i1", "Rust", None, "{}").await.unwrap();
    images.set_project_image("p1", Some("i1")).await.unwrap();

    // The build pushed the image but the metadata file carried no
    // `containerimage.digest`, so the sentinel came out empty.
    let metadata = parse_build_metadata(&build_logs(
        None,
        Some(LauncherAuthorityProtocol::LeafV1.as_wire()),
    ));
    let ReadyOutcome::Refuse(last_error) =
        classify_ready("i1", "djinn-build-i1-abc123def456", &metadata)
    else {
        panic!(
            "a protocol-declaring build with no digest must be refused — marking it ready \
             produces a row whose Pod can never capture resize identity (migration 164)"
        );
    };
    assert!(last_error.contains(DIGEST_SENTINEL), "{last_error}");
    assert!(last_error.contains("leaf-v1"), "{last_error}");
    assert!(last_error.contains("migration 164"), "{last_error}");

    images.mark_failed("i1", &last_error).await.unwrap();

    let row = images.get("i1").await.unwrap().expect("row");
    assert_ne!(
        row.status,
        djinn_db::ImageStatus::READY,
        "a digestless protocol-declaring image must not be ready"
    );
    assert_eq!(row.last_error.as_deref(), Some(last_error.as_str()));

    let dispatch = projects
        .resolve_dispatch_image("p1")
        .await
        .unwrap()
        .expect("project resolves");
    assert!(
        dispatch.pull_ref().is_none(),
        "a refused image must report as non-dispatchable, got {:?}",
        dispatch.pull_ref()
    );
}

/// The happy path stays intact: a declaring build that did capture a digest
/// goes ready and dispatches on the digest-pinned ref.
#[tokio::test]
async fn a_declaring_build_with_a_digest_goes_ready_and_dispatches_pinned() {
    let db = Database::open_in_memory().unwrap();
    let projects = ProjectRepository::new(db.clone(), EventBus::noop());
    projects
        .create_with_id("p1", "p-p1", "test", "p1")
        .await
        .unwrap();
    let images = ImageRepository::new(db.clone());
    images.create("i1", "Rust", None, "{}").await.unwrap();
    images.set_project_image("p1", Some("i1")).await.unwrap();

    let metadata = parse_build_metadata(&build_logs(
        Some("sha256:abc123"),
        Some(LauncherAuthorityProtocol::ResizeV2.as_wire()),
    ));
    let ReadyOutcome::Ready { digest, protocol } = classify_ready("i1", "job", &metadata) else {
        panic!("a declaring build with a digest must be marked ready");
    };
    images
        .mark_ready("i1", "reg/djinn-image-i1:hash", digest.as_deref(), protocol)
        .await
        .unwrap();

    let dispatch = projects
        .resolve_dispatch_image("p1")
        .await
        .unwrap()
        .expect("project resolves");
    assert_eq!(
        dispatch.pull_ref().as_deref(),
        Some("reg/djinn-image-i1@sha256:abc123")
    );
}

#[test]
fn image_catalog_event_ready_shape() {
    let ev = image_catalog_event(
        "ready",
        "img-1",
        Some("reg/djinn-image-img-1:abc123"),
        Some("abc123"),
        None,
        "djinn-build-img-img-1-abc123",
    );
    assert_eq!(ev.entity_type, "image");
    assert_eq!(ev.action, "ready");
    assert_eq!(ev.id.as_deref(), Some("img-1"));
    assert_eq!(
        ev.payload.get("image_tag").and_then(|v| v.as_str()),
        Some("reg/djinn-image-img-1:abc123")
    );
}

#[test]
fn image_status_event_build_failed_carries_last_error() {
    let envelope = image_status_event(
        "build_failed",
        "proj-xyz",
        None,
        Some("deadbeef"),
        Some(
            "build Job djinn-build-proj-xyz-deadbeef failed; see kubectl logs job/djinn-build-proj-xyz-deadbeef",
        ),
        None,
        "djinn-build-proj-xyz-deadbeef",
    );
    assert_eq!(envelope.action, "build_failed");
    assert!(
        envelope
            .payload
            .get("last_error")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("kubectl logs")
    );
}
