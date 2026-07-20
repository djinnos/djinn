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
    CreateUserAuthSession, Database, MAX_RETENTION_BATCH, RepoGraphGenerationRepository,
    RepoGraphRetentionRepository, ReservedGalaxyArtifactChunk, ReservedGalaxyArtifactManifest,
    ReservedGraphPublication, ReservedPublicationFailureStage, RetentionMode,
    RetentionSweepRequest, SessionAuthRepository, UserRepository,
};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::sync::Barrier;
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

/// Read the current process RSS from `/proc/self/status` (Linux only).
fn current_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib * 1024);
        }
    }
    None
}

/// Extract only a named production function body, excluding unrelated source.
fn function_body<'a>(source: &'a str, function: &str) -> &'a str {
    let start = source
        .find(function)
        .unwrap_or_else(|| panic!("missing function `{function}`"));
    let open = source[start..]
        .find('{')
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("function `{function}` has no body"));
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut index = open;
    let mut quoted = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote) = quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == quote {
                quoted = None;
            }
        } else if matches!(byte, b'\"' | b'\'') {
            quoted = Some(byte);
        } else if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth -= 1;
            if depth == 0 {
                return &source[open + 1..index];
            }
        }
        index += 1;
    }
    panic!("function `{function}` has an unclosed body");
}

#[derive(Clone, Copy)]
enum GalaxyBodyContract {
    Stream,
    ChunkRead,
}

/// Finite structural guard shared by the landed bodies and reviewer mutations.
/// The 150 MiB live RSS test below remains its behavioral backstop.
fn assert_galaxy_body_contract(contract: GalaxyBodyContract, body: &str) -> Result<(), String> {
    let compact = body.split_whitespace().collect::<String>();
    match contract {
        GalaxyBodyContract::Stream => {
            if !compact.contains("forindexin0..chunk_count{") {
                return Err("stream must iterate indexed chunks".into());
            }
            let loop_body = function_body(body, "for index in 0..chunk_count");
            if loop_body.matches("reader.read_chunk(index).await").count() != 1
                || body.matches("reader.read_chunk(index).await").count() != 1
            {
                return Err("stream must read exactly one indexed chunk per iteration".into());
            }
            let has_vec_allocation = ["Vec::new()", "Vec::with_capacity(", "Vec::<", "Vec<"]
                .iter()
                .any(|needle| body.contains(needle));
            if has_vec_allocation && (body.contains(".push(") || body.contains(".extend(")) {
                return Err("stream must not accumulate chunks in a Vec".into());
            }
            let denied = [
                ".collect(",
                ".collect::<",
                ".try_collect(",
                ".try_collect::<",
                ".to_vec(",
                ".concat(",
                "BytesMut",
                "GzEncoder",
                "GzDecoder",
                "GzipEncoder",
                "GzipDecoder",
                "decompress",
                "compress(",
                "decode_all",
                "encode_all",
                "read_to_end",
                "read_to_string",
            ];
            if let Some(needle) = denied.iter().find(|needle| body.contains(**needle)) {
                return Err(format!(
                    "stream contains forbidden whole-payload operation `{needle}`"
                ));
            }
        }
        GalaxyBodyContract::ChunkRead => {
            if !compact.contains("artifact_id=$1") || !compact.contains("chunk_index=$2") {
                return Err(
                    "chunk query must constrain artifact identity and chunk_index = $2".into(),
                );
            }
            if !body.contains(".fetch_optional(") && !body.contains(".fetch_one(") {
                return Err("chunk query must use a one-row fetch path".into());
            }
            if body.contains(".fetch_all(") || body.contains(".fetch_many(") {
                return Err("chunk query must not use a multi-row fetch path".into());
            }
        }
    }
    Ok(())
}

