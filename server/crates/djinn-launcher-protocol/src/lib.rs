//! The canonical launcher quota-authority protocol, and its wire form.
//!
//! # Why this is its own crate
//!
//! Three components have to agree on exactly two strings — `leaf-v1` and
//! `resize-v2`:
//!
//! * `djinn-cgroup-launcher`, a **static sidecar binary** shipped into every
//!   per-project image. It decides whether the launcher writes leaf `cpu.max`
//!   at all, so it must be able to name the protocol; it must NOT be able to
//!   reach a database client, a Kubernetes client, or a runtime to get there.
//! * `djinn-db`, which persists `observed_launcher_protocol` and
//!   `effective_launcher_protocol` under a migration-164 `CHECK ... IN` list.
//! * The admission/reconcile plane, which compares the two.
//!
//! Putting the type where the database constants live would make the sidecar
//! depend on `djinn-db`. Putting it in the launcher would make `djinn-db`
//! depend on the launcher. So it lives here, in a leaf that depends on nothing
//! but `serde` (without `derive`).
//!
//! # Why one definition and not two
//!
//! PR #2800 shipped a launcher-local copy of this decision with no wire form at
//! all, while `djinn-db` carried the strings inline in two `matches!` arms. The
//! two could not be shown to agree, and nothing forced them to. Every consumer
//! now routes through [`LauncherAuthorityProtocol::as_wire`] and
//! [`LauncherAuthorityProtocol::from_str`], and a workspace-wide scan
//! (`exactly_one_definition_of_the_protocol_exists_workspace_wide`) fails if a
//! second definition reappears.

use std::fmt;
use std::str::FromStr;

/// Wire form of [`LauncherAuthorityProtocol::LeafV1`]. Also the migration-164
/// `CHECK` literal and the Job-rendered environment value.
pub const LEAF_V1_WIRE: &str = "leaf-v1";

/// Wire form of [`LauncherAuthorityProtocol::ResizeV2`].
pub const RESIZE_V2_WIRE: &str = "resize-v2";

/// The component that owns an invocation leaf's CPU quota.
///
/// This is intentionally independent from the per-invocation armed/unarmed
/// lease decision (`djinn_cgroup_launcher::LeaseAuthority`). Keeping the axes
/// separate prevents a `resize-v2` leaf from inheriting a `leaf-v1` quota
/// mutation.
///
/// [`Self::LeafV1`] is the default because it is the behavior that predates the
/// protocol existing: absent an explicit selection, the launcher keeps doing
/// what it has always done. A *malformed* selection is never this default — see
/// [`FromStr`], which has no fallback arm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum LauncherAuthorityProtocol {
    /// The launcher owns per-leaf quota birth and fenced lift writes.
    #[default]
    LeafV1,
    /// Pod resize owns quota. The launcher must never write leaf `cpu.max`.
    ResizeV2,
}

impl LauncherAuthorityProtocol {
    /// Every variant, in wire-stable order.
    ///
    /// Exposed so agreement tests can iterate the type rather than restate its
    /// members: adding a variant without teaching the database about it is
    /// exactly the failure this array makes detectable.
    pub const ALL: [Self; 2] = [Self::LeafV1, Self::ResizeV2];

    /// The exact string persisted, rendered into the Job environment, and
    /// checked by migration 164. Never localized, never re-cased.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::LeafV1 => LEAF_V1_WIRE,
            Self::ResizeV2 => RESIZE_V2_WIRE,
        }
    }

    /// Stable single-byte encoding for the broker's length-prefixed socket
    /// frames, which are byte-oriented (see `djinn_cgroup_launcher::transport`).
    ///
    /// `0` is deliberately unassigned: the neighbouring `LeaseAuthority::wire`
    /// uses `0` for `Unarmed`, and a zeroed or truncated frame must not decode
    /// as a valid protocol on either axis.
    pub const fn frame_byte(self) -> u8 {
        match self {
            Self::LeafV1 => 1,
            Self::ResizeV2 => 2,
        }
    }

    /// Decode [`Self::frame_byte`]. `None` for every unassigned byte, including
    /// `0`; callers must treat that as an invalid frame, not as a default.
    pub const fn from_frame_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::LeafV1),
            2 => Some(Self::ResizeV2),
            _ => None,
        }
    }

    /// Does the launcher write this leaf's `cpu.max`?
    ///
    /// Under `resize-v2` the answer is no — not even `max`, which is itself a
    /// launcher mutation of a value Pod resize owns.
    pub const fn launcher_owns_leaf_quota(self) -> bool {
        matches!(self, Self::LeafV1)
    }
}

