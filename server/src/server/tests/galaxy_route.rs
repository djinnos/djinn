//! Live Postgres route contract for pinned galaxy artifacts.
use crate::{
    server::{
        self, AppState,
        galaxy::{
            ERROR_CORRUPT, ERROR_UNAVAILABLE, ERROR_UNSUPPORTED, HEADER_ARTIFACT_VERSION,
            HEADER_COMMIT_SHA, HEADER_GENERATION_ID, HEADER_PROJECT_ID, HEADER_SEMANTIC_HASH,
        },
    },
    test_helpers,
};
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use djinn_db::{
    CreateUserAuthSession, Database, RepoGraphGenerationRepository, ReservedGalaxyArtifactChunk,
    ReservedGalaxyArtifactManifest, ReservedGraphPublication, ReservedPublicationFailureStage,
    SessionAuthRepository, UserRepository,
};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
fn hex(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn publication(p: &str, c: &str, cs: &[&[u8]]) -> ReservedGraphPublication {
    let g = uuid::Uuid::now_v7().to_string();
    let a = uuid::Uuid::now_v7().to_string();
    let hs: Vec<_> = cs.iter().map(|b| hex(b)).collect();
    let all = cs.concat();
    ReservedGraphPublication {
        project_id: p.into(),
        commit_sha: c.into(),
        generation_id: g.clone(),
        graph_blob: b"graph".to_vec(),
        artifact: ReservedGalaxyArtifactManifest {
            artifact_id: a.clone(),
            generation_id: g.clone(),
            graph_content_hash: hex(b"semantic"),
            transport_sha256: hex(&all),
            chunk_count: cs.len() as i32,
            byte_count: all.len() as i64,
            chunk_hashes: hs.clone(),
        },
        chunks: cs
            .iter()
            .enumerate()
            .map(|(i, b)| ReservedGalaxyArtifactChunk {
                generation_id: g.clone(),
                artifact_id: a.clone(),
                chunk_index: i as i32,
                sha256: hs[i].clone(),
                bytes: b.to_vec(),
            })
            .collect(),
    }
}
async fn app(db: Database) -> (axum::Router, String) {
    let u = UserRepository::new(db.clone())
        .upsert_from_github(7, "galaxy-admin", None, None)
        .await
        .unwrap();
    UserRepository::new(db.clone())
        .set_admin_status(&u.id, true)
        .await
        .unwrap();
    let t = format!("g{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &t,
            user_fk: &u.id,
            github_login: "galaxy-admin",
            github_name: None,
            github_avatar_url: None,
            github_access_token: "test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    (
        server::router(AppState::new(db, CancellationToken::new()), false),
        t,
    )
}
async fn get(a: &axum::Router, t: &str, p: &str) -> axum::response::Response {
    a.clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/projects/{p}/code-graph/galaxy"))
                .header(header::COOKIE, format!("djinn_session={t}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn assert_error_code(response: axum::response::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        format!(r#"{{"code":"{code}"}}"#).as_bytes()
    );
}
async fn assert_pin_released_after_cancellation(r: &RepoGraphGenerationRepository, gid: &str) {
    for _ in 0..20 {
        if r.try_generation_stream_pin_exclusive_for_test(gid)
            .await
            .unwrap()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cancelled response did not release its generation stream pin");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_outcomes_rollback_and_pinning() {
    let db = test_helpers::create_test_db();
    let r = RepoGraphGenerationRepository::new(db.clone());
    let p = "galaxy-live";
    r.prepare_publication_test_project(p).await.unwrap();
    let (a, t) = app(db).await;
    assert_error_code(
        get(&a, &t, p).await,
        StatusCode::NOT_FOUND,
        ERROR_UNAVAILABLE,
    )
    .await;
    let g = publication(p, "g1", &[b"gzip-", b"bytes"]);
    let gid = g.generation_id.clone();
    let bytes = g
        .chunks
        .iter()
        .flat_map(|x| x.bytes.clone())
        .collect::<Vec<_>>();
    let tag = format!("\"{}\"", g.artifact.transport_sha256);
    let semantic_hash = g.artifact.graph_content_hash.clone();
    r.publish_reserved_generation(g).await.unwrap();
    let x = get(&a, &t, p).await;
    assert_eq!(x.status(), StatusCode::OK);
    assert_eq!(x.headers()[header::ETAG], tag);
    assert_eq!(x.headers()[header::CONTENT_TYPE], "application/gzip");
    assert!(x.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(x.headers()[HEADER_PROJECT_ID], p);
    assert_eq!(x.headers()[HEADER_GENERATION_ID], gid);
    assert_eq!(x.headers()[HEADER_COMMIT_SHA], "g1");
    assert_eq!(x.headers()[HEADER_ARTIFACT_VERSION], "1");
    assert_eq!(x.headers()[HEADER_SEMANTIC_HASH], semantic_hash);
    assert_eq!(
        x.into_body().collect().await.unwrap().to_bytes().as_ref(),
        bytes.as_slice()
    );
    let n = a
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/projects/{p}/code-graph/galaxy"))
                .header(header::COOKIE, format!("djinn_session={t}"))
                .header(header::IF_NONE_MATCH, &tag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(n.status(), StatusCode::NOT_MODIFIED);
    assert!(n.into_body().collect().await.unwrap().to_bytes().is_empty());
    let selected = get(&a, &t, p).await;
    assert!(
        !r.try_generation_stream_pin_exclusive_for_test(&gid)
            .await
            .unwrap()
    );
    let g2 = publication(p, "g2", &[b"new"]);
    let g2id = g2.generation_id.clone();
    r.publish_reserved_generation(g2).await.unwrap();
    assert_eq!(
        selected
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        bytes.as_slice()
    );
    assert!(
        r.try_generation_stream_pin_exclusive_for_test(&gid)
            .await
            .unwrap()
    );
    let cancelled = get(&a, &t, p).await;
    assert!(
        !r.try_generation_stream_pin_exclusive_for_test(&g2id)
            .await
            .unwrap()
    );
    drop(cancelled);
    assert_pin_released_after_cancellation(&r, &g2id).await;
    r.legacy_upsert_for_publication_test(p, "legacy", b"none")
        .await
        .unwrap();
    assert_error_code(
        get(&a, &t, p).await,
        StatusCode::NOT_FOUND,
        ERROR_UNAVAILABLE,
    )
    .await;
    r.publish_reserved_generation(publication(p, "g3", &[b"prior"]))
        .await
        .unwrap();
    let prior = get(&a, &t, p).await;
    let old = prior.headers()[header::ETAG].clone();
    assert_eq!(
        prior
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"prior"
    );
    assert!(
        r.publish_reserved_generation_with_failure(
            publication(p, "bad", &[b"a", b"b"]),
            ReservedPublicationFailureStage::FirstChunkInsert
        )
        .await
        .is_err()
    );
    let retained = get(&a, &t, p).await;
    assert_eq!(retained.headers()[header::ETAG], old);
    assert_eq!(
        retained
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"prior"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_and_corruption_boundaries() {
    let db = test_helpers::create_test_db();
    let r = RepoGraphGenerationRepository::new(db.clone());
    let p = "galaxy-corrupt";
    r.prepare_publication_test_project(p).await.unwrap();
    let (a, t) = app(db).await;
    r.publish_reserved_generation(publication(p, "c1", &[b"first", b"second"]))
        .await
        .unwrap();
    r.set_current_artifact_version_for_test(p, 2).await.unwrap();
    assert_error_code(
        get(&a, &t, p).await,
        StatusCode::CONFLICT,
        ERROR_UNSUPPORTED,
    )
    .await;
    r.set_current_artifact_version_for_test(p, 1).await.unwrap();
    r.corrupt_current_artifact_metadata_for_test(p)
        .await
        .unwrap();
    assert_error_code(
        get(&a, &t, p).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        ERROR_CORRUPT,
    )
    .await;
    r.publish_reserved_generation(publication(p, "c2", &[b"first", b"second"]))
        .await
        .unwrap();
    let x = get(&a, &t, p).await;
    assert_eq!(x.status(), StatusCode::OK);
    r.corrupt_current_artifact_chunk_for_test(p, 1)
        .await
        .unwrap();
    assert!(x.into_body().collect().await.is_err());
}
