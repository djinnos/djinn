//! Provider-catalog refresh cadence.
//!
//! The refresh loop implementation now lives in [`djinn_provider::catalog::refresh`]
//! (alongside [`djinn_provider::catalog::CatalogService`]) so that integration tests
//! in downstream crates — notably `djinn-control-plane` — can drive the **same**
//! single-owner refresh loop against a mocked upstream URL. This module re-exports
//! the items the server state needs from the canonical location.

pub(crate) use djinn_provider::catalog::refresh::refresh_interval_from_env;
pub(crate) use djinn_provider::catalog::refresh::run_provider_catalog_refresh_loop;
