use super::*;
use crate::note_hash::note_content_hash;

fn classification(
    uuid: String,
    classification: &str,
    disposition: &str,
    rationale: &str,
    superseded_by: Option<String>,
    supersedes: Option<String>,
) -> FileEraGuidanceClassification {
    FileEraGuidanceClassification {
        uuid,
        classification: classification.to_owned(),
        disposition: disposition.to_owned(),
        rationale: rationale.to_owned(),
        superseded_by,
        supersedes,
    }
}

#[tokio::test]
async fn file_era_manifest_is_case_insensitive_complete_and_contract_shaped() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let adr = repo
        .create(
            &project.id,
            "ADR-057 File-era architecture",
            "Historical architecture record.",
            "adr",
            "[]",
        )
        .await
        .unwrap();
    let maintained = repo
        .create(
            &project.id,
            "PROJECT DIRECTORY compatibility",
            "No matching body required.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let body_hit = repo
        .create(
            &project.id,
            "Migration guidance",
            "Workers formerly read from a .DJINN/ project directory.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let linked_only = repo
        .create(&project.id, "Linked historical rationale", "This record deliberately uses no discovery keyword. See [[ADR-057 File-era architecture]].", "reference", "[]")
        .await
        .unwrap();
    let unrelated = repo
        .create(
            &project.id,
            "Unrelated guidance",
            "Current MCP semantics.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let manifest = repo
        .build_file_era_guidance_manifest(
            &project.id,
            &adr.id,
            &["project directory", ".djinn/"],
            &[
                classification(
                    adr.id.clone(),
                    "deprecate",
                    "db_supersedes_file",
                    "The file-era architecture is historical and is superseded by DB/MCP guidance.",
                    Some(maintained.id.clone()),
                    None,
                ),
                classification(
                    maintained.id.clone(),
                    "rewrite",
                    "db_supersedes_file",
                    "Maintained compatibility guidance is rewritten for DB/MCP semantics.",
                    None,
                    Some(adr.id.clone()),
                ),
                classification(
                    body_hit.id.clone(),
                    "rewrite",
                    "db_supersedes_file",
                    "The discovered file-era claim is retained only after a DB/MCP rewrite.",
                    None,
                    None,
                ),
                classification(
                    linked_only.id.clone(),
                    "archive",
                    "approved_discard",
                    "ADR-linked historical rationale remains auditable but is not current guidance.",
                    Some(maintained.id.clone()),
                    None,
                ),
            ],
        )
        .await
        .unwrap();

    assert_eq!(manifest.schema, "djinn-retirement-db-guidance/v1");
    assert_eq!(
        manifest.record_count, 4,
        "every affected record appears exactly once"
    );
    assert!(manifest.records.iter().all(|record| {
        !record.uuid.is_empty()
            && !record.permalink.is_empty()
            && !record.status.is_empty()
            && record.normalized_sha256.len() == 64
            && !record.classification.is_empty()
            && !record.disposition.is_empty()
            && !record.rationale.is_empty()
    }));
    assert!(
        !manifest
            .records
            .iter()
            .any(|record| record.uuid == unrelated.id)
    );

    let linked = manifest
        .records
        .iter()
        .find(|record| record.uuid == linked_only.id)
        .unwrap();
    assert_eq!(
        linked.normalized_sha256,
        note_content_hash(&linked_only.content)
    );
    assert_eq!(linked.disposition, "approved_discard");
    assert_eq!(
        linked.superseded_by.as_deref(),
        Some(maintained.id.as_str())
    );

    let json = serde_json::to_value(&manifest).unwrap();
    assert_eq!(json["records"].as_array().unwrap().len(), 4);
    assert_eq!(
        json["records"][0]["normalized_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
}

#[tokio::test]
async fn file_era_manifest_rejects_an_unclassified_discovery_candidate() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));
    let adr = repo
        .create(&project.id, "File-era ADR", "Historical.", "adr", "[]")
        .await
        .unwrap();
    let linked = repo
        .create(
            &project.id,
            "Linked only",
            "[[File-era ADR]]",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let error = repo
        .build_file_era_guidance_manifest(&project.id, &adr.id, &["project directory"], &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no reconciliation disposition"));
    assert!(!linked.id.is_empty());
}
