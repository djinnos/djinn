//! The admission compatibility decision for pre-protocol ("no-handshake")
//! catalog images, and the signed inventory of digests that opens it.
//!
//! # What this decides
//!
//! Since migration 166 an image may **declare** which component owns the CPU
//! quota of the invocation leaf its `djinn-cgroup-launcher` sidecar creates
//! ([`LauncherAuthorityProtocol`]). Every image built before that declaration
//! existed says nothing, and `NULL` is not `leaf-v1`: it is *this artifact
//! never said*.
//!
//! Task `z3gi` (#2837) already owns the decision for artifacts that *did*
//! speak: [`decide_launcher_protocol_admission`] compares a declaration against
//! the persisted `launcher_authority_mode` singleton and admits only an exact
//! agreement. Its [`LauncherProtocolAdmission::UndeclaredProtocol`] verdict is
//! a refusal *pending this task* — its own doc comment says so.
//!
//! [`decide_admission`] here is that decision **composed with** the legacy
//! allowlist, and it is the only place a missing declaration can become an
//! effective quota authority. It is not a second implementation: every
//! artifact that declares anything is routed straight through z3gi's verdict,
//! untouched. The legacy arm requires all of:
//!
//! * the artifact is identified by an **exact, canonical, immutable digest**
//!   (`sha256:` + 64 lowercase hex — never a tag, never an image name), **and**
//! * that digest is listed in a signed inventory of known pre-protocol
//!   artifacts, **and**
//! * the persisted authority mode is [`LauncherAuthorityProtocol::LeafV1`],
//!   which is the behavior those artifacts were built against.
//!
//! Everything else is refused *before* a Job is rendered, let alone before a
//! shell runs inside one: a declaration that disagrees with the mode, an
//! unparseable declaration, an unreadable authority singleton, a missing
//! declaration under `resize-v2`, a malformed digest, a digest the inventory
//! does not list, and an inventory that fails its own provenance/signature
//! validation.
//!
//! # What this deliberately does NOT decide
//!
//! An undeclared image with **no digest at all** is [`AdmissionDecision::Undeclarable`],
//! not a rejection *here*. Migration 166 keeps such rows legal on purpose — a
//! pre-existing `status = 'ready'` row with `registry_digest IS NULL` also has
//! a `NULL` protocol, and the CHECK is scoped to declaring rows so the upgrade
//! cannot strand a live catalog. The refusal for that shape already lives one
//! layer out, in `djinn_k8s::launcher::render_authority_protocol` (#2836),
//! which refuses to render a Job for an image that identifies neither itself
//! nor its quota authority. Duplicating it here as a resolution *error* would
//! turn a reporting path (dispatch readiness, warm, indexer) into a failure
//! path for a row the database is explicitly allowed to hold.
//!
//! So there is still exactly one effective authority per admitted dispatch, and
//! exactly one fence per rejected one.
//!
//! # Arming
//!
//! [`LegacyDigestInventory::Unconfigured`] — no inventory source in the
//! deployment's environment — keeps the pre-existing rule for the *membership*
//! test only: an undeclared, canonically digest-pinned image still maps to
//! `leaf-v1` under `leaf-v1` mode, exactly as #2836 ships today. An inventory
//! that is not configured cannot classify any digest as "unknown", and making
//! the absence of a config file retroactively strand every already-built
//! digest-pinned image is precisely the wedge migration 166 refused to create.
//!
//! The moment a deployment configures an inventory, membership is required and
//! a miss is a rejection. Every *other* rejection class — mode disagreement,
//! missing declaration under `resize-v2`, malformed digest, unusable inventory
//! — is unconditional and needs no configuration to bite.
//!
//! The server authority mode is **not** configured here: it is the durable
//! `launcher_authority_mode` singleton z3gi's migration 167 seeds, read through
//! [`LauncherAuthorityModeRepository`](crate::repositories::launcher_authority_mode::LauncherAuthorityModeRepository).
//! An absent or unreadable singleton is
//! [`LauncherProtocolAdmission::AuthorityUnavailable`] — a refusal, never a
//! default.
//!
//! # Configuration (read at process start, so a restart re-reads it)
//!
//! | variable | meaning |
//! |---|---|
//! | `DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY` | path to the inventory document |
//! | `DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_PUBLIC_KEY` | base64 raw Ed25519 public key (32 bytes) |
//! | `DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_SIGNATURE` | base64 Ed25519 signature (64 bytes) over the document's exact bytes |
//!
//! Document shape:
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "issuer": "platform-ops",
//!   "issued_at": "2026-07-30T00:00:00Z",
//!   "digests": ["sha256:7822b7de...64 lowercase hex..."]
//! }
//! ```
//!
//! Every validation failure — unreadable file, absent key, absent signature,
//! bad base64, wrong key/signature length, failed verification, unknown schema
//! version, malformed digest entry — produces
//! [`LegacyDigestInventory::Unusable`], which rejects **all** legacy
//! compatibility rather than falling back to the unarmed rule. Fail closed
//! means a broken inventory is stricter than no inventory, never looser.
//!
//! To build the document for a live deployment, enumerate the exact rows that
//! need it with
//! [`ImageRepository::legacy_pre_protocol_digests`](crate::repositories::image::ImageRepository::legacy_pre_protocol_digests).

