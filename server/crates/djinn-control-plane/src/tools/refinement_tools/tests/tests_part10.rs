use super::*;
// Human-readable admission rejections.
//
// `RefinementAdmissionError::AlreadyActive` used to be rendered as the literal
// Rust variant name `"AlreadyActive"`, which is what the user saw in an error
// toast after clicking "Send feedback for another round" on proposal y6q4.
// Every variant must now render a sentence that says what happened and what to
// do, with the machine-readable classification kept as a separate `code` field.

/// Assert on the actual strings: a message that is not a sentence, or that
/// leaks a Rust identifier, is the defect this test exists to catch.
fn assert_is_human_readable(rejection: &AdmissionRejection) {
    let message = &rejection.message;
    assert!(
        message.contains(' '),
        "{:?} is not a sentence: {message:?}",
        rejection.code
    );
    assert!(
        message.ends_with('.'),
        "{:?} message must be a complete sentence: {message:?}",
        rejection.code
    );
    for identifier in [
        "AlreadyActive",
        "AdmissionConflict",
        "GenerationConflict",
        "ProposalNotFound",
        "InvalidRequest",
        "RefinementAdmissionError",
    ] {
        assert!(
            !message.contains(identifier),
            "{:?} message leaks the Rust identifier {identifier}: {message:?}",
            rejection.code
        );
    }
    assert!(
        rejection
            .code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_'),
        "code must be a stable snake_case token, got {:?}",
        rejection.code
    );
}

#[test]
fn every_admission_error_variant_renders_a_human_readable_message() {
    let variants = vec![
        RefinementAdmissionError::AlreadyActive {
            proposal_id: "p-1".into(),
            run_id: "r-1".into(),
        },
        RefinementAdmissionError::GenerationConflict {
            proposal_id: "p-1".into(),
            generation: 3,
        },
        RefinementAdmissionError::AdmissionConflict,
        RefinementAdmissionError::Database(djinn_db::Error::InvalidData("boom".into())),
        RefinementAdmissionError::ProposalNotFound {
            proposal_id: "p-1".into(),
        },
        RefinementAdmissionError::InvalidRequest("empty idempotency key".into()),
    ];
    let mut codes = Vec::new();
    for variant in &variants {
        let rejection = admission_rejection(variant);
        assert_is_human_readable(&rejection);
        codes.push(rejection.code);
    }
    codes.sort_unstable();
    let mut unique = codes.clone();
    unique.dedup();
    assert_eq!(codes, unique, "each variant needs its own machine code");
}

#[test]
fn already_active_tells_the_user_what_happened_and_what_to_do() {
    let rejection = admission_rejection(&RefinementAdmissionError::AlreadyActive {
        proposal_id: "019fa0bb-6174-7462-859c-9f0a5530e88c".into(),
        run_id: "run-1".into(),
    });
    assert_eq!(rejection.code, "already_active");
    assert_eq!(
        rejection.message,
        "A tribunal round is already running for this proposal. \
         Wait for it to finish (or stop it) before starting another."
    );
}

#[test]
fn proposal_not_found_names_the_proposal() {
    let rejection = admission_rejection(&RefinementAdmissionError::ProposalNotFound {
        proposal_id: "y6q4".into(),
    });
    assert_eq!(rejection.code, "proposal_not_found");
    assert!(rejection.message.contains("y6q4"), "{}", rejection.message);
}

#[test]
fn invalid_request_carries_the_underlying_detail() {
    let rejection = admission_rejection(&RefinementAdmissionError::InvalidRequest(
        "no targets".into(),
    ));
    assert_eq!(rejection.code, "invalid_request");
    assert!(
        rejection.message.contains("no targets"),
        "{}",
        rejection.message
    );
}
