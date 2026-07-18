//! REST contract for serving the current pinned galaxy artifact.
//!
//! The handler deliberately delegates selection and per-chunk validation to the
//! database pinned reader.  It never derives a lock key or issues chunk queries
//! itself, so a response remains attached to precisely the selected generation.

use std::io;

use async_stream::stream;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::server::{AppState, authenticate};
use djinn_db::{
    PinnedGalaxyArtifact, PinnedGalaxyArtifactSelection, RepoGraphGenerationRepository,
};

pub const GALAXY_ROUTE: &str = "/api/projects/{project_id}/code-graph/galaxy";
pub const ERROR_UNAVAILABLE: &str = "galaxy_artifact_unavailable";
pub const ERROR_UNSUPPORTED: &str = "galaxy_artifact_unsupported";
pub const ERROR_CORRUPT: &str = "galaxy_artifact_corrupt";
pub const HEADER_PROJECT_ID: &str = "x-djinn-project-id";
pub const HEADER_GENERATION_ID: &str = "x-djinn-generation-id";
pub const HEADER_COMMIT_SHA: &str = "x-djinn-commit-sha";
pub const HEADER_ARTIFACT_VERSION: &str = "x-djinn-galaxy-artifact-version";
pub const HEADER_SEMANTIC_HASH: &str = "x-djinn-galaxy-semantic-sha256";

pub(super) fn router() -> Router<AppState> {
    Router::new().route(GALAXY_ROUTE, get(get_galaxy))
}

async fn get_galaxy(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Browser sessions are the client-facing graph-read boundary.  Graph data
    // is deployment-wide today, with admin users retaining the explicit
    // authorization capability used by project graph management.
    let user = match authenticate(&state, &headers).await {
        Ok(Some(user)) => user,
        Ok(None) => return error_response(StatusCode::UNAUTHORIZED, "authentication_required"),
        Err(error) => {
            tracing::error!(%error, "galaxy route authentication failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, ERROR_CORRUPT);
        }
    };
    if !user.is_admin {
        return error_response(StatusCode::FORBIDDEN, "graph_read_forbidden");
    }

    let repository = RepoGraphGenerationRepository::new(state.db().clone());
    let selection = match repository.pin_current_galaxy_artifact(&project_id).await {
        Ok(selection) => selection,
        Err(error) => {
            tracing::error!(%error, "galaxy route selection failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, ERROR_CORRUPT);
        }
    };
    match selection {
        PinnedGalaxyArtifactSelection::NoCurrentGeneration
        | PinnedGalaxyArtifactSelection::ArtifactUnavailable => {
            error_response(StatusCode::NOT_FOUND, ERROR_UNAVAILABLE)
        }
        PinnedGalaxyArtifactSelection::UnsupportedVersion { .. } => {
            error_response(StatusCode::CONFLICT, ERROR_UNSUPPORTED)
        }
        PinnedGalaxyArtifactSelection::CorruptMetadata { reason } => {
            tracing::warn!(%reason, "galaxy route rejected corrupt metadata");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, ERROR_CORRUPT)
        }
        PinnedGalaxyArtifactSelection::Pinned(reader) => pinned_response(reader, &headers).await,
    }
}

async fn pinned_response(reader: PinnedGalaxyArtifact, request_headers: &HeaderMap) -> Response {
    let metadata = reader.metadata().clone();
    let etag = format!("\"{}\"", metadata.transport_sha256);
    let response_headers = match identity_headers(&metadata, &etag) {
        Ok(headers) => headers,
        Err(()) => {
            // The reader is dropped here and closes its session if it was not
            // explicitly finished. Metadata should already be bounded by irxf;
            // this second validation protects Axum header construction.
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, ERROR_CORRUPT);
        }
    };
    if etag_matches(request_headers, &etag) {
        if let Err(error) = reader.finish().await {
            tracing::warn!(%error, "galaxy route failed to release 304 reader");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, ERROR_CORRUPT);
        }
        djinn_telemetry::galaxy_artifact_route::record_outcome("not_modified");
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }

    djinn_telemetry::galaxy_artifact_route::record_outcome("ok");
    let body = Body::from_stream(stream_gzip(reader));
    (StatusCode::OK, response_headers, body).into_response()
}

fn identity_headers(
    metadata: &djinn_db::PinnedGalaxyArtifactMetadata,
    etag: &str,
) -> Result<HeaderMap, ()> {
    let artifact_version = metadata.artifact_version.to_string();
    let values = [
        (header::CONTENT_TYPE, "application/json"),
        (header::CONTENT_ENCODING, "gzip"),
        (header::ETAG, etag),
        (
            HeaderName::from_static(HEADER_PROJECT_ID),
            metadata.project_id.as_str(),
        ),
        (
            HeaderName::from_static(HEADER_GENERATION_ID),
            metadata.generation_id.as_str(),
        ),
        (
            HeaderName::from_static(HEADER_COMMIT_SHA),
            metadata.commit_sha.as_str(),
        ),
        (
            HeaderName::from_static(HEADER_ARTIFACT_VERSION),
            artifact_version.as_str(),
        ),
        (
            HeaderName::from_static(HEADER_SEMANTIC_HASH),
            metadata.graph_content_hash.as_str(),
        ),
    ];
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        headers.insert(name, HeaderValue::from_str(value).map_err(|_| ())?);
    }
    Ok(headers)
}