use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use base64::Engine as _;
use djinn_launcher_protocol::LauncherAuthorityProtocol;

use crate::repositories::launcher_authority_mode::{
    LauncherProtocolAdmission, decide_launcher_protocol_admission,
};

/// Environment variable holding the path of the legacy digest inventory.
pub const INVENTORY_PATH_ENV: &str = "DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY";
/// Environment variable holding the base64 Ed25519 public key.
pub const INVENTORY_PUBLIC_KEY_ENV: &str = "DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_PUBLIC_KEY";
/// Environment variable holding the base64 Ed25519 signature.
pub const INVENTORY_SIGNATURE_ENV: &str = "DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_SIGNATURE";

/// The only schema version [`LegacyDigestInventory`] accepts.
pub const INVENTORY_SCHEMA_VERSION: u32 = 1;

const DIGEST_PREFIX: &str = "sha256:";
const DIGEST_HEX_LEN: usize = 64;
const ED25519_PUBLIC_KEY_LEN: usize = 32;
const ED25519_SIGNATURE_LEN: usize = 64;

// ── digests ─────────────────────────────────────────────────────────────────

/// A canonical, immutable OCI manifest digest: `sha256:` followed by exactly 64
/// lowercase hex characters.
///
/// Uppercase hex is rejected rather than normalized. Two spellings of the same
/// digest would make "exact inventory match" a comparison between whichever
/// spelling each side happened to write, and the whole point of comparing
/// digests instead of tags is that the comparison is exact.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PreProtocolDigest(String);

impl PreProtocolDigest {
    /// Parse a canonical digest. `Err` carries the offending input so a
    /// fail-closed caller can name it.
    pub fn parse(raw: &str) -> Result<Self, MalformedDigest> {
        let candidate = raw.trim();
        let malformed = || MalformedDigest {
            input: candidate.to_owned(),
        };
        let hex = candidate
            .strip_prefix(DIGEST_PREFIX)
            .ok_or_else(malformed)?;
        if hex.len() != DIGEST_HEX_LEN {
            return Err(malformed());
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(malformed());
        }
        Ok(Self(candidate.to_owned()))
    }

    /// The canonical `sha256:<64 hex>` text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PreProtocolDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A digest that is not exactly `sha256:` + 64 lowercase hex.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MalformedDigest {
    input: String,
}

impl MalformedDigest {
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for MalformedDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} is not a canonical immutable digest (expected \"sha256:\" followed by exactly \
             64 lowercase hex characters)",
            self.input
        )
    }
}

// ── inventory ───────────────────────────────────────────────────────────────

/// Why a configured inventory cannot be used. Every variant means "grant no
/// legacy compatibility at all".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InventoryFault {
    /// The document path is configured but could not be read.
    Unreadable { path: String, error: String },
    /// The document is configured but no verification key was supplied.
    PublicKeyMissing,
    /// The document is configured but no signature was supplied.
    SignatureMissing,
    /// The key or signature is not base64, or is the wrong length.
    ProvenanceMalformed(String),
    /// Ed25519 verification of the document bytes failed under the configured key.
    SignatureInvalid,
    /// The document is not the JSON shape this module accepts.
    DocumentMalformed(String),
    /// A `digests` entry is not a canonical immutable digest.
    DigestMalformed(MalformedDigest),
}