impl fmt::Display for LauncherAuthorityProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_wire())
    }
}

/// A string that is not exactly one of the two wire forms.
///
/// Carries the offending input so a fail-closed caller can name it in the error
/// it exits on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseLauncherAuthorityProtocolError {
    input: String,
}

impl ParseLauncherAuthorityProtocolError {
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseLauncherAuthorityProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} is not a launcher authority protocol (expected {LEAF_V1_WIRE:?} or {RESIZE_V2_WIRE:?})",
            self.input
        )
    }
}

impl std::error::Error for ParseLauncherAuthorityProtocolError {}

impl FromStr for LauncherAuthorityProtocol {
    type Err = ParseLauncherAuthorityProtocolError;

    /// Exact match only.
    ///
    /// No trimming, no case folding and — critically — **no fallback arm**. The
    /// protocol decides whether a build's CPU quota is governed by the launcher
    /// or by Pod resize; silently reading an unrecognised value as either one
    /// is a production outage in one direction and a 16x slowdown in the other.
    /// A caller that cannot parse must fail, not default.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            LEAF_V1_WIRE => Ok(Self::LeafV1),
            RESIZE_V2_WIRE => Ok(Self::ResizeV2),
            other => Err(ParseLauncherAuthorityProtocolError {
                input: other.to_owned(),
            }),
        }
    }
}

impl serde::Serialize for LauncherAuthorityProtocol {
    /// Delegates to [`LauncherAuthorityProtocol::as_wire`] rather than deriving,
    /// so the serialized form cannot drift from the persisted one.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> serde::Deserialize<'de> for LauncherAuthorityProtocol {
    /// Delegates to [`FromStr`], so serde inherits the same closed set and the
    /// same refusal to fall back.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct WireVisitor;

        impl serde::de::Visitor<'_> for WireVisitor {
            type Value = LauncherAuthorityProtocol;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{LEAF_V1_WIRE:?} or {RESIZE_V2_WIRE:?}")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_str(WireVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The closed set, asserted from both ends.
    ///
    /// The rejected inputs are the near-misses a human or a Job template
    /// actually produces: a dropped separator, the wrong case, the next version
    /// that does not exist yet, and whitespace a shell or a YAML block scalar
    /// left behind.
    #[test]
    fn from_str_accepts_exactly_the_two_wire_forms_and_rejects_every_near_miss() {
        assert_eq!(
            "leaf-v1".parse::<LauncherAuthorityProtocol>(),
            Ok(LauncherAuthorityProtocol::LeafV1)
        );
        assert_eq!(
            "resize-v2".parse::<LauncherAuthorityProtocol>(),
            Ok(LauncherAuthorityProtocol::ResizeV2)
        );

        // Accumulated rather than asserted case by case, so a fallback arm
        // reports EVERY input it would swallow instead of stopping at the
        // first. The task's non-vacuity bar is "at least three cases fail";
        // this makes the count visible in the failure itself.
        let mut swallowed = Vec::new();
        for rejected in [
            "",
            " ",
            "leafv1",
            "LEAF-V1",
            "Leaf-V1",
            "leaf_v1",
            "resize-v3",
            "resizev2",
            "RESIZE-V2",
            " leaf-v1",
            "leaf-v1 ",
            "\tresize-v2",
            "resize-v2\n",
            "leaf-v1\0",
            "leaf-v1,resize-v2",
            "unknown-v3",
        ] {
            match rejected.parse::<LauncherAuthorityProtocol>() {
                Err(refusal) => assert_eq!(
                    refusal.input(),
                    rejected,
                    "the refusal must name the offending input so a fail-closed caller can log it"
                ),
                Ok(accepted) => swallowed.push((rejected, accepted)),
            }
        }
        assert!(
            swallowed.is_empty(),
            "{} inputs were silently accepted as a protocol: {swallowed:?}",
            swallowed.len()
        );
    }

    /// Every variant round-trips through its own wire form, and the wire forms
    /// are the literals the database and the Job template use.
    #[test]
    fn every_variant_round_trips_through_its_wire_form() {
        for protocol in LauncherAuthorityProtocol::ALL {
            assert_eq!(
                protocol.as_wire().parse::<LauncherAuthorityProtocol>(),
                Ok(protocol)
            );
            assert_eq!(protocol.to_string(), protocol.as_wire());
        }
        assert_eq!(
            LauncherAuthorityProtocol::ALL
                .map(LauncherAuthorityProtocol::as_wire)
                .to_vec(),
            vec!["leaf-v1", "resize-v2"],
        );
    }