fn etag_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some((expected_opaque_tag, _)) = parse_entity_tag(expected) else {
        return false;
    };

    // If-None-Match uses weak comparison for GET, so the weak marker is not
    // part of the comparison. Multiple header fields are equivalent to a
    // comma-separated list; a wildcard in any valid field matches the current
    // representation.
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| if_none_match_value_matches(value, expected_opaque_tag))
}

fn if_none_match_value_matches(value: &str, expected_opaque_tag: &str) -> bool {
    let value = trim_ows(value);
    if value == "*" {
        return true;
    }

    let mut remainder = value;
    let mut matched = false;
    loop {
        remainder = trim_ows_start(remainder);
        let (opaque_tag, rest) = match parse_entity_tag(remainder) {
            Some(tag) => tag,
            None => return false,
        };
        matched |= opaque_tag == expected_opaque_tag;

        remainder = trim_ows_start(rest);
        if remainder.is_empty() {
            return matched;
        }
        let Some(after_comma) = remainder.strip_prefix(',') else {
            return false;
        };
        remainder = after_comma;
    }
}

/// Parses one entity tag and returns its opaque tag plus the remaining input.
/// The optional weak marker is intentionally discarded: RFC 9110 specifies
/// weak comparison for If-None-Match on GET and HEAD.
fn parse_entity_tag(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix("W/").unwrap_or(value);
    let quoted = value.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let opaque_tag = &quoted[..end];
    if !opaque_tag.bytes().all(is_etagc) {
        return None;
    }
    Some((opaque_tag, &quoted[end + 1..]))
}

fn is_etagc(byte: u8) -> bool {
    byte == b'!' || (b'#'..=b'~').contains(&byte) || byte >= 0x80
}

fn trim_ows(value: &str) -> &str {
    value.trim_matches([' ', '\t'])
}

fn trim_ows_start(value: &str) -> &str {
    value.trim_start_matches([' ', '\t'])
}

fn stream_gzip(
    mut reader: PinnedGalaxyArtifact,
) -> impl futures::Stream<Item = Result<Bytes, io::Error>> {
    stream! {
        let expected_transport_hash = reader.metadata().transport_sha256.clone();
        let chunk_count = reader.metadata().chunk_count;
        let mut transport_hasher = Sha256::new();
        for index in 0..chunk_count {
            match reader.read_chunk(index).await {
                Ok(chunk) => {
                    transport_hasher.update(&chunk.bytes);
                    djinn_telemetry::galaxy_artifact_route::record_chunk(chunk.bytes.len());
                    yield Ok(Bytes::from(chunk.bytes));
                }
                Err(error) => {
                    tracing::warn!(%error, chunk_index = index, "galaxy route stream chunk failed");
                    djinn_telemetry::galaxy_artifact_route::record_outcome("stream_error");
                    let _ = reader.finish().await;
                    yield Err(io::Error::other("galaxy artifact stream aborted"));
                    return;
                }
            }
        }
        if format!("{:x}", transport_hasher.finalize()) != expected_transport_hash {
            djinn_telemetry::galaxy_artifact_route::record_outcome("stream_error");
            let _ = reader.finish().await;
            yield Err(io::Error::other("galaxy artifact transport hash mismatch"));
            return;
        }
        if let Err(error) = reader.finish().await {
            tracing::warn!(%error, "galaxy route stream finish failed");
            djinn_telemetry::galaxy_artifact_route::record_outcome("stream_error");
            yield Err(io::Error::other("galaxy artifact stream release failed"));
        }
    }
}

fn error_response(status: StatusCode, code: &'static str) -> Response {
    djinn_telemetry::galaxy_artifact_route::record_outcome(match status {
        StatusCode::UNAUTHORIZED => "unauthenticated",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "unavailable",
        StatusCode::CONFLICT => "unsupported",
        _ => "corrupt",
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{{\"code\":\"{code}\"}}"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> djinn_db::PinnedGalaxyArtifactMetadata {
        djinn_db::PinnedGalaxyArtifactMetadata {
            project_id: "project-id".to_owned(),
            generation_id: "00000000-0000-7000-8000-000000000001".to_owned(),
            commit_sha: "a".repeat(40),
            artifact_id: "00000000-0000-7000-8000-000000000002".to_owned(),
            graph_content_hash: "b".repeat(64),
            transport_sha256: "c".repeat(64),
            artifact_version: 1,
            encoding: "gzip".to_owned(),
            chunk_count: 2,
            byte_count: 3,
        }
    }

    #[test]
    fn identity_headers_quote_transport_etag_and_keep_fixed_identity() {
        let metadata = metadata();
        let etag = format!("\"{}\"", metadata.transport_sha256);
        let headers = identity_headers(&metadata, &etag).expect("safe metadata headers");
        assert_eq!(headers[header::ETAG], etag);
        assert_eq!(headers[HEADER_PROJECT_ID], "project-id");
        assert_eq!(headers[HEADER_ARTIFACT_VERSION], "1");
        assert_eq!(headers[header::CONTENT_ENCODING], "gzip");
    }

    #[test]
    fn if_none_match_uses_weak_comparison_and_list_grammar() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"abc\", W/\"def\""),
        );
        assert!(etag_matches(&headers, "\"def\""));
        assert!(!etag_matches(&headers, "def"));
        assert!(!etag_matches(&headers, "\"de\""));
    }

    #[test]
    fn if_none_match_wildcard_matches_any_current_representation() {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(etag_matches(&headers, "\"transport-sha256\""));
    }

    #[test]
    fn malformed_if_none_match_does_not_match() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("W/\"transport-sha256\", invalid"),
        );
        assert!(!etag_matches(&headers, "\"transport-sha256\""));
    }
}
