//! Data-layer background tasks that operate directly on `Database` and
//! repositories. Extracted from `djinn-server` so they live next to the
//! repositories they drive.
//!
//! Each submodule exposes a `spawn` function that takes plain ingredients
//! (`Database`, `EventBus`, `CancellationToken`, etc.) — server code wires
//! them up at startup instead of handing over an `AppState`.

pub mod housekeeping;