    /// The frame byte is an encoding, not a default: a zeroed or truncated
    /// frame must decode to nothing at all.
    #[test]
    fn the_frame_byte_is_stable_and_zero_decodes_to_nothing() {
        assert_eq!(LauncherAuthorityProtocol::LeafV1.frame_byte(), 1);
        assert_eq!(LauncherAuthorityProtocol::ResizeV2.frame_byte(), 2);
        for protocol in LauncherAuthorityProtocol::ALL {
            assert_eq!(
                LauncherAuthorityProtocol::from_frame_byte(protocol.frame_byte()),
                Some(protocol)
            );
        }
        for unassigned in [0u8, 3, 4, 127, 255] {
            assert_eq!(
                LauncherAuthorityProtocol::from_frame_byte(unassigned),
                None,
                "byte {unassigned} must not decode to a protocol"
            );
        }
    }

    /// serde is the persistence/transport form, so it must be the same two
    /// strings and the same closed set — not a derived `"LeafV1"`.
    #[test]
    fn serde_is_the_wire_string_and_inherits_the_closed_set() {
        for protocol in LauncherAuthorityProtocol::ALL {
            let json = serde_json::to_string(&protocol).unwrap();
            assert_eq!(json, format!("{:?}", protocol.as_wire()));
            assert_eq!(
                serde_json::from_str::<LauncherAuthorityProtocol>(&json).unwrap(),
                protocol
            );
        }
        for rejected in [
            "\"\"",
            "\"LeafV1\"",
            "\"resize-v3\"",
            "\"leaf-v1 \"",
            "1",
            "2",
        ] {
            assert!(
                serde_json::from_str::<LauncherAuthorityProtocol>(rejected).is_err(),
                "{rejected} must not deserialize into a protocol"
            );
        }
    }

    /// `leaf-v1` is the only protocol under which the launcher touches leaf
    /// `cpu.max`. This is the predicate the birth and lift writes are gated on.
    #[test]
    fn only_leaf_v1_lets_the_launcher_write_leaf_quota() {
        assert!(LauncherAuthorityProtocol::LeafV1.launcher_owns_leaf_quota());
        assert!(!LauncherAuthorityProtocol::ResizeV2.launcher_owns_leaf_quota());
        assert_eq!(
            LauncherAuthorityProtocol::default(),
            LauncherAuthorityProtocol::LeafV1,
            "the default must be the pre-existing behavior, never the new one"
        );
    }

    /// One canonical definition, or the whole point of this crate is lost.
    ///
    /// A second copy is exactly how #2800 shipped: the launcher had its own
    /// type with no wire form, `djinn-db` had the strings inline, and nothing
    /// made them agree. This scans the checkout rather than trusting a comment.
    ///
    /// The needle is assembled at compile time so this file does not match
    /// itself.
    #[test]
    fn exactly_one_definition_of_the_protocol_exists_workspace_wide() {
        const NEEDLE: &str = concat!("enum ", "LauncherAuthorityProtocol");

        let root = repository_root();
        let mut hits = Vec::new();
        collect_rust_sources(&root, &mut |path, contents| {
            if contents.contains(NEEDLE) {
                hits.push(path.to_path_buf());
            }
        });

        assert_eq!(
            hits.len(),
            1,
            "expected exactly one definition of the protocol, found {hits:#?}"
        );
        assert!(
            hits[0].ends_with("djinn-launcher-protocol/src/lib.rs"),
            "the one definition must be this crate's, found {:?}",
            hits[0]
        );
    }

    /// `<checkout>/server/crates/djinn-launcher-protocol` → `<checkout>`.
    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("the manifest always sits three levels below the checkout root")
            .to_path_buf()
    }

    /// Depth-first walk of every `.rs` file under `root`, skipping build output
    /// and nested checkouts. Written against `std` only: this crate has no
    /// `walkdir` and is not going to acquire one.
    fn collect_rust_sources(root: &Path, visit: &mut impl FnMut(&Path, &str)) {
        const SKIP: &[&str] = &[
            "target",
            ".git",
            "node_modules",
            ".worktrees",
            ".claude",
            "dist",
            "vendor",
        ];

        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if !SKIP.contains(&name.as_ref()) {
                    collect_rust_sources(&path, visit);
                }
            } else if name.ends_with(".rs")
                && let Ok(contents) = std::fs::read_to_string(&path)
            {
                visit(&path, &contents);
            }
        }
    }
}