impl fmt::Display for InventoryFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, error } => {
                write!(formatter, "inventory {path:?} could not be read: {error}")
            }
            Self::PublicKeyMissing => write!(
                formatter,
                "inventory is configured but {INVENTORY_PUBLIC_KEY_ENV} is not set; an unsigned \
                 inventory is never trusted"
            ),
            Self::SignatureMissing => write!(
                formatter,
                "inventory is configured but {INVENTORY_SIGNATURE_ENV} is not set; an unsigned \
                 inventory is never trusted"
            ),
            Self::ProvenanceMalformed(detail) => {
                write!(formatter, "inventory provenance is malformed: {detail}")
            }
            Self::SignatureInvalid => write!(
                formatter,
                "inventory signature does not verify against {INVENTORY_PUBLIC_KEY_ENV}"
            ),
            Self::DocumentMalformed(detail) => {
                write!(formatter, "inventory document is malformed: {detail}")
            }
            Self::DigestMalformed(digest) => {
                write!(formatter, "inventory lists a bad digest: {digest}")
            }
        }
    }
}

/// Who issued a verified inventory, carried so a rejection or an admission can
/// name the document it was decided against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryProvenance {
    pub issuer: String,
    pub issued_at: String,
}

/// The set of pre-protocol artifacts a deployment vouches for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegacyDigestInventory {
    /// No inventory source configured. The membership test is unarmed; see the
    /// module docs. Every other fence still applies.
    Unconfigured,
    /// A signature-verified inventory. Only these digests get compatibility.
    Verified {
        provenance: InventoryProvenance,
        digests: BTreeSet<PreProtocolDigest>,
    },
    /// Configured but unusable. No digest gets compatibility.
    Unusable(InventoryFault),
}

/// The wire shape of the inventory document.
#[derive(Debug, serde::Deserialize)]
struct InventoryDocument {
    schema_version: u32,
    #[serde(default)]
    issuer: String,
    #[serde(default)]
    issued_at: String,
    digests: Vec<String>,
}

impl LegacyDigestInventory {
    /// Build a verified inventory directly, for tests and for callers that
    /// obtained the document through some path other than the environment.
    pub fn verified<I>(issuer: &str, issued_at: &str, digests: I) -> Self
    where
        I: IntoIterator<Item = PreProtocolDigest>,
    {
        Self::Verified {
            provenance: InventoryProvenance {
                issuer: issuer.to_owned(),
                issued_at: issued_at.to_owned(),
            },
            digests: digests.into_iter().collect(),
        }
    }

