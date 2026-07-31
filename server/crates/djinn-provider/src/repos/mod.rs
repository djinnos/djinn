//! Compatibility re-export.
//!
//! `CredentialRepository` and `CustomProviderRepository` live in `djinn-db`
//! alongside every other repository — they issue Postgres queries and belong on
//! the far side of the raw-SQL boundary that `scripts/check-raw-sql-boundary.sh`
//! polices. They spent time in this crate only as the residue of a half-reverted
//! move: `ef02b5104` moved them out of djinn-db, and `4c78658c4` moved the
//! crypto half back the same day "so CredentialRepository can access crypto
//! without depending on the provider crate" — leaving the repositories stranded.
//!
//! This shim keeps `djinn_provider::repos::*` working for existing call sites.
//! New code should import from `djinn_db` directly.
pub use djinn_db::repositories::{credential, custom_provider};
pub use djinn_db::{CredentialRepository, CustomProviderRepository};
