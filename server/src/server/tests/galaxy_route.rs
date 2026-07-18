//! Live-Postgres route regressions for the pinned galaxy artifact stream.
//!
//! These tests intentionally use the real Axum router and the persisted reader.
//! They consume body frames one at a time: collecting a body here would hide the
//! allocation shape the production route is required to preserve.

use std::sync::OnceLock;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use djinn_db::{
    CreateUserAuthSession, Database, DatabaseConnectConfig, PostgresDatabaseConfig,
    RepoGraphGenerationRepository, ReservedGalaxyArtifactChunk, ReservedGalaxyArtifactManifest,
    ReservedGraphPublication, SessionAuthRepository, UserRepository, generation_stream_pin_key,
    release_generation_stream_pin_exclusive, try_acquire_generation_stream_pin_exclusive,
};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::server::{self, AppState};

const PROJECT: &str = "galaxy-route-live-regression";
const ROUTE: &str = "/api/projects/galaxy-route-live-regression/code-graph/galaxy";

fn database_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn fresh() -> Option<(
    Database,
    RepoGraphGenerationRepository,
    axum::Router,
    String,
)> {
    let url = std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .ok()?;
    let db = Database::open_with_config(DatabaseConnectConfig::Postgres(PostgresDatabaseConfig {
        url,
    }))
    .expect("open live Postgres database");
    db.ensure_initialized()
        .await
        .expect("migrate live Postgres database");
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(PROJECT)
        .execute(db.pool())
        .await
        .expect("clear route fixture project");
    sqlx::query("INSERT INTO projects(id, name, github_owner, github_repo) VALUES ($1, 'galaxy route fixture', 'test-owner', 'test-repo')")
        .bind(PROJECT).execute(db.pool()).await.expect("insert route fixture project");
    let token = seed_admin(&db).await;
    let app = server::router(AppState::new(db.clone(), CancellationToken::new()), false);
    Some((
        db.clone(),
        RepoGraphGenerationRepository::new(db),
        app,
        token,
    ))
}

async fn seed_admin(db: &Database) -> String {
    let id_bytes = *uuid::Uuid::now_v7().as_bytes();
    let github_id =
        i64::from_be_bytes(id_bytes[8..16].try_into().expect("eight bytes")).unsigned_abs() as i64;
    let login = format!("galaxy-route-{}", uuid::Uuid::now_v7().simple());
    let user = UserRepository::new(db.clone())
        .upsert_from_github(github_id, &login, None, None)
        .await
        .expect("create route user");
    UserRepository::new(db.clone())
        .set_admin_status(&user.id, true)
        .await
        .expect("make route user admin");
    let token = format!("galaxy-route-session-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: &login,
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_galaxy_route_test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .expect("create route session");
    token
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

async fn publish(
    repo: &RepoGraphGenerationRepository,
    commit: &str,
    chunks: Vec<Vec<u8>>,
) -> (String, String) {
    let generation_id = uuid::Uuid::now_v7().to_string();
    let artifact_id = uuid::Uuid::now_v7().to_string();
    let hashes: Vec<_> = chunks.iter().map(|chunk| digest(chunk)).collect();
    let mut transport = Sha256::new();
    for chunk in &chunks {
        transport.update(chunk);
    }
    let byte_count = chunks.iter().map(|chunk| chunk.len() as i64).sum();
    repo.publish_reserved_generation(ReservedGraphPublication {
        project_id: PROJECT.to_owned(),
        commit_sha: commit.to_owned(),
        generation_id: generation_id.clone(),
        graph_blob: b"route graph".to_vec(),
        artifact: ReservedGalaxyArtifactManifest {
            artifact_id: artifact_id.clone(),
            generation_id: generation_id.clone(),
            graph_content_hash: digest(format!("semantic-{commit}").as_bytes()),
            transport_sha256: format!("{:x}", transport.finalize()),
            chunk_count: i32::try_from(chunks.len()).expect("chunk count"),
            byte_count,
            chunk_hashes: hashes.clone(),
        },
        chunks: chunks
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| ReservedGalaxyArtifactChunk {
                generation_id: generation_id.clone(),
                artifact_id: artifact_id.clone(),
                chunk_index: index as i32,
                sha256: hashes[index].clone(),
                bytes,
            })
            .collect(),
    })
    .await
    .expect("publish fixture artifact");
    (generation_id, artifact_id)
}

async fn get(app: &axum::Router, token: &str, etag: Option<&str>) -> axum::response::Response {
    let mut request = Request::builder()
        .uri(ROUTE)
        .header("cookie", format!("djinn_session={token}"));
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("route response")
}

async fn consume_incrementally(mut body: Body) -> Result<Vec<u8>, axum::Error> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame?.into_data() {
            bytes.extend_from_slice(&data);
        }
    }
    Ok(bytes)
}

