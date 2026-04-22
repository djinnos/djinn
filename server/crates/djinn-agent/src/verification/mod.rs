pub mod environment;
pub mod mcp_json;
pub mod scoped;
pub mod service;
pub mod settings;
pub mod task_confidence;

#[derive(Clone, Debug, serde::Serialize)]
pub enum StepEvent {
    Started {
        index: u32,
        total: u32,
        name: String,
        command: String,
    },
    Finished {
        index: u32,
        name: String,
        exit_code: i32,
        duration_ms: u64,
        stdout: String,
        stderr: String,
    },
    PhaseComplete {
        passed: bool,
        total_duration_ms: u64,
    },
    CacheHit {
        commit_sha: String,
        cached_at: String,
        original_duration_ms: u64,
    },
}
