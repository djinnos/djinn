use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct LspWarning {
    pub server: String,
    pub message: String,
}

// ── LSP ─────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LspOps: Send + Sync {
    async fn warnings(&self) -> Vec<LspWarning>;
}
