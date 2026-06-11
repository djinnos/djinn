use super::*;

impl RepoGraphBridge {
    pub(super) async fn route_map(
        &self,
        _ctx: &ProjectCtx,
        _route_id: Option<&str>,
        _method: Option<&str>,
        _path_glob: Option<&str>,
        _framework: Option<&str>,
        _limit: usize,
    ) -> Result<RouteMapResult, String> {
        Ok(RouteMapResult::default())
    }

    pub(super) async fn shape_check(
        &self,
        _ctx: &ProjectCtx,
        _route_id: Option<&str>,
        _method: Option<&str>,
        _path: Option<&str>,
        _include_optional: bool,
    ) -> Result<ShapeCheckResult, String> {
        Ok(ShapeCheckResult::default())
    }

    pub(super) async fn api_impact(
        &self,
        _ctx: &ProjectCtx,
        _route_id: Option<&str>,
        _method: Option<&str>,
        _path: Option<&str>,
        _min_confidence: f64,
        _limit: usize,
    ) -> Result<ApiImpactResult, String> {
        Ok(ApiImpactResult::default())
    }
}
