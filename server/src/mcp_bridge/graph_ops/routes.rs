use djinn_control_plane::bridge::{
    ApiImpactResult, ProjectCtx, RouteMapResult, RouteShape, ShapeCheckResult,
};

use super::RepoGraphBridge;

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
        // TODO(ykcg-route-map): resolve Route nodes and walk HandlesRoute,
        // Fetches, and EntryPointOf edges. Until route extraction data is
        // present in cached graphs, an empty result is the safe success shape.
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
        // TODO(ykcg-shape-check): resolve the route/handler, extract response
        // keys, and compare consumer accesses. Empty graphs should continue to
        // return this default rather than a hard route-not-found error.
        Ok(ShapeCheckResult {
            route_shape: RouteShape {
                route: None,
                response_keys: Vec::new(),
            },
            drifts: Vec::new(),
        })
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
        // TODO(ykcg-api-impact): combine shape_check drift and the existing
        // impact traversal to score consumer risk tiers.
        Ok(ApiImpactResult::default())
    }
}