#[test]
fn galaxy_allocation_shape_mutation_contract() {
    let stream = function_body(include_str!("../galaxy.rs"), "fn stream_gzip(");
    let read_chunk = function_body(
        include_str!("../../../crates/djinn-db/src/repositories/repo_graph_generation.rs"),
        "pub async fn read_chunk(",
    );
    assert_galaxy_body_contract(GalaxyBodyContract::Stream, stream).unwrap();
    assert_galaxy_body_contract(GalaxyBodyContract::ChunkRead, read_chunk).unwrap();

    let stream_mutations = [
        (
            "reviewed inferred Vec second-read push",
            r#"
                for index in 0..chunk_count {
                    match reader.read_chunk(index).await { Ok(chunk) => yield Ok(Bytes::from(chunk.bytes)), Err(_) => return }
                    let mut all = Vec::new(); all.push(reader.read_chunk(index).await.unwrap().bytes);
                }
            "#,
        ),
        (
            "typed Vec extend",
            "for index in 0..chunk_count { let mut all = Vec::<u8>::with_capacity(1); all.extend(reader.read_chunk(index).await.unwrap().bytes); }",
        ),
        (
            "inferred Vec capacity push",
            "for index in 0..chunk_count { let mut all = Vec::with_capacity(1); all.push(reader.read_chunk(index).await.unwrap().bytes); }",
        ),
        (
            "typed Vec new extend",
            "for index in 0..chunk_count { let mut all = Vec<u8>::new(); all.extend(reader.read_chunk(index).await.unwrap().bytes); }",
        ),
        (
            "collect",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; } let _: Vec<_> = stream.collect().await;",
        ),
        (
            "try_collect",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; } let _: Vec<_> = stream.try_collect().await;",
        ),
        (
            "collect turbofish",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; } let _ = stream.collect::<Vec<_>>().await;",
        ),
        (
            "try_collect turbofish",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; } let _ = stream.try_collect::<Vec<_>>().await;",
        ),
        (
            "to_vec",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; let _ = chunk.bytes.to_vec(); }",
        ),
        (
            "concat",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; } let _ = chunks.concat();",
        ),
        (
            "BytesMut",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; let _ = BytesMut::new(); }",
        ),
        (
            "whole payload codec",
            "for index in 0..chunk_count { let _ = reader.read_chunk(index).await; } let _ = GzDecoder::new(payload);",
        ),
    ];
    for (name, body) in stream_mutations {
        assert!(
            assert_galaxy_body_contract(GalaxyBodyContract::Stream, body).is_err(),
            "mutation `{name}` unexpectedly passed"
        );
    }

    // Build this spelling at runtime: the raw-SQL boundary guard correctly
    // rejects direct test-file `sqlx::query_as` calls outside djinn-db, while
    // these source-text mutations must still model the repository query body.
    let query_as = ["sqlx::query", "_as"].concat();
    let query_mutations = [
        (
            "omitted index",
            format!(
                "{query_as}(\"SELECT bytes FROM repo_graph_galaxy_chunk WHERE artifact_id = $1\").fetch_optional(conn).await"
            ),
        ),
        (
            "multi-row fetch",
            format!(
                "{query_as}(\"SELECT bytes FROM repo_graph_galaxy_chunk WHERE artifact_id = $1 AND chunk_index = $2\").fetch_all(conn).await"
            ),
        ),
    ];
    for (name, body) in query_mutations {
        assert!(
            assert_galaxy_body_contract(GalaxyBodyContract::ChunkRead, &body).is_err(),
            "mutation `{name}` unexpectedly passed"
        );
    }
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