    /// The process-wide inventory, read from the environment on first use.
    ///
    /// A restart re-reads the document, which is how an operator adds an
    /// artifact to the inventory. Nothing re-reads it in-process: an allowlist
    /// that can change under a running dispatch is not an allowlist.
    pub fn process() -> &'static Self {
        static INVENTORY: OnceLock<LegacyDigestInventory> = OnceLock::new();
        INVENTORY.get_or_init(Self::from_env)
    }

    /// Read the inventory the deployment configured, verifying its provenance.
    pub fn from_env() -> Self {
        let Some(path) = non_empty_env(INVENTORY_PATH_ENV) else {
            return Self::Unconfigured;
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Self::Unusable(InventoryFault::Unreadable {
                    path,
                    error: error.to_string(),
                });
            }
        };
        Self::from_signed_document(
            &bytes,
            non_empty_env(INVENTORY_PUBLIC_KEY_ENV).as_deref(),
            non_empty_env(INVENTORY_SIGNATURE_ENV).as_deref(),
        )
    }

    /// Verify `document` against `public_key_b64`/`signature_b64` and parse it.
    ///
    /// Exposed so the provenance rules are testable without writing files or
    /// mutating process environment.
    pub fn from_signed_document(
        document: &[u8],
        public_key_b64: Option<&str>,
        signature_b64: Option<&str>,
    ) -> Self {
        let Some(public_key_b64) = public_key_b64 else {
            return Self::Unusable(InventoryFault::PublicKeyMissing);
        };
        let Some(signature_b64) = signature_b64 else {
            return Self::Unusable(InventoryFault::SignatureMissing);
        };
        let public_key = match decode_fixed(public_key_b64, ED25519_PUBLIC_KEY_LEN, "public key") {
            Ok(bytes) => bytes,
            Err(fault) => return Self::Unusable(fault),
        };
        let signature = match decode_fixed(signature_b64, ED25519_SIGNATURE_LEN, "signature") {
            Ok(bytes) => bytes,
            Err(fault) => return Self::Unusable(fault),
        };
        if ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public_key)
            .verify(document, &signature)
            .is_err()
        {
            return Self::Unusable(InventoryFault::SignatureInvalid);
        }

        let parsed: InventoryDocument = match serde_json::from_slice(document) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Self::Unusable(InventoryFault::DocumentMalformed(error.to_string()));
            }
        };
        if parsed.schema_version != INVENTORY_SCHEMA_VERSION {
            return Self::Unusable(InventoryFault::DocumentMalformed(format!(
                "schema_version {} is not {INVENTORY_SCHEMA_VERSION}",
                parsed.schema_version
            )));
        }
        let mut digests = BTreeSet::new();
        for raw in &parsed.digests {
            match PreProtocolDigest::parse(raw) {
                Ok(digest) => {
                    digests.insert(digest);
                }
                Err(malformed) => {
                    return Self::Unusable(InventoryFault::DigestMalformed(malformed));
                }
            }
        }
        Self::Verified {
            provenance: InventoryProvenance {
                issuer: parsed.issuer,
                issued_at: parsed.issued_at,
            },
            digests,
        }
    }

    /// Is `digest` vouched for? `Err` when the inventory cannot answer.
    fn vouches_for(&self, digest: &PreProtocolDigest) -> Result<bool, &InventoryFault> {
        match self {
            // Unarmed: the membership question has no configured answer, so it
            // is not asked. See the module docs.
            Self::Unconfigured => Ok(true),
            Self::Verified { digests, .. } => Ok(digests.contains(digest)),
            Self::Unusable(fault) => Err(fault),
        }
    }
}

