//! Real-Postgres service tests for authenticated readiness kickoff.
//!
//! The fixture project is deliberately owned by a GitHub *organization* while
//! every seeded user has a personal login, reproducing the production shape in
//! which no user's `github_login` can ever equal the project's `github_owner`.

use async_trait::async_trait;
use djinn_control_plane::readiness_kickoff::{
    READINESS_SKILL_NAME, READINESS_SKILL_VERSION, ReadinessKickoffError, ReadinessKickoffRequest,
    ReadinessKickoffService, ReadinessSkillPinError, ReadinessSkillPinResolver,
};
use djinn_core::events::EventBus;
use djinn_db::{
    Database, ProjectRepository, RepoGraphCacheInsert, RepoGraphCacheRepository, UserRepository,
};

/// The fixture project's GitHub owner. An organization, never a person.
const ORGANIZATION_OWNER: &str = "readiness-org";

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
    let (project_id, runner_id, _) = seed_project_and_users(&db).await;

    let blank = service(&db, PinState::Available)
        .kickoff(request(&project_id, &runner_id, " \t "))
        .await
        .expect_err("blank keys are rejected before work materializes");
    assert_eq!(blank, ReadinessKickoffError::BlankIdempotencyKey);
    assert_counts(&db, &project_id, (0, 0)).await;

    // Authentication is still mandatory. A caller whose user row does not
    // exist must be rejected, never silently admitted now that the owner
    // equality gate is gone.
    let unauthenticated = service(&db, PinState::Available)
        .kickoff(request(&project_id, "missing-user", "unauthenticated"))
        .await
        .expect_err("an unresolvable user is rejected before work materializes");
    assert!(matches!(
        unauthenticated,
        ReadinessKickoffError::AuthenticatedOwnerNotFound { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;

    let missing_project = service(&db, PinState::Available)
        .kickoff(request("missing-project", &runner_id, "missing-project"))
        .await
        .expect_err("an unknown project is rejected before work materializes");
    assert!(matches!(
        missing_project,
        ReadinessKickoffError::ProjectNotFound { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;

    let no_snapshot = service(&db, PinState::Available)
        .kickoff(request(&project_id, &runner_id, "no-snapshot"))
        .await
        .expect_err("no immutable snapshot is rejected before work materializes");
    assert!(matches!(
        no_snapshot,
        ReadinessKickoffError::ImmutableSnapshotUnavailable { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;

    seed_snapshot(&db, &project_id).await;
    let unavailable = service(&db, PinState::Unavailable)
        .kickoff(request(&project_id, &runner_id, "pin-unavailable"))
        .await
        .expect_err("unavailable pin is rejected before work materializes");
    assert!(matches!(
        unavailable,
        ReadinessKickoffError::SkillPinUnavailable { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;

    let wrong = service(&db, PinState::Wrong)
        .kickoff(request(&project_id, &runner_id, "pin-wrong"))
        .await
        .expect_err("wrong pin is rejected before work materializes");
    assert!(matches!(
        wrong,
        ReadinessKickoffError::SkillPinMismatch { .. }
    ));
    assert_counts(&db, &project_id, (0, 0)).await;
}

#[tokio::test]
async fn any_authenticated_runner_starts_an_org_owned_project_and_owns_the_attribution() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, runner_id, other_user_id) = seed_project_and_users(&db).await;
    seed_snapshot(&db, &project_id).await;
    let service = service(&db, PinState::Available);

    let first = service
        .kickoff(request(&project_id, &runner_id, "same-key"))
        .await
        .expect("a personal login starts readiness on an organization-owned project");
    let redelivery = service
        .kickoff(request(&project_id, &runner_id, "same-key"))
        .await
        .expect("same key redelivery");
    assert_eq!(first.run.id, redelivery.run.id);
    assert_eq!(
        first.identification_task.id,
        redelivery.identification_task.id
    );
    assert_counts(&db, &project_id, (1, 1)).await;

    // Cost attribution follows whoever ran it: both the run row and the
    // dispatched identification task record the authenticated runner.
    assert_eq!(
        run_creator(&db, &first.run.id).await.as_deref(),
        Some(runner_id.as_str())
    );
    assert_eq!(
        identification_task_creator(&db, &first.identification_task.id)
            .await
            .as_deref(),
        Some(runner_id.as_str())
    );

    // A different authenticated user, equally unrelated to the owning
    // organization, starts their own run on their own project and receives
    // their own attribution.
    let other_project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("readiness-other-project", ORGANIZATION_OWNER, "other")
        .await
        .expect("create the second organization-owned project");
    seed_snapshot(&db, &other_project.id).await;
    let by_other = service
        .kickoff(request(&other_project.id, &other_user_id, "other-key"))
        .await
        .expect("a second personal login also starts readiness");
    assert_eq!(
        run_creator(&db, &by_other.run.id).await.as_deref(),
        Some(other_user_id.as_str())
    );
    assert_eq!(
        identification_task_creator(&db, &by_other.identification_task.id)
            .await
            .as_deref(),
        Some(other_user_id.as_str())
    );

    let second_project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("readiness-race", ORGANIZATION_OWNER, "race")
        .await
        .expect("create second project");
    seed_snapshot(&db, &second_project.id).await;
    let left = service.kickoff(request(&second_project.id, &runner_id, "left"));
    let right = service.kickoff(request(&second_project.id, &runner_id, "right"));
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

/// Seed the production shape: the project belongs to a GitHub organization and
/// both users have personal logins, so neither login equals `github_owner`.
async fn seed_project_and_users(db: &Database) -> (String, String, String) {
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("readiness-control", ORGANIZATION_OWNER, "control")
        .await
        .expect("create project");
    let users = UserRepository::new(db.clone());
    let runner = users
        .upsert_from_github(501_001, "readiness-runner", None, None)
        .await
        .expect("create runner");
    let other = users
        .upsert_from_github(501_002, "readiness-other", None, None)
        .await
        .expect("create other user");
    for login in [&runner.github_login, &other.github_login] {
        assert_ne!(
            login.to_ascii_lowercase(),
            ORGANIZATION_OWNER,
            "the fixture must keep every personal login distinct from the owning organization"
        );
    }
    (project.id, runner.id, other.id)
}

/// The persisted creator of a readiness run, as recorded on the run row.
async fn run_creator(db: &Database, run_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT created_by_user_id FROM readiness_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_one(db.pool())
    .await
    .expect("read the persisted readiness run creator")
}

/// The persisted creator of a run's identification task, which is the row that
/// dispatch reads for credential and cost attribution.
async fn identification_task_creator(db: &Database, task_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT created_by_user_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(db.pool())
        .await
        .expect("read the persisted identification task creator")
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