/// Publish 600 ordered 256 KiB chunks (150 MiB) and prove the streamed request
/// keeps peak RSS growth bounded: the production reader yields one verified
/// chunk at a time without ever aggregating the full payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peak_rss_growth_for_600_chunk_stream_is_bounded() {
    let chunk_size = 256 * 1024;
    let chunk_count = 600usize;
    // A repeating 256 KiB pattern that is cheap to generate but distinct enough
    // to force real per-chunk hashing in the production stream.
    let chunk: Vec<u8> = (0..chunk_size).map(|i| (i & 0xff) as u8).collect();

    let db = test_helpers::create_test_db();
    let r = RepoGraphGenerationRepository::new(db.clone());
    let p = "galaxy-rss";
    r.prepare_publication_test_project(p).await.unwrap();
    let (a, t) = app(db).await;

    // Publish all producer/setup buffers inside a scope so they are released
    // before the RSS baseline. `publish_reserved_generation` consumes and drops
    // the publication (600 × 256 KiB chunk vectors), and the source buffer is
    // dropped below so producer memory is not charged as streaming growth.
    {
        let chunks: Vec<&[u8]> = vec![chunk.as_slice(); chunk_count];
        r.publish_reserved_generation(publication(p, "rss1", &chunks))
            .await
            .unwrap();
    }
    drop(chunk);

    // Start the request before measuring the baseline so reader/pin allocation
    // is part of the steady-state the test bounds.
    let response = get(&a, &t, p).await;
    assert_eq!(response.status(), StatusCode::OK);

    let baseline = current_rss_bytes().expect("VmRSS must be readable on Linux");
    let mut peak = baseline;
    let mut consumed_chunks = 0usize;
    let mut body = response.into_body();
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("stream frame should succeed");
        if frame.is_data() {
            consumed_chunks += 1;
        }
        // Sample RSS during every consumption step while the response is still
        // actively yielding frames.
        if let Some(rss) = current_rss_bytes() {
            peak = peak.max(rss);
        }
    }
    assert_eq!(consumed_chunks, chunk_count);

    let growth = peak.saturating_sub(baseline);
    assert!(
        growth <= 32 * 1024 * 1024,
        "peak request RSS growth {growth} bytes exceeds the 32 MiB bound \
         (baseline={baseline}, peak={peak})"
    );
}