fn decode_fixed(value: &str, len: usize, what: &str) -> Result<Vec<u8>, InventoryFault> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|error| {
            InventoryFault::ProvenanceMalformed(format!("{what} is not base64: {error}"))
        })?;
    if bytes.len() != len {
        return Err(InventoryFault::ProvenanceMalformed(format!(
            "{what} is {} bytes, expected {len}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

// ── the decision ────────────────────────────────────────────────────────────

/// What admission concluded about one resolved catalog image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionDecision {
    /// The single effective quota authority for this dispatch.
    Admitted(LauncherAuthorityProtocol),
    /// The artifact declares no protocol and carries no digest, so there is no
    /// compatibility claim to evaluate and none may be invented. The render
    /// (`djinn_k8s::launcher::render_authority_protocol`) refuses the Job.
    Undeclarable,
}

/// Why an image may not dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionRejection {
    /// z3gi's verdict on a declaration refused it: it named the other protocol,
    /// it was unparseable, or the authority singleton could not be read. Carried
    /// verbatim rather than restated, so there is exactly one vocabulary for
    /// "this declaration may not run here".
    Declared(LauncherProtocolAdmission),
    /// No declaration, and the persisted authority is not the protocol legacy
    /// artifacts were built against.
    MissingDeclarationUnderMode {
        mode: LauncherAuthorityProtocol,
        digest: String,
    },
    /// No declaration, and the identifying digest is not canonical.
    MalformedDigest(MalformedDigest),
    /// No declaration, and the inventory does not vouch for the digest.
    UninventoriedDigest(PreProtocolDigest),
    /// No declaration, and the configured inventory cannot be trusted.
    InventoryUnusable(InventoryFault),
}

impl fmt::Display for AdmissionRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Declared(LauncherProtocolAdmission::ProtocolMismatch { mode, declared }) => {
                write!(
                    formatter,
                    "the image declares launcher authority protocol {declared} but this server \
                     runs {mode}; dispatching would put two components in charge of one leaf's \
                     cpu.max, so the image must be rebuilt against {mode}"
                )
            }
            Self::Declared(LauncherProtocolAdmission::UnknownProtocol { mode, declared }) => {
                write!(
                    formatter,
                    "the image declares launcher authority protocol {declared:?}, which is not a \
                     protocol this plane knows; refusing to guess that it means {mode}"
                )
            }
            Self::Declared(LauncherProtocolAdmission::AuthorityUnavailable) => write!(
                formatter,
                "the launcher authority mode singleton (migration 167) could not be read; \
                 refusing every dispatch rather than resolving it to a default"
            ),
            // `Admitted` and `UndeclaredProtocol` never reach this arm —
            // `admit_with_legacy_inventory` converts both — but a refusal must
            // still render rather than panic.
            Self::Declared(other) => write!(
                formatter,
                "the image's declared launcher authority protocol was refused: {other:?}"
            ),
            Self::MissingDeclarationUnderMode { mode, digest } => write!(
                formatter,
                "the image at {digest} declares no launcher authority protocol and this server \
                 runs {mode}; a no-handshake artifact may only be admitted under \
                 {leaf}, so the image must be rebuilt",
                leaf = LauncherAuthorityProtocol::LeafV1
            ),
            Self::MalformedDigest(malformed) => write!(
                formatter,
                "the image declares no launcher authority protocol and is not pinned to an \
                 immutable manifest digest: {malformed}. Compatibility is granted to exact \
                 digests only — never to a tag or an image name"
            ),
            Self::UninventoriedDigest(digest) => write!(
                formatter,
                "the image at {digest} declares no launcher authority protocol and is not listed \
                 in the pre-protocol digest inventory ({INVENTORY_PATH_ENV}); rebuild it so it \
                 declares a protocol, or add this exact digest to the signed inventory"
            ),
            Self::InventoryUnusable(fault) => write!(
                formatter,
                "the image declares no launcher authority protocol and the pre-protocol digest \
                 inventory cannot be trusted, so no legacy compatibility is granted: {fault}"
            ),
        }
    }
}

impl std::error::Error for AdmissionRejection {}

/// **The centralized admission compatibility decision.**
///
/// `mode` is the persisted `launcher_authority_mode` (migration 167),
/// `declared` is the artifact's own build-metadata declaration (migration 166),
/// `digest` is `images.registry_digest`, and `inventory` is the deployment's
/// signed pre-protocol allowlist.
///
/// The declaration arm is delegated wholesale to
/// [`decide_launcher_protocol_admission`]; only its
/// [`LauncherProtocolAdmission::UndeclaredProtocol`] refusal — the one z3gi
/// explicitly reserved for this task — is reinterpreted here, and only on the
/// evidence the module docs list.
///
/// Only digests are compared. `tag` is deliberately not a parameter: a tag is
/// mutable naming that can be made to claim anything, and `djinn-image-controller`
/// already has a fixture whose repository segment and tag suffix both read
/// `resize-v2` for an artifact that declares `leaf-v1`.
pub fn decide_admission(
    mode: LauncherAuthorityProtocol,
    declared: Option<LauncherAuthorityProtocol>,
    digest: Option<&str>,
    inventory: &LegacyDigestInventory,
) -> Result<AdmissionDecision, AdmissionRejection> {
    admit_with_legacy_inventory(
        decide_launcher_protocol_admission(mode, declared.map(LauncherAuthorityProtocol::as_wire)),
        digest,
        inventory,
    )
}

