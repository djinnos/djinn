use super::*;

/// Maximum retained candidate rows in one persisted mismatch scan page.
pub const BOARD_HEALTH_MISMATCH_PAGE_SIZE: i64 = 250;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BoardHealthMismatchScanState {
    /// True from pass creation through completion, including an empty pass.
    /// This distinguishes an empty in-progress pass from an idle reset state.
    pub active: bool,
    pub cursor_id: Option<String>,
    pub eligible_high_water_id: Option<String>,
    pub pass_id: Option<String>,
    pub pass_started_at: Option<String>,
    pub leader_epoch: i64,
    pub completed_at: Option<String>,
    pub last_pass_duration_ms: Option<i64>,
}

#[derive(Debug)]
pub struct BoardHealthMismatchPage {
    pub state: BoardHealthMismatchScanState,
    pub candidates: Vec<BoardHealthMismatchCandidate>,
}

const SCAN_STATE_COLUMNS: &str = "active, cursor_id, eligible_high_water_id, pass_id, pass_started_at, leader_epoch, completed_at, last_pass_duration_ms";

impl TaskRepository {
    /// Creates an idle singleton then captures high-water, or resumes its cursor.
    /// `active`, rather than high-water, distinguishes a completed pass from an
    /// unfinished empty pass.
    pub async fn start_or_resume_board_health_mismatch_pass(
        &self,
        leader_epoch: i64,
    ) -> Result<BoardHealthMismatchScanState> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(
            "INSERT INTO board_health_mismatch_scan_state (singleton) VALUES (TRUE) \
             ON CONFLICT (singleton) DO NOTHING",
        )
        .execute(&mut *tx)
        .await?;
        let state: BoardHealthMismatchScanState = sqlx::query_as(&format!(
            "SELECT {SCAN_STATE_COLUMNS} FROM board_health_mismatch_scan_state \
             WHERE singleton FOR UPDATE"
        ))
        .fetch_one(&mut *tx)
        .await?;
        if leader_epoch < state.leader_epoch {
            return Err(Error::InvalidData(
                "board-health mismatch pass is owned by a newer leader epoch".into(),
            ));
        }