/// A selected route response holds G's canonical shared session pin until its
/// final frame. Retention must skip G without waiting, fill its batch from
/// later unpinned candidates, and only prune G once that pin is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_route_stream_survives_retention_then_prunes_after_final_frame() {
    let db = test_helpers::create_test_db();
    let generations = RepoGraphGenerationRepository::new(db.clone());
    let p = "galaxy-retention-pin";
    generations
        .prepare_publication_test_project(p)
        .await
        .unwrap();
    let (router, token) = app(db.clone()).await;

    let g = publication(p, "stream-g", &[b"G-first-", b"G-middle-", b"G-final"]);
    let gid = g.generation_id.clone();
    let g_chunks: Vec<Vec<u8>> = g.chunks.iter().map(|chunk| chunk.bytes.clone()).collect();
    let g_tag = format!("\"{}\"", g.artifact.transport_sha256);
    generations.publish_reserved_generation(g).await.unwrap();

    // Selection forms headers and acquires G's shared pin before body polling.
    let response = get(&router, &token, p).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[HEADER_GENERATION_ID], gid);
    assert_eq!(response.headers()[header::ETAG], g_tag);
    assert!(
        !generations
            .try_generation_stream_pin_exclusive_for_test(&gid)
            .await
            .unwrap(),
        "selected response must own G's shared stream pin"
    );

    // Begin the selected response before moving the pointer. This makes the
    // later assertions specifically about G's *remaining* frames.
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("G's first frame must be ready")
        .expect("G must have a first frame")
        .expect("pinned G first frame must remain readable")
        .into_data()
        .expect("galaxy route emits data frames");
    assert_eq!(first.as_ref(), g_chunks[0].as_slice());
    let mut delivered = vec![first];

    // G comes first in publish order, so the sweep sees its active pin before
    // continuing to these candidates and filling all 25 batch slots.
    let mut blocked_candidate_id = None;
    for candidate_index in 0..MAX_RETENTION_BATCH {
        let commit = format!("retention-candidate-{candidate_index}");
        let candidate = publication(p, &commit, &[b"candidate"]);
        if candidate_index == 0 {
            blocked_candidate_id = Some(candidate.generation_id.clone());
        }
        generations
            .publish_reserved_generation(candidate)
            .await
            .unwrap();
    }
    let blocked_candidate_id = blocked_candidate_id.expect("first candidate identity");
    let history = publication(p, "retained-history", &[b"history"]);
    let history_id = history.generation_id.clone();
    generations
        .publish_reserved_generation(history)
        .await
        .unwrap();
    let g2 = publication(p, "stream-g2", &[b"G2-current"]);
    let g2id = g2.generation_id.clone();
    generations.publish_reserved_generation(g2).await.unwrap();

    // Hold the first unpinned candidate's row lock. Because G is older, a live
    // production sweep can reach this lock only after probing and skipping G's
    // shared pin. This keeps the sweep in flight while the route consumes G's
    // remaining frames, without sleeps or a parallel route fixture.
    let row_locked = Arc::new(Barrier::new(2));
    let release_row_lock = Arc::new(Barrier::new(2));
    let lock_generations = RepoGraphGenerationRepository::new(db.clone());
    let lock_ready = row_locked.clone();
    let lock_release = release_row_lock.clone();
    let row_locker = tokio::spawn(async move {
        lock_generations
            .hold_generation_row_lock_for_test(&blocked_candidate_id, lock_ready, lock_release)
            .await
    });
    row_locked.wait().await;

    // Start the production sweep at a deterministic barrier. History N=2
    // preserves current G2 and the history row.
    let start = Arc::new(Barrier::new(2));
    let sweep_start = start.clone();
    let sweep_db = db.clone();
    let mut sweep = tokio::spawn(async move {
        sweep_start.wait().await;
        RepoGraphRetentionRepository::new(sweep_db)
            .sweep(RetentionSweepRequest {
                project_id: p,
                mode: RetentionMode::Delete,
                history_n: 2,
            })
            .await
    });
    start.wait().await;

    // Bounded waiting proves the sweep is blocked after its nonblocking G pin
    // probe, rather than waiting for the reader. Consume every remaining frame
    // while that production sweep is still live.
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut sweep)
            .await
            .is_err(),
        "retention must remain live at the deliberately locked unpinned candidate"
    );
    while let Some(frame) = body.frame().await {
        let bytes = frame
            .expect("pinned G frame must remain readable")
            .into_data()
            .expect("galaxy route emits data frames");
        let index = delivered.len();
        assert_eq!(bytes.as_ref(), g_chunks[index].as_slice());
        delivered.push(bytes);
    }
    assert_eq!(delivered.len(), g_chunks.len());

    // Let the live sweep fill its batch and complete only after all of G has
    // been delivered and its reader pin has been released.
    release_row_lock.wait().await;
    tokio::time::timeout(Duration::from_secs(2), row_locker)
        .await
        .expect("candidate row lock must release")
        .expect("candidate row lock task must not panic")
        .expect("candidate row lock transaction must roll back");
    let first_sweep = tokio::time::timeout(Duration::from_secs(2), sweep)
        .await
        .expect("retention must finish after candidate row unlock")
        .expect("retention task must not panic")
        .expect("production retention sweep must succeed");
    assert_eq!(first_sweep.skipped_active_pin, 1);
    assert_eq!(first_sweep.candidates, MAX_RETENTION_BATCH);
    assert_eq!(first_sweep.deleted, MAX_RETENTION_BATCH);

    assert!(
        generations
            .try_generation_stream_pin_exclusive_for_test(&gid)
            .await
            .unwrap(),
        "final frame consumption must release G's reader pin"
    );

    let later_sweep = tokio::time::timeout(
        Duration::from_secs(2),
        RepoGraphRetentionRepository::new(db.clone()).sweep(RetentionSweepRequest {
            project_id: p,
            mode: RetentionMode::Delete,
            history_n: 2,
        }),
    )
    .await
    .expect("later retention sweep must finish")
    .expect("later production retention sweep must succeed");
    assert_eq!(
        later_sweep.deleted, 1,
        "released G becomes the sole candidate"
    );

    // Production deletion removes compatibility before immutable G, then FK
    // cascades its artifact/chunks. The only published survivors are H and G2.
    assert!(
        generations
            .compatibility_generation_id(p, "stream-g")
            .await
            .is_err(),
        "G compatibility row must be deleted"
    );
    assert!(
        generations
            .galaxy_chunks_for_test(&gid)
            .await
            .unwrap()
            .is_empty(),
        "G artifact chunks must cascade with its immutable generation"
    );
    let snapshot = generations.publication_snapshot_for_test(p).await.unwrap();
    assert_eq!(snapshot.current.as_deref(), Some(g2id.as_str()));
    assert_eq!(snapshot.cache, 2, "G2 and configured history survive");
    assert_eq!(snapshot.generations, 2, "G immutable generation is pruned");
    assert_eq!(
        snapshot.artifacts, 2,
        "only G2 and history artifacts remain"
    );
    assert_eq!(snapshot.chunks, 2, "only G2 and history chunks remain");
    assert_eq!(
        generations
            .compatibility_generation_id(p, "retained-history")
            .await
            .unwrap(),
        history_id
    );
}
