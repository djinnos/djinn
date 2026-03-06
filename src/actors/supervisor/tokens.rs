use super::*;

impl AgentSupervisor {
    pub(super) async fn tokens_for_session(&self, goose_session_id: &str) -> (i64, i64) {
        let session = self
            .session_manager
            .get_session(goose_session_id, false)
            .await;
        let Ok(session) = session else {
            if let Some(tokens) = Self::tokens_from_goose_sqlite(goose_session_id).await {
                return tokens;
            }
            return (0, 0);
        };

        let tokens_in = session
            .accumulated_input_tokens
            .or(session.input_tokens)
            .unwrap_or(0) as i64;
        let tokens_out = session
            .accumulated_output_tokens
            .or(session.output_tokens)
            .unwrap_or(0) as i64;

        if tokens_in == 0
            && tokens_out == 0
            && let Some(tokens) = Self::tokens_from_goose_sqlite(goose_session_id).await
        {
            return tokens;
        }

        (tokens_in, tokens_out)
    }

    pub(super) async fn update_session_record(
        &self,
        record_id: Option<&str>,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
    ) {
        let Some(record_id) = record_id else {
            return;
        };

        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        if let Err(e) = repo.update(record_id, status, tokens_in, tokens_out).await {
            tracing::warn!(record_id = %record_id, error = %e, "failed to update session record");
        }
    }

    pub(super) async fn tokens_from_goose_sqlite(goose_session_id: &str) -> Option<(i64, i64)> {
        for db_path in Self::goose_session_db_candidates() {
            let Some(tokens) = Self::tokens_from_goose_sqlite_at(&db_path, goose_session_id).await
            else {
                continue;
            };
            return Some(tokens);
        }

        None
    }

    fn goose_session_db_candidates() -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        if let Ok(root) = std::env::var("GOOSE_PATH_ROOT") {
            let root = PathBuf::from(root);
            candidates.push(root.join("data").join("sessions").join("sessions.db"));
        }

        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".djinn").join("sessions").join("sessions.db"));
            candidates.push(
                home.join(".djinn")
                    .join("sessions")
                    .join("sessions")
                    .join("sessions.db"),
            );
        }

        candidates
    }

    pub(super) async fn tokens_from_goose_sqlite_at(
        db_path: &Path,
        goose_session_id: &str,
    ) -> Option<(i64, i64)> {
        if !db_path.exists() {
            return None;
        }

        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .read_only(true)
            .create_if_missing(false)
            .busy_timeout(Duration::from_secs(1));

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .ok()?;

        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT COALESCE(accumulated_input_tokens, input_tokens, 0), COALESCE(accumulated_output_tokens, output_tokens, 0) FROM sessions WHERE id = ?1",
        )
        .bind(goose_session_id)
        .fetch_optional(&pool)
        .await
        .ok()??;

        Some(row)
    }
}
