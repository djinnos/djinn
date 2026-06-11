use super::*;

impl RepoGraphBridge {
    pub(super) async fn flow(
        &self,
        _ctx: &ProjectCtx,
        _query: &str,
        _kind_filter: Option<&str>,
        _limit: usize,
    ) -> Result<FlowResult, String> {
        Ok(FlowResult::default())
    }
}
