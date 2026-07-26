//! Real-Postgres service tests for owner-authorized readiness kickoff.

use async_trait::async_trait;
use djinn_control_plane::readiness_kickoff::{
    READINESS_SKILL_NAME, READINESS_SKILL_VERSION, ReadinessKickoffError, ReadinessKickoffRequest,
    ReadinessKickoffService, ReadinessSkillPinError, ReadinessSkillPinResolver,
};
use djinn_core::events::EventBus;
use djinn_db::{
    Database, ProjectRepository, RepoGraphCacheInsert, RepoGraphCacheRepository, UserRepository,
};

#[derive(Clone, Copy)]
enum PinState {
    Available,
    Unavailable,
    Wrong,
}

#[derive(Clone)]
struct TestPinResolver(PinState);

#[async_trait]
impl ReadinessSkillPinResolver for TestPinResolver {
    async fn resolve_exact(
        &self,
        name: &'static str,
        version: &'static str,
    ) -> Result<(), ReadinessSkillPinError> {
        assert_eq!(
            name, READINESS_SKILL_NAME,
            "service must not trust a caller pin"
        );
        assert_eq!(
            version, READINESS_SKILL_VERSION,
            "service must use the exact pin"
        );
        match self.0 {
            PinState::Available => Ok(()),
            PinState::Unavailable => Err(ReadinessSkillPinError::Unavailable {
                detail: "registry offline".into(),
            }),
            PinState::Wrong => Err(ReadinessSkillPinError::WrongPin {
                registered_name: READINESS_SKILL_NAME.into(),
                registered_version: "9.9.9".into(),
            }),
        }
    }
}

#[tokio::test]
async fn rejected_kickoffs_create_no_readiness_work_and_report_distinct_failures() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id, outsider_id) = seed_project_and_users(&db).await;

    let blank = service(&db, PinState::Available)
        .kickoff(request(&project_id, &owner_id, " \t "))
        .await
        .expect_err("blank keys are rejected before work materializes");
    assert_eq!(blank, ReadinessKickoffError::BlankIdempotencyKey);
    assert_counts(&db, &project_id, (0, 0)).await;

    let unauthorized = service(&db, PinState::Available)
        .kickoff(request(&project_id, &outsider_id, "unauthorized"))
        .await
        .expect_err("non-owner is rejected before work materializes");
    assert!(matches!(
        unauthorized,
        ReadinessKickoffError::UnauthorizedOwner { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;

    let no_snapshot = service(&db, PinState::Available)
        .kickoff(request(&project_id, &owner_id, "no-snapshot"))
        .await
        .expect_err("no immutable snapshot is rejected before work materializes");
    assert!(matches!(
        no_snapshot,
        ReadinessKickoffError::ImmutableSnapshotUnavailable { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;

    seed_snapshot(&db, &project_id).await;
    let unavailable = service(&db, PinState::Unavailable)
        .kickoff(request(&project_id, &owner_id, "pin-unavailable"))
        .await
        .expect_err("unavailable pin is rejected before work materializes");
    assert!(matches!(
        unavailable,
        ReadinessKickoffError::SkillPinUnavailable { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;

    let wrong = service(&db, PinState::Wrong)
        .kickoff(request(&project_id, &owner_id, "pin-wrong"))
        .await
        .expect_err("wrong pin is rejected before work materializes");
    assert!(matches!(
        wrong,
        ReadinessKickoffError::SkillPinMismatch { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;
}

#[tokio::test]
async fn authorized_redelivery_and_racing_keys_converge_on_one_materialization() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id, _) = seed_project_and_users(&db).await;
    seed_snapshot(&db, &project_id).await;
    let service = service(&db, PinState::Available);

    let first = service
        .kickoff(request(&project_id, &owner_id, "same-key"))
        .await
        .expect("first kickoff");
    let redelivery = service
        .kickoff(request(&project_id, &owner_id, "same-key"))
        .await
        .expect("same key redelivery");
    assert_eq!(first.run.id, redelivery.run.id);
    assert_eq!(
        first.identification_task.id,
        redelivery.identification_task.id
    );
    assert_counts(&db, &project_id, (1, 1)).await;

    let second_project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("readiness-race", "readiness-owner", "race")
        .await
        .expect("create second project");
    seed_snapshot(&db, &second_project.id).await;
    let left = service.kickoff(request(&second_project.id, &owner_id, "left"));
    let right = service.kickoff(request(&second_project.id, &owner_id, "right"));
    let (left, right) = tokio::join!(left, right);
    let left = left.expect("left race result");
    let right = right.expect("right race result");
    assert_eq!(left.run.id, right.run.id);
    assert_eq!(left.identification_task.id, right.identification_task.id);
    assert_counts(&db, &second_project.id, (1, 1)).await;
}

fn service(db: &Database, pin_state: PinState) -> ReadinessKickoffService<TestPinResolver> {
    ReadinessKickoffService::new(db.clone(), TestPinResolver(pin_state))
}

fn request(project_id: &str, owner_id: &str, idempotency_key: &str) -> ReadinessKickoffRequest {
    ReadinessKickoffRequest {
        project_id: project_id.into(),
        authenticated_owner_id: owner_id.into(),
        idempotency_key: idempotency_key.into(),
    }
}

async fn seed_project_and_users(db: &Database) -> (String, String, String) {
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("readiness-control", "readiness-owner", "control")
        .await
        .expect("create project");
    let users = UserRepository::new(db.clone());
    let owner = users
        .upsert_from_github(501_001, "readiness-owner", None, None)
        .await
        .expect("create owner");
    let outsider = users
        .upsert_from_github(501_002, "readiness-outsider", None, None)
        .await
        .expect("create outsider");
    (project.id, owner.id, outsider.id)
}

async fn seed_snapshot(db: &Database, project_id: &str) {
    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id,
            commit_sha: "d34db33fd34db33fd34db33fd34db33fd34db33f",
            graph_blob: b"persisted graph",
        })
        .await
        .expect("persist immutable snapshot");
}

async fn assert_counts(db: &Database, project_id: &str, expected: (i64, i64)) {
    let runs =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM readiness_runs WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(db.pool())
            .await
            .expect("count runs");
    let tasks = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM tasks WHERE project_id = $1 \
         AND (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END) \
             ->> 'kind' = 'readiness_identification'",
    )
    .bind(project_id)
    .fetch_one(db.pool())
    .await
    .expect("count identification tasks");
    assert_eq!((runs, tasks), expected);
}
