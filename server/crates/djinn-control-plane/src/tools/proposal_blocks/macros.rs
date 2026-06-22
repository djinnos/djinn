//! Field-schema builder helpers and the `define_blocks!` macro.
//!
//! `define_blocks!` collapses the per-block triplication (the `BlockType` enum
//! variant + its `as_str()`/`tag()` match arms, the `PROPOSAL_BLOCK_REGISTRY`
//! entry, and the canonical parity list) into a single declaration per block.
//! Field schemas are built with the small `*_field`/`fields` helpers; irregular
//! nested schemas use the macro's `nested(<expr>)` escape, which accepts a
//! hand-written [`ProposalBlockFieldSchema`] expression verbatim.

use std::collections::BTreeMap;

use super::types::ProposalBlockFieldSchema;

pub(super) fn string_field() -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: None,
        fields: None,
        items: None,
    }
}


pub(super) fn enum_string_field(values: Vec<&'static str>) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: Some(values),
        fields: None,
        items: None,
    }
}

pub(super) fn object_field(
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "object",
        enum_values: None,
        fields: Some(fields),
        items: None,
    }
}

pub(super) fn array_field(items: ProposalBlockFieldSchema) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "array",
        enum_values: None,
        fields: None,
        items: Some(Box::new(items)),
    }
}

pub(super) fn fields(
    entries: Vec<(&'static str, ProposalBlockFieldSchema)>,
) -> BTreeMap<&'static str, ProposalBlockFieldSchema> {
    entries.into_iter().collect()
}

/// Expand a single list of block specs into:
///   * the `BlockType` enum + its `as_str()` / `tag()` match arms,
///   * the `PROPOSAL_BLOCK_REGISTRY` (a `LazyLock<BTreeMap<…>>`), and
///   * `CANONICAL_BLOCK_TYPES: &[(&str, &str)]` (`(type_str, tag)`) for the
///     parity tests.
///
/// Per-block grammar (commas separate blocks):
///
/// ```ignore
/// define_blocks! {
///     Variant => "type-str", "Tag" [, desc = "…authoring guidance…"],
///         fields { "name" => <field-expr>, ... },
///     ...
/// }
/// ```
///
/// where `<field-expr>` is any of `string`, `enum [..]`, or
/// `nested(<ProposalBlockFieldSchema expression>)` for irregular nested shapes
/// the macro grammar does not model directly.
macro_rules! define_blocks {
    (
        $(
            $variant:ident => $type_str:literal, $tag:literal
            $(, desc = $desc:literal)?,
            fields { $( $fname:literal => $fval:tt ),* $(,)? }
        ),* $(,)?
    ) => {
        /// Stable v1 proposal block types.
        #[derive(Debug, Clone, PartialEq, Eq, ::serde::Serialize, ::serde::Deserialize)]
        pub enum BlockType {
            $($variant),*
        }

        impl BlockType {
            /// Return the kebab-case block type identifier used in the registry.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(BlockType::$variant => $type_str),*
                }
            }

            /// Return the MDX component tag name for this block type.
            pub fn tag(&self) -> &'static str {
                match self {
                    $(BlockType::$variant => $tag),*
                }
            }
        }

        /// Canonical `(type_str, tag)` pairs in declaration order. Single source
        /// of truth for the Rust-side parity tests.
        pub static CANONICAL_BLOCK_TYPES: &[(&str, &str)] = &[
            $( ($type_str, $tag) ),*
        ];

        pub static PROPOSAL_BLOCK_REGISTRY: ::std::sync::LazyLock<
            ::std::collections::BTreeMap<&'static str, $crate::tools::proposal_blocks::ProposalBlockDefinition>,
        > = ::std::sync::LazyLock::new(|| {
            ::std::collections::BTreeMap::from([
                $((
                    $type_str,
                    $crate::tools::proposal_blocks::ProposalBlockDefinition {
                        block_type: $type_str,
                        tag: $tag,
                        description: $crate::tools::proposal_blocks::macros::__opt_desc!($($desc)?),
                        fields: $crate::tools::proposal_blocks::macros::fields(vec![
                            $( ($fname, define_blocks!(@field $fval)) ),*
                        ]),
                    },
                )),*
            ])
        });
    };

    // ── field-expression sub-rules ──────────────────────────────────────────
    (@field string) => {
        $crate::tools::proposal_blocks::macros::string_field()
    };
    (@field (enum [ $($v:literal),* $(,)? ])) => {
        $crate::tools::proposal_blocks::macros::enum_string_field(vec![ $($v),* ])
    };
    (@field (nested($expr:expr))) => {
        $expr
    };
}

/// Expand an optional `desc = "…"` into `Some("…")`, or to `None` when absent.
macro_rules! __opt_desc {
    () => {
        None
    };
    ($desc:literal) => {
        Some($desc)
    };
}

pub(crate) use __opt_desc;
pub(crate) use define_blocks;
