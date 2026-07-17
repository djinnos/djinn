use std::collections::HashSet;

use super::*;

#[tokio::test]
async fn file_era_discovery_is_case_insensitive_and_includes_adr_links() {
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
    let title_hit = repo
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
        .create(
            &project.id,
            "Linked historical rationale",
            "This record deliberately uses no discovery keyword.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    repo.update(
        &linked_only.id,
        &linked_only.title,
        "This record deliberately uses no discovery keyword. See [[ADR-057 File-era architecture]].",
        "[]",
    )
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

    let discovered = repo
        .discover_file_era_guidance(&project.id, &adr.id, &["project directory", ".djinn/"])
        .await
        .unwrap();
    let ids = discovered
        .notes
        .into_iter()
        .map(|note| note.id)
        .collect::<HashSet<_>>();
    assert_eq!(ids.len(), 4, "every affected record appears exactly once");
    assert!(ids.contains(&adr.id));
    assert!(ids.contains(&title_hit.id));
    assert!(ids.contains(&body_hit.id));
    assert!(ids.contains(&linked_only.id));
    assert!(!ids.contains(&unrelated.id));
}
