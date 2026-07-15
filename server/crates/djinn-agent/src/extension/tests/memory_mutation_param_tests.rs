use super::{MemoryEditParams, MemoryWriteParams};

fn write_args(reason: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "reason": reason,
        "title": "A note",
        "content": "body",
        "type": "reference"
    })
}

fn edit_args(reason: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "reason": reason,
        "identifier": "reference/a-note",
        "operation": "append",
        "content": "more"
    })
}

#[test]
fn agent_mutation_decoders_require_nonblank_unicode_trimmed_reason() {
    let mut write_missing = write_args("placeholder".into());
    write_missing
        .as_object_mut()
        .expect("object")
        .remove("reason");
    assert!(serde_json::from_value::<MemoryWriteParams>(write_missing).is_err());
    let mut edit_missing = edit_args("placeholder".into());
    edit_missing
        .as_object_mut()
        .expect("object")
        .remove("reason");
    assert!(serde_json::from_value::<MemoryEditParams>(edit_missing).is_err());

    for reason in ["", "   ", "\u{2003}\u{00a0}\u{3000}"] {
        let write_error = serde_json::from_value::<MemoryWriteParams>(write_args(reason.into()))
            .err()
            .expect("blank write reason must be rejected");
        let edit_error = serde_json::from_value::<MemoryEditParams>(edit_args(reason.into()))
            .err()
            .expect("blank edit reason must be rejected");
        assert!(write_error.to_string().contains("reason must be non-blank"));
        assert!(edit_error.to_string().contains("reason must be non-blank"));
    }

    let write: MemoryWriteParams =
        serde_json::from_value(write_args("\u{2003}explain change\u{3000}".into()))
            .expect("padded write reason is valid");
    let edit: MemoryEditParams =
        serde_json::from_value(edit_args("\u{00a0}edit note\u{2003}".into()))
            .expect("padded edit reason is valid");
    assert_eq!(write.reason, "explain change");
    assert_eq!(edit.reason, "edit note");
}

#[test]
fn agent_mutation_decoders_reject_spoofed_and_arbitrary_fields() {
    for field in [
        "actor_id",
        "actor_role",
        "provenance",
        "task_run_id",
        "arbitrary",
    ] {
        let mut write = write_args("reason".into());
        write
            .as_object_mut()
            .expect("object")
            .insert(field.into(), "spoof".into());
        assert!(
            serde_json::from_value::<MemoryWriteParams>(write).is_err(),
            "write accepted {field}"
        );

        let mut edit = edit_args("reason".into());
        edit.as_object_mut()
            .expect("object")
            .insert(field.into(), "spoof".into());
        assert!(
            serde_json::from_value::<MemoryEditParams>(edit).is_err(),
            "edit accepted {field}"
        );
    }
}