#[tokio::test]
async fn galaxy_route_returns_200_identity_headers_and_304() {
    let _serial = database_lock().lock().await;
    let Some((_, repo, app, token)) = fresh().await else {
        return;
    };
    let (generation, _) = publish(&repo, "g1", vec![b"one".to_vec(), b"two".to_vec()]).await;
    let response = get(&app, &token, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-djinn-generation-id"], generation);
    let etag = response.headers()["etag"]
        .to_str()
        .expect("etag")
        .to_owned();
    assert_eq!(
        consume_incrementally(response.into_body())
            .await
            .expect("stream succeeds"),
        b"onetwo"
    );
    let not_modified = get(&app, &token, Some(&etag)).await;
    assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
    let mut empty_body = not_modified.into_body();
    assert!(empty_body.frame().await.is_none(), "304 must have no body");
}

#[tokio::test]
async fn galaxy_route_machine_codes_unavailable_unsupported_and_preheader_corruption() {
    let _serial = database_lock().lock().await;
    let Some((db, repo, app, token)) = fresh().await else {
        return;
    };
    let unavailable = get(&app, &token, None).await;
    assert_eq!(unavailable.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        consume_incrementally(unavailable.into_body())
            .await
            .unwrap(),
        br#"{"code":"galaxy_artifact_unavailable"}"#
    );

    let (generation, artifact) = publish(&repo, "unsupported", vec![b"chunk".to_vec()]).await;
    sqlx::query(
        "UPDATE repo_graph_galaxy_artifact SET artifact_version = 2 WHERE artifact_id = $1::uuid",
    )
    .bind(&artifact)
    .execute(db.pool())
    .await
    .expect("set unsupported version");
    let unsupported = get(&app, &token, None).await;
    assert_eq!(unsupported.status(), StatusCode::CONFLICT);
    assert_eq!(
        consume_incrementally(unsupported.into_body())
            .await
            .unwrap(),
        br#"{"code":"galaxy_artifact_unsupported"}"#
    );

    sqlx::query("UPDATE repo_graph_galaxy_artifact SET artifact_version = 1, chunk_hashes = '[\"bad\"]'::jsonb WHERE artifact_id = $1::uuid")
        .bind(&artifact).execute(db.pool()).await.expect("make manifest corrupt");
    let corrupt = get(&app, &token, None).await;
    assert_eq!(corrupt.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        consume_incrementally(corrupt.into_body()).await.unwrap(),
        br#"{"code":"galaxy_artifact_corrupt"}"#
    );
    assert_eq!(
        generation_stream_pin_key(&generation).unwrap().class_id,
        djinn_db::GENERATION_STREAM_PIN_LOCK_CLASS
    );
}

#[tokio::test]
async fn galaxy_route_aborts_after_headers_when_a_later_chunk_is_corrupt() {
    let _serial = database_lock().lock().await;
    let Some((db, repo, app, token)) = fresh().await else {
        return;
    };
    let (generation, artifact) = publish(
        &repo,
        "post-header-corrupt",
        vec![b"good-first".to_vec(), b"must-not-arrive".to_vec()],
    )
    .await;
    let response = get(&app, &token, None).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "headers are committed first"
    );
    sqlx::query(
        "UPDATE repo_graph_galaxy_chunk SET sha256 = $1 \
         WHERE generation_id = $2::uuid AND artifact_id = $3::uuid AND chunk_index = 1",
    )
    .bind(digest(b"wrong"))
    .bind(&generation)
    .bind(&artifact)
    .execute(db.pool())
    .await
    .expect("corrupt second persisted chunk after headers");
    let mut body = response.into_body();
    let first = body
        .frame()
        .await
        .expect("first response frame")
        .expect("first frame is valid")
        .into_data()
        .expect("first frame contains data");
    assert_eq!(first, b"good-first".as_slice());
    let abort = body
        .frame()
        .await
        .expect("stream produces post-header abort frame");
    assert!(
        abort.is_err(),
        "corrupt later chunk must abort the established 200 stream"
    );
}

