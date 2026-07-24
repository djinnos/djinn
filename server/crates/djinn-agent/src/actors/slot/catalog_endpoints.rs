use djinn_k8s::sidecar::ResolvedCatalogService;
use djinn_sandbox::final_verification::FinalVerificationLoopbackEndpoint;

pub(super) fn catalog_loopback_endpoints(
    services: &[ResolvedCatalogService],
) -> Result<Vec<FinalVerificationLoopbackEndpoint>, &'static str> {
    services
        .iter()
        .map(|service| {
            Ok(FinalVerificationLoopbackEndpoint {
                preset_id: service.preset_id.clone(),
                port: u16::try_from(service.port)
                    .map_err(|_| "strict catalog service port is outside u16")?,
            })
        })
        .collect()
}