/// Reinterpret an already-computed [`LauncherProtocolAdmission`] in the light of
/// the legacy digest inventory.
///
/// Exposed so a caller that has already spent the authority-singleton read (a
/// preflight, a diagnostic) reaches the same verdict without a second round
/// trip — the same split z3gi made between its repository method and its pure
/// decision.
pub fn admit_with_legacy_inventory(
    verdict: LauncherProtocolAdmission,
    digest: Option<&str>,
    inventory: &LegacyDigestInventory,
) -> Result<AdmissionDecision, AdmissionRejection> {
    let mode = match verdict {
        // The artifact spoke and agrees with the authority. Nothing legacy
        // about it; the inventory is not consulted at all.
        LauncherProtocolAdmission::Admitted { mode } => {
            return Ok(AdmissionDecision::Admitted(mode));
        }
        // The one verdict this task owns.
        LauncherProtocolAdmission::UndeclaredProtocol { mode } => mode,
        // Every other refusal stands exactly as z3gi decided it.
        refused => return Err(AdmissionRejection::Declared(refused)),
    };

    // No declaration. Without a digest there is nothing to check a claim
    // against; that shape is refused at the render, not resolved here.
    let Some(raw) = digest.map(str::trim).filter(|d| !d.is_empty()) else {
        return Ok(AdmissionDecision::Undeclarable);
    };

    let digest = PreProtocolDigest::parse(raw).map_err(AdmissionRejection::MalformedDigest)?;

    if mode != LauncherAuthorityProtocol::LeafV1 {
        return Err(AdmissionRejection::MissingDeclarationUnderMode {
            mode,
            digest: digest.as_str().to_owned(),
        });
    }

    match inventory.vouches_for(&digest) {
        Ok(true) => Ok(AdmissionDecision::Admitted(
            LauncherAuthorityProtocol::LeafV1,
        )),
        Ok(false) => Err(AdmissionRejection::UninventoriedDigest(digest)),
        Err(fault) => Err(AdmissionRejection::InventoryUnusable(fault.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canonical digest fixture: `sha256:` + 64 lowercase hex.
    pub(super) fn digest(seed: char) -> String {
        format!("sha256:{}", seed.to_string().repeat(DIGEST_HEX_LEN))
    }

    fn inventory_with(seed: char) -> LegacyDigestInventory {
        LegacyDigestInventory::verified(
            "ops",
            "now",
            [PreProtocolDigest::parse(&digest(seed)).unwrap()],
        )
    }

    #[test]
    fn only_canonical_lowercase_sha256_digests_parse() {
        assert_eq!(
            PreProtocolDigest::parse(&digest('a')).unwrap().as_str(),
            digest('a')
        );
        for rejected in [
            "sha256:abc",                          // too short
            &digest('a')[..70],                    // truncated
            &format!("{}0", digest('a')),          // too long
            &digest('a').to_uppercase(),           // uppercase prefix + hex
            &format!("sha256:{}", "A".repeat(64)), // uppercase hex
            &format!("sha512:{}", "a".repeat(64)), // wrong algorithm
            &"a".repeat(64),                       // no prefix
            &format!("sha256:{}", "g".repeat(64)), // non-hex
            "reg/djinn-image-i1:hash",             // a tag
        ] {
            assert!(
                PreProtocolDigest::parse(rejected).is_err(),
                "{rejected:?} must not parse as a canonical digest"
            );
        }
    }

    /// A declaration is routed straight to z3gi's decision, which admits only
    /// an exact agreement with the persisted authority.
    #[test]
    fn a_declaration_is_honored_only_when_it_matches_the_authority_mode() {
        for mode in LauncherAuthorityProtocol::ALL {
            for declared in LauncherAuthorityProtocol::ALL {
                let outcome = decide_admission(
                    mode,
                    Some(declared),
                    Some(&digest('a')),
                    &LegacyDigestInventory::Unconfigured,
                );
                if declared == mode {
                    assert_eq!(outcome, Ok(AdmissionDecision::Admitted(declared)));
                } else {
                    assert_eq!(
                        outcome,
                        Err(AdmissionRejection::Declared(
                            LauncherProtocolAdmission::ProtocolMismatch { mode, declared }
                        ))
                    );
                }
            }
        }
    }

    /// The refusals z3gi owns pass through untouched, inventory or not: the
    /// legacy allowlist may only reinterpret `UndeclaredProtocol`.
    #[test]
    fn only_the_undeclared_verdict_is_reinterpreted() {
        let inventory = inventory_with('a');
        for verdict in [
            LauncherProtocolAdmission::ProtocolMismatch {
                mode: LauncherAuthorityProtocol::LeafV1,
                declared: LauncherAuthorityProtocol::ResizeV2,
            },
            LauncherProtocolAdmission::UnknownProtocol {
                mode: LauncherAuthorityProtocol::LeafV1,
                declared: "leaf-v9".into(),
            },
            LauncherProtocolAdmission::AuthorityUnavailable,
        ] {
            assert_eq!(
                admit_with_legacy_inventory(verdict.clone(), Some(&digest('a')), &inventory),
                Err(AdmissionRejection::Declared(verdict.clone())),
                "{verdict:?} must survive the legacy arm unchanged"
            );
        }
        // And an admitted declaration is not re-decided either.
        assert_eq!(
            admit_with_legacy_inventory(
                LauncherProtocolAdmission::Admitted {
                    mode: LauncherAuthorityProtocol::ResizeV2
                },
                None,
                &LegacyDigestInventory::Unusable(InventoryFault::SignatureInvalid),
            ),
            Ok(AdmissionDecision::Admitted(
                LauncherAuthorityProtocol::ResizeV2
            ))
        );
    }

    #[test]
    fn a_missing_declaration_never_survives_resize_v2() {
        // Even the inventoried digest is refused: resize-v2 is not the behavior
        // a no-handshake artifact was built against.
        assert!(matches!(
            decide_admission(
                LauncherAuthorityProtocol::ResizeV2,
                None,
                Some(&digest('a')),
                &inventory_with('a')
            ),
            Err(AdmissionRejection::MissingDeclarationUnderMode { .. })
        ));
    }

    #[test]
    fn an_armed_inventory_admits_only_the_exact_digests_it_lists() {
        let inventory = inventory_with('a');
        assert_eq!(
            decide_admission(
                LauncherAuthorityProtocol::LeafV1,
                None,
                Some(&digest('a')),
                &inventory
            ),
            Ok(AdmissionDecision::Admitted(
                LauncherAuthorityProtocol::LeafV1
            ))
        );
        assert!(matches!(
            decide_admission(
                LauncherAuthorityProtocol::LeafV1,
                None,
                Some(&digest('b')),
                &inventory
            ),
            Err(AdmissionRejection::UninventoriedDigest(_))
        ));
    }

    #[test]
    fn a_malformed_digest_is_rejected_even_while_the_inventory_is_unarmed() {
        assert!(matches!(
            decide_admission(
                LauncherAuthorityProtocol::LeafV1,
                None,
                Some("sha256:abc"),
                &LegacyDigestInventory::Unconfigured
            ),
            Err(AdmissionRejection::MalformedDigest(_))
        ));
    }

    #[test]
    fn an_unusable_inventory_is_stricter_than_no_inventory() {
        let unusable = LegacyDigestInventory::Unusable(InventoryFault::SignatureInvalid);
        assert!(matches!(
            decide_admission(
                LauncherAuthorityProtocol::LeafV1,
                None,
                Some(&digest('a')),
                &unusable
            ),
            Err(AdmissionRejection::InventoryUnusable(_))
        ));
        // The same inputs against the unarmed inventory are admitted, so the
        // rejection above is caused by the fault and not by the digest.
        assert_eq!(
            decide_admission(
                LauncherAuthorityProtocol::LeafV1,
                None,
                Some(&digest('a')),
                &LegacyDigestInventory::Unconfigured
            ),
            Ok(AdmissionDecision::Admitted(
                LauncherAuthorityProtocol::LeafV1
            ))
        );
    }

    #[test]
    fn an_artifact_with_neither_declaration_nor_digest_is_left_undeclarable() {
        for absent in [None, Some(""), Some("   ")] {
            for mode in LauncherAuthorityProtocol::ALL {
                assert_eq!(
                    decide_admission(mode, None, absent, &LegacyDigestInventory::Unconfigured),
                    Ok(AdmissionDecision::Undeclarable),
                    "an unidentified artifact must not be given an authority here; \
                     render_authority_protocol refuses it"
                );
            }
        }
    }

    /// An unreadable authority singleton refuses every artifact, legacy or not.
    #[test]
    fn an_unavailable_authority_refuses_even_an_inventoried_digest() {
        assert_eq!(
            admit_with_legacy_inventory(
                LauncherProtocolAdmission::AuthorityUnavailable,
                Some(&digest('a')),
                &inventory_with('a'),
            ),
            Err(AdmissionRejection::Declared(
                LauncherProtocolAdmission::AuthorityUnavailable
            ))
        );
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::tests::digest;
    use super::*;
    use ring::signature::KeyPair as _;

    fn signing_key() -> ring::signature::Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn document() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "issuer": "platform-ops",
            "issued_at": "2026-07-30T00:00:00Z",
            "digests": [digest('a'), digest('b')],
        }))
        .unwrap()
    }

    #[test]
    fn a_correctly_signed_document_verifies_and_yields_its_digests() {
        let key = signing_key();
        let doc = document();
        let signature = key.sign(&doc);
        let inventory = LegacyDigestInventory::from_signed_document(
            &doc,
            Some(&b64(key.public_key().as_ref())),
            Some(&b64(signature.as_ref())),
        );
        let LegacyDigestInventory::Verified {
            provenance,
            digests,
        } = inventory
        else {
            panic!("a correctly signed document must verify, got {inventory:?}");
        };
        assert_eq!(provenance.issuer, "platform-ops");
        assert_eq!(digests.len(), 2);
        assert!(digests.contains(&PreProtocolDigest::parse(&digest('a')).unwrap()));
    }

    /// Every provenance failure lands on `Unusable`, never on `Unconfigured` —
    /// a tampered inventory must be stricter than an absent one.
    #[test]
    fn every_provenance_failure_fails_closed() {
        let key = signing_key();
        let doc = document();
        let good_sig = b64(key.sign(&doc).as_ref());
        let good_key = b64(key.public_key().as_ref());

        let cases: Vec<(&str, LegacyDigestInventory)> = vec![
            (
                "no key",
                LegacyDigestInventory::from_signed_document(&doc, None, Some(&good_sig)),
            ),
            (
                "no signature",
                LegacyDigestInventory::from_signed_document(&doc, Some(&good_key), None),
            ),
            (
                "key is not base64",
                LegacyDigestInventory::from_signed_document(&doc, Some("!!!"), Some(&good_sig)),
            ),
            (
                "key is the wrong length",
                LegacyDigestInventory::from_signed_document(
                    &doc,
                    Some(&b64(&[0u8; 16])),
                    Some(&good_sig),
                ),
            ),
            (
                "signature is the wrong length",
                LegacyDigestInventory::from_signed_document(
                    &doc,
                    Some(&good_key),
                    Some(&b64(&[0u8; 8])),
                ),
            ),
            (
                "signature is for a different key",
                LegacyDigestInventory::from_signed_document(
                    &doc,
                    Some(&b64(signing_key().public_key().as_ref())),
                    Some(&good_sig),
                ),
            ),
            (
                "document was tampered with after signing",
                LegacyDigestInventory::from_signed_document(
                    &[doc.clone(), b" ".to_vec()].concat(),
                    Some(&good_key),
                    Some(&good_sig),
                ),
            ),
        ];
        for (name, inventory) in cases {
            assert!(
                matches!(inventory, LegacyDigestInventory::Unusable(_)),
                "{name} must fail closed, got {inventory:?}"
            );
        }
    }

    #[test]
    fn a_signed_but_malformed_document_still_fails_closed() {
        let key = signing_key();
        for (name, body) in [
            (
                "unknown schema version",
                serde_json::json!({"schema_version": 2, "digests": []}),
            ),
            (
                "missing digests",
                serde_json::json!({"schema_version": 1, "issuer": "ops"}),
            ),
            (
                "a tag instead of a digest",
                serde_json::json!({"schema_version": 1, "digests": ["reg/img:tag"]}),
            ),
            (
                "an uppercase digest",
                serde_json::json!({"schema_version": 1, "digests": [digest('a').to_uppercase()]}),
            ),
        ] {
            let doc = serde_json::to_vec(&body).unwrap();
            let inventory = LegacyDigestInventory::from_signed_document(
                &doc,
                Some(&b64(key.public_key().as_ref())),
                Some(&b64(key.sign(&doc).as_ref())),
            );
            assert!(
                matches!(inventory, LegacyDigestInventory::Unusable(_)),
                "{name} must fail closed, got {inventory:?}"
            );
        }
    }
}