#[tokio::test]
async fn galaxy_route_artifactless_pointer_and_failed_publication_keep_prior_artifact() {
    let _serial = database_lock().lock().await;
    let Some((db, repo, app, token)) = fresh().await else {
        return;
    };
    let (_, artifact) = publish(&repo, "prior", vec![b"prior".to_vec()]).await;
    let etag = get(&app, &token, None).await.headers()["etag"]
        .to_str()
        .unwrap()
        .to_owned();
    // The legacy compatibility write advances the current pointer without an artifact.
    sqlx::query("INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) VALUES ($1, 'artifactless', $2, CURRENT_TIMESTAMP) ON CONFLICT (project_id, commit_sha) DO UPDATE SET graph_blob = EXCLUDED.graph_blob")
        .bind(PROJECT).bind(b"legacy graph".as_slice()).execute(db.pool()).await.expect("advance artifactless pointer");
    let unavailable = get(&app, &token, None).await;
    assert_eq!(unavailable.status(), StatusCode::NOT_FOUND);

    // Restore a valid current artifact, then force a transaction failure while attempting a new publication.
    sqlx::query("UPDATE repo_graph_current SET generation_id = (SELECT generation_id FROM repo_graph_galaxy_artifact WHERE artifact_id = $1::uuid) WHERE project_id = $2")
        .bind(&artifact).bind(PROJECT).execute(db.pool()).await.expect("restore prior pointer");
    let before = get(&app, &token, None).await;
    let prior_etag = before.headers()["etag"].to_str().unwrap().to_owned();
    drop(before);
    let failed = sqlx::query("INSERT INTO repo_graph_generation(generation_id, project_id, commit_sha, graph_blob, built_at, publish_seq, artifact_required) VALUES (gen_random_uuid(), $1, 'failed', $2, CURRENT_TIMESTAMP, 999999, true)")
        .bind(PROJECT).bind(b"failed".as_slice()).execute(db.pool()).await;
    assert!(
        failed.is_err(),
        "generation validation must reject incomplete publication"
    );
    let after = get(&app, &token, Some(&prior_etag)).await;
    assert_eq!(after.status(), StatusCode::NOT_MODIFIED);
    assert_ne!(
        etag, "",
        "prior ETag was observable before pointer advancement"
    );
}

#[tokio::test]
async fn galaxy_route_pins_g1_until_completion_and_cancellation_then_releases() {
    let _serial = database_lock().lock().await;
    let Some((db, repo, app, token)) = fresh().await else {
        return;
    };
    let (g1, _) = publish(&repo, "g1", vec![b"first-".to_vec(), b"last".to_vec()]).await;
    let response = get(&app, &token, None).await;
    let key = generation_stream_pin_key(&g1).expect("canonical g1 key");
    let mut contender = db.pool().acquire().await.expect("retention connection");
    publish(&repo, "g2", vec![b"new".to_vec()]).await;
    assert!(
        !try_acquire_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .unwrap(),
        "canonical retention try-lock must fail while G1 streams"
    );
    assert_eq!(
        consume_incrementally(response.into_body()).await.unwrap(),
        b"first-last"
    );
    assert!(
        try_acquire_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .unwrap(),
        "completion releases G1 pin"
    );
    assert!(
        release_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .unwrap()
    );

    // Start another G1 stream by repointing current, then cancel after its first frame.
    sqlx::query("UPDATE repo_graph_current SET generation_id = $1::uuid WHERE project_id = $2")
        .bind(&g1)
        .bind(PROJECT)
        .execute(db.pool())
        .await
        .unwrap();
    let cancelled = get(&app, &token, None).await;
    let mut body = cancelled.into_body();
    assert!(
        body.frame().await.unwrap().unwrap().into_data().is_ok(),
        "first G1 chunk arrives"
    );
    drop(body);
    tokio::task::yield_now().await;
    assert!(
        try_acquire_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .unwrap(),
        "client cancellation releases G1 pin"
    );
    assert!(
        release_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .unwrap()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn galaxy_route_600_chunk_request_stays_below_32_mib_rss_growth() {
    let _serial = database_lock().lock().await;
    let Some((_, repo, app, token)) = fresh().await else {
        return;
    };
    let chunks = (0..600)
        .map(|index| vec![(index % 251) as u8; 256 * 1024])
        .collect();
    publish(&repo, "rss", chunks).await;
    let baseline = rss_bytes();
    let response = get(&app, &token, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut frames = 0_usize;
    while let Some(frame) = body.frame().await {
        let data = frame.unwrap().into_data().expect("data frame");
        assert!(
            data.len() <= 256 * 1024,
            "route must emit one bounded stored chunk"
        );
        frames += 1;
    }
    assert_eq!(frames, 600);
    let growth = rss_bytes().saturating_sub(baseline);
    assert!(
        growth <= 32 * 1024 * 1024,
        "request RSS grew {} MiB (limit 32 MiB)",
        growth / 1024 / 1024
    );
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("read process statm");
    let resident_pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .expect("resident pages")
        .parse()
        .expect("numeric resident pages");
    resident_pages * 4096
}

#[test]
fn galaxy_route_allocation_shape_guard_rejects_payload_aggregation_regressions() {
    let route = include_str!("../galaxy.rs");
    let reader = include_str!("../../../crates/djinn-db/src/repositories/repo_graph_generation.rs");
    for forbidden in [
        "collect().await",
        "fetch_all",
        "Vec<RepoGraphGalaxyChunk>",
        "read_to_end",
        "GzDecoder",
    ] {
        assert!(
            !route.contains(forbidden) && !reader.contains(forbidden),
            "route/reader allocation guard rejected {forbidden}"
        );
    }
    assert!(
        reader.contains("chunk_index = $3"),
        "reader must retain indexed one-chunk query"
    );
    assert!(
        route.contains("for index in 0..chunk_count"),
        "route must stream in ordered bounded chunks"
    );
}
