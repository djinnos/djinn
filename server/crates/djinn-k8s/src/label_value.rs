//! Kubernetes label-value and object-name budget helpers.
//!
//! Every string djinn stamps into `metadata.labels` (or into a
//! `metadata.name` that the apiserver then defaults into
//! `spec.template.labels`, as the Job controller does with `job-name`)
//! must satisfy the label-value grammar:
//!
//! - at most [`LABEL_VALUE_MAX_BYTES`] bytes;
//! - alphanumerics, `-`, `_`, `.` only;
//! - first and last characters alphanumeric (empty is also legal).
//!
//! Violating either half is a **422 at Job-create time**, which surfaces as
//! a workload that silently never runs — the apiserver rejects the POST, so
//! there is no Pod, no event, and no log beyond the dispatcher's own warning.
//! That is exactly how the graph warmer stalled: an 88-byte colon-separated
//! `work_id` and a 67-char Job name both failed validation on every tick
//! while the warmer looked healthy from the outside.
//!
//! Prefer constructing identifiers that are *natively* legal
//! ([`is_valid_label_value`] as an assertion) over sanitising at the stamp
//! site: admission work ids round-trip through labels back into journal keys,
//! so a lossy sanitiser would break reconciliation matching by making the
//! label-derived key differ from the durable one.

/// Maximum byte length of a Kubernetes label value.
pub const LABEL_VALUE_MAX_BYTES: usize = 63;

/// Is `value` a legal Kubernetes label value?
///
/// Mirrors apiserver validation (`IsValidLabelValue`): the empty string is
/// legal, otherwise `[A-Za-z0-9]([-A-Za-z0-9_.]*[A-Za-z0-9])?` within
/// [`LABEL_VALUE_MAX_BYTES`].
pub fn is_valid_label_value(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    if value.len() > LABEL_VALUE_MAX_BYTES {
        return false;
    }
    let bytes = value.as_bytes();
    let alnum = |b: u8| b.is_ascii_alphanumeric();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes
        .iter()
        .all(|b| alnum(*b) || *b == b'-' || *b == b'_' || *b == b'.')
}

/// Coerce `value` into a legal label value.
///
/// Illegal bytes become `-`, the result is truncated to
/// [`LABEL_VALUE_MAX_BYTES`], and non-alphanumeric edges are trimmed. This is
/// a defence-in-depth net for values that cannot be made natively legal — it
/// is **lossy and not injective**, so never use it on a value that has to
/// match a durable identity (see the module docs).
pub fn sanitize_label_value(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(LABEL_VALUE_MAX_BYTES);
    let trimmed = out.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_legal_values() {
        for value in [
            "",
            "a",
            "true",
            "graph-warm",
            "gw.019ea3bd-a305-73e3-806c-4edcc96ebfe2.f2d49f5a27dd",
            &"a".repeat(LABEL_VALUE_MAX_BYTES),
        ] {
            assert!(is_valid_label_value(value), "should accept: {value}");
        }
    }

    #[test]
    fn rejects_the_two_failure_modes_that_stalled_the_warmer() {
        // Over budget: the 67-char deterministic Job name, which the
        // apiserver defaults into `spec.template.labels[job-name]`.
        let long = "djinn-warm-019ea3bd-a305-73e3-806c-4edcc96ebfe2-g1-a29aafdb20df2fc9";
        assert_eq!(long.len(), 67);
        assert!(!is_valid_label_value(long));

        // Illegal character *and* over budget: the colon-separated work id.
        let work_id = "graph-warm:019ea3bd-a305-73e3-806c-4edcc96ebfe2:d6360bb71ebb0824da8c85b4633e582c879c983b";
        assert!(!is_valid_label_value(work_id));
        // Colons are illegal even well within the byte budget.
        assert!(!is_valid_label_value("graph-warm:abc"));
    }

    #[test]
    fn rejects_non_alphanumeric_edges() {
        assert!(!is_valid_label_value("-lead"));
        assert!(!is_valid_label_value("trail-"));
        assert!(!is_valid_label_value(".dot"));
    }

    #[test]
    fn sanitize_produces_valid_values() {
        for value in [
            "graph-warm:019ea3bd-a305-73e3-806c-4edcc96ebfe2:d6360bb71ebb0824da8c85b4633e582c879c983b",
            "-leading",
            "trailing-",
            "with spaces and/slashes",
            "::::",
        ] {
            let sanitized = sanitize_label_value(value);
            assert!(
                is_valid_label_value(&sanitized),
                "sanitize({value}) = {sanitized} is still invalid"
            );
        }
    }

    #[test]
    fn sanitize_is_identity_on_already_legal_values() {
        for value in ["true", "graph-warm", "gw.019ea3bd.f2d49f5a27dd", "1"] {
            assert_eq!(sanitize_label_value(value), value);
        }
    }
}
