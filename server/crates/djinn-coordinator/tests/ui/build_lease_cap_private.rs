use djinn_coordinator::build_lease::BuildLeaseService;

fn external_code_cannot_bypass_snapshot_adoption(service: &BuildLeaseService) {
    let _ = &service.cap;
    let _ = &service.derived_fallback;
    service.set_cap_directly(99);
}

fn main() {}
