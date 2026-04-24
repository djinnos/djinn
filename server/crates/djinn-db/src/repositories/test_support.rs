use std::path::{Path, PathBuf};

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;
use tokio::sync::broadcast;

use crate::database::Database;
use crate::repositories::note::NoteRepository;

pub fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| {
        let _ = tx.send(event);
    })
}

pub async fn make_project(db: &Database, path: &Path) -> Project {
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let path_slug = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("root");
    let project_name = format!("test-project-{path_slug}-{id}");
    // Synthesize unique (owner, repo) coords; the actual filesystem
    // `path` argument is used only for downstream note fixtures, not
    // persisted.
    let owner = "test";
    let repo_slug = format!("{path_slug}-{id}");
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES (?, ?, ?, ?)",
        id,
        project_name,
        owner,
        repo_slug,
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query_as!(
        Project,
        r#"SELECT id, name,
                  github_owner AS "github_owner!: String",
                  github_repo  AS "github_repo!: String",
                  created_at, target_branch,
                  auto_merge AS "auto_merge!: bool",
                  sync_enabled AS "sync_enabled!: bool",
                  sync_remote
           FROM projects WHERE id = ?"#,
        id
    )
    .fetch_one(db.pool())
    .await
    .unwrap()
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixtureExpectedCounts {
    pub prune_associations: u64,
    pub flag_orphan_notes: u64,
    pub rebuild_missing_content_hashes: u64,
    pub repair_broken_wikilinks: u64,
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixtureProject {
    pub project: Project,
    pub path: PathBuf,
    pub expected: HousekeepingFixtureExpectedCounts,
    pub orphan_note_id: String,
    pub repaired_source_note_id: String,
    pub repaired_target_note_id: String,
    pub legacy_hash_note_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixture {
    pub projects: Vec<HousekeepingFixtureProject>,
}

pub async fn build_multi_project_housekeeping_fixture(db: &Database) -> HousekeepingFixture {
    let tmp = crate::database::test_tempdir().unwrap();
    let root = tmp.keep();
    let project_one_path = root.join("project-one");
    let project_two_path = root.join("project-two");
    std::fs::create_dir_all(&project_one_path).unwrap();
    std::fs::create_dir_all(&project_two_path).unwrap();

    let project_one = make_project(db, &project_one_path).await;
    let project_two = make_project(db, &project_two_path).await;

    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let project_one_stale_a = repo
        .create(
            &project_one.id,
            "Project One Stale A",
            "content one",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_stale_b = repo
        .create(
            &project_one.id,
            "Project One Stale B",
            "content two",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_recent_a = repo
        .create(
            &project_one.id,
            "Project One Recent A",
            "content three",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_recent_b = repo
        .create(
            &project_one.id,
            "Project One Recent B",
            "content four",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_orphan = repo
        .create(
            &project_one.id,
            "Project One Orphan",
            "orphan body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_linked_target = repo
        .create(
            &project_one.id,
            "Project One Linked Target",
            "linked body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _project_one_linked_source = repo
        .create(
            &project_one.id,
            "Project One Linked Source",
            &format!("links to [[{}]]", project_one_linked_target.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_canonical_hash = repo
        .create_db_note(
            &project_one.id,
            "Project One Canonical Hash",
            "Alpha\r\nBeta\n",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_legacy_hash = repo
        .create_db_note(
            &project_one.id,
            "Project One Legacy Hash",
            " Alpha\nBeta ",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_repair_target = repo
        .create(
            &project_one.id,
            "Rust Ownership Guide",
            "Rust Ownership. Rust Ownership. Rust Ownership. Rust Ownership. Borrowing and lifetimes details.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_repair_source = repo
        .create(
            &project_one.id,
            "Project One Broken Link Source",
            "Read [[Rust Ownership]] before editing.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let project_two_stale_a = repo
        .create(
            &project_two.id,
            "Project Two Stale A",
            "content five",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_stale_b = repo
        .create(
            &project_two.id,
            "Project Two Stale B",
            "content six",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_recent_a = repo
        .create(
            &project_two.id,
            "Project Two Recent A",
            "content seven",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_recent_b = repo
        .create(
            &project_two.id,
            "Project Two Recent B",
            "content eight",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_orphan = repo
        .create(
            &project_two.id,
            "Project Two Orphan",
            "orphan body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_linked_target = repo
        .create(
            &project_two.id,
            "Project Two Linked Target",
            "linked body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _project_two_linked_source = repo
        .create(
            &project_two.id,
            "Project Two Linked Source",
            &format!("links to [[{}]]", project_two_linked_target.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_canonical_hash = repo
        .create_db_note(
            &project_two.id,
            "Project Two Canonical Hash",
            "Gamma\r\nDelta\n",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_legacy_hash = repo
        .create_db_note(
            &project_two.id,
            "Project Two Legacy Hash",
            " Gamma\nDelta ",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_repair_target = repo
        .create(
            &project_two.id,
            "Async Runtime Guide",
            "Async Runtime. Async Runtime. Async Runtime. Async Runtime. Scheduling and executors details.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_repair_source = repo
        .create(
            &project_two.id,
            "Project Two Broken Link Source",
            "Review [[Async Runtime]] before tuning workers.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    repo.upsert_association(&project_one_stale_a.id, &project_one_stale_b.id, 1)
        .await
        .unwrap();
    repo.upsert_association(&project_one_recent_a.id, &project_one_recent_b.id, 6)
        .await
        .unwrap();
    repo.upsert_association(&project_two_stale_a.id, &project_two_stale_b.id, 1)
        .await
        .unwrap();
    repo.upsert_association(&project_two_recent_a.id, &project_two_recent_b.id, 6)
        .await
        .unwrap();

    sqlx::query!(
        "UPDATE note_associations
         SET last_co_access = DATE_SUB(NOW(3), INTERVAL 100 DAY)
         WHERE (note_a_id = ? AND note_b_id = ?)
            OR (note_a_id = ? AND note_b_id = ?)",
        project_one_stale_a.id,
        project_one_stale_b.id,
        project_two_stale_a.id,
        project_two_stale_b.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        "UPDATE note_associations
         SET last_co_access = DATE_SUB(NOW(3), INTERVAL 1 DAY)
         WHERE (note_a_id = ? AND note_b_id = ?)
            OR (note_a_id = ? AND note_b_id = ?)",
        project_one_recent_a.id,
        project_one_recent_b.id,
        project_two_recent_a.id,
        project_two_recent_b.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        "UPDATE notes
         SET last_accessed = DATE_SUB(NOW(3), INTERVAL 31 DAY), access_count = 0
         WHERE id IN (?, ?, ?, ?)",
        project_one_orphan.id,
        project_one_linked_target.id,
        project_two_orphan.id,
        project_two_linked_target.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        "UPDATE notes SET content_hash = NULL WHERE id IN (?, ?, ?, ?)",
        project_one_canonical_hash.id,
        project_one_legacy_hash.id,
        project_two_canonical_hash.id,
        project_two_legacy_hash.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    HousekeepingFixture {
        projects: vec![
            HousekeepingFixtureProject {
                project: project_one,
                path: project_one_path,
                expected: HousekeepingFixtureExpectedCounts {
                    prune_associations: 1,
                    flag_orphan_notes: 1,
                    rebuild_missing_content_hashes: 2,
                    repair_broken_wikilinks: 1,
                },
                orphan_note_id: project_one_orphan.id,
                repaired_source_note_id: project_one_repair_source.id,
                repaired_target_note_id: project_one_repair_target.id,
                legacy_hash_note_ids: vec![
                    project_one_canonical_hash.id,
                    project_one_legacy_hash.id,
                ],
            },
            HousekeepingFixtureProject {
                project: project_two,
                path: project_two_path,
                expected: HousekeepingFixtureExpectedCounts {
                    prune_associations: 1,
                    flag_orphan_notes: 1,
                    rebuild_missing_content_hashes: 2,
                    repair_broken_wikilinks: 1,
                },
                orphan_note_id: project_two_orphan.id,
                repaired_source_note_id: project_two_repair_source.id,
                repaired_target_note_id: project_two_repair_target.id,
                legacy_hash_note_ids: vec![
                    project_two_canonical_hash.id,
                    project_two_legacy_hash.id,
                ],
            },
        ],
    }
}
