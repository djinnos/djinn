use super::*;

impl RepoGraphBridge {
    pub(super) async fn flow(
        &self,
        _ctx: &ProjectCtx,
        _query: &str,
        _kind_filter: Option<&str>,
        _limit: usize,
    ) -> Result<FlowResult, String> {
        // TODO(ykcg-flow): reuse hybrid_search (BM25 + semantic + structural
        // RRF) and map symbol hits through process memberships. Graphs without
        // process sidecar data intentionally return an empty hit list.
        Ok(FlowResult::default())
    }
}