        let result = if !state.active {
            let high_water: Option<String> = sqlx::query_scalar(&format!(
                "SELECT MAX(t.id) FROM tasks t WHERE {}",
                board_health_mismatch_predicate("t")
            ))
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query_as(&format!(
                "UPDATE board_health_mismatch_scan_state \
                 SET active=TRUE, cursor_id=NULL, eligible_high_water_id=$1, pass_id=$2, \
                     pass_started_at=to_char(now() AT TIME ZONE 'utc', \
                         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                     leader_epoch=$3, completed_at=NULL, last_pass_duration_ms=NULL \
                 WHERE singleton RETURNING {SCAN_STATE_COLUMNS}"
            ))
            .bind(high_water)
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(leader_epoch)
            .fetch_one(&mut *tx)
            .await?
        } else if state.leader_epoch < leader_epoch {
            sqlx::query_as(&format!(
                "UPDATE board_health_mismatch_scan_state SET leader_epoch=$1 \
                 WHERE singleton RETURNING {SCAN_STATE_COLUMNS}"
            ))
            .bind(leader_epoch)
            .fetch_one(&mut *tx)
            .await?
        } else {
            state
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Fetching does not mutate state: a failed evaluator call retries this page.
    pub async fn load_board_health_mismatch_page(
        &self,
        leader_epoch: i64,
    ) -> Result<BoardHealthMismatchPage> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let state: BoardHealthMismatchScanState = sqlx::query_as(&format!(
            "SELECT {SCAN_STATE_COLUMNS} FROM board_health_mismatch_scan_state \
             WHERE singleton FOR UPDATE"
        ))
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            Error::InvalidData("board-health mismatch pass has not been started".into())
        })?;
        if state.leader_epoch != leader_epoch {
            return Err(Error::InvalidData(
                "board-health mismatch pass is owned by another leader epoch".into(),
            ));
        }
        if !state.active {
            return Err(Error::InvalidData(
                "board-health mismatch pass is not active".into(),
            ));
        }
        let candidates = match &state.eligible_high_water_id {
            None => Vec::new(),
            Some(high_water) => {
                sqlx::query_as(&format!(
                    "SELECT t.id, t.short_id, t.epic_id, t.title, t.description, t.design, \
                        t.acceptance_criteria::text AS acceptance_criteria, t.issue_type, \
                        t.status, t.total_reopen_count \
                 FROM tasks t WHERE {} AND ($1::text IS NULL OR t.id > $1) AND t.id <= $2 \
                 ORDER BY t.id ASC LIMIT {}",
                    board_health_mismatch_predicate("t"),
                    BOARD_HEALTH_MISMATCH_PAGE_SIZE
                ))
                .bind(&state.cursor_id)
                .bind(high_water)
                .fetch_all(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        Ok(BoardHealthMismatchPage { state, candidates })
    }

    /// The only success-path cursor mutation; epoch/cursor guards reject stale owners.
    pub async fn commit_board_health_mismatch_page(
        &self,
        leader_epoch: i64,
        expected_cursor: Option<&str>,
        page_last_id: &str,
    ) -> Result<BoardHealthMismatchScanState> {
        self.db.ensure_initialized().await?;
        sqlx::query_as(&format!(
            "UPDATE board_health_mismatch_scan_state SET cursor_id=$1 \
             WHERE singleton AND active AND leader_epoch=$2 \
               AND cursor_id IS NOT DISTINCT FROM $3 \
               AND eligible_high_water_id IS NOT NULL AND $1 <= eligible_high_water_id \
             RETURNING {SCAN_STATE_COLUMNS}"
        ))
        .bind(page_last_id)
        .bind(leader_epoch)
        .bind(expected_cursor)
        .fetch_optional(self.db.pool())
        .await?
        .ok_or_else(|| Error::InvalidData("stale board-health mismatch page commit".into()))
    }

    /// Completion verifies no eligible row remains before resetting the next pass.
    pub async fn complete_board_health_mismatch_pass(
        &self,
        leader_epoch: i64,
    ) -> Result<BoardHealthMismatchScanState> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let state: BoardHealthMismatchScanState = sqlx::query_as(&format!(
            "SELECT {SCAN_STATE_COLUMNS} FROM board_health_mismatch_scan_state \
             WHERE singleton FOR UPDATE"
        ))
        .fetch_one(&mut *tx)
        .await?;
        if state.leader_epoch != leader_epoch {
            return Err(Error::InvalidData(
                "board-health mismatch pass is owned by another leader epoch".into(),
            ));
        }
        if !state.active {
            return Err(Error::InvalidData(
                "board-health mismatch pass is not active".into(),
            ));
        }
        let remaining = match &state.eligible_high_water_id {
            None => false,
            Some(high_water) => {
                sqlx::query_scalar(&format!(
                    "SELECT EXISTS(SELECT 1 FROM tasks t WHERE {} \
                 AND ($1::text IS NULL OR t.id > $1) AND t.id <= $2)",
                    board_health_mismatch_predicate("t")
                ))
                .bind(&state.cursor_id)
                .bind(high_water)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        if remaining {
            return Err(Error::InvalidData(
                "board-health mismatch pass still has an uncommitted page".into(),
            ));
        }
        let result = sqlx::query_as(&format!(
            "UPDATE board_health_mismatch_scan_state \
             SET active=FALSE, cursor_id=NULL, eligible_high_water_id=NULL, \
                 completed_at=to_char(now() AT TIME ZONE 'utc', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                 last_pass_duration_ms=CASE WHEN pass_started_at IS NULL THEN NULL ELSE \
                     (EXTRACT(EPOCH FROM (now() - pass_started_at::timestamptz))*1000)::bigint END \
             WHERE singleton RETURNING {SCAN_STATE_COLUMNS}"
        ))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}
