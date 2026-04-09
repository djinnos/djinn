use super::super::fuzzy::{fuzzy_replace, reindent_replacement};
use super::*;

#[test]
fn rebases_multiline_replacement_using_matched_indentation() {
    let content = "fn main() {\n    match value {\n        Some(x) => {\n            process(x);\n        }\n    }\n}\n";
    let old_text = "match value {\n    Some(x) => {\n        process(x);\n    }\n}";
    let new_text = "match value {\n    Some(x) => {\n        if ready {\n            process(x);\n        }\n    }\n}";

    let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
        .expect("fuzzy replace should succeed");

    assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
    assert!(updated.contains(
        "    match value {\n        Some(x) => {\n            if ready {\n                process(x);\n            }\n        }\n    }"
    ));
}

#[test]
fn preserves_later_nested_indent_when_first_replacement_line_is_less_indented() {
    let content = "impl Example {\n        if condition {\n            run();\n        }\n}\n";
    let old_text = "if condition {\n    run();\n}";
    let new_text =
        "if condition {\n    let nested = || {\n        run();\n    };\n    nested();\n}";

    let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
        .expect("fuzzy replace should succeed");

    assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
    assert!(updated.contains(
        "        if condition {\n            let nested = || {\n                run();\n            };\n            nested();\n        }"
    ));
}

#[test]
fn reindent_replacement_preserves_internal_relative_indentation() {
    let matched_block = "        if ready {\n            execute();\n        }";
    let replacement =
        "if ready {\n    let nested = || {\n        execute();\n    };\n    nested();\n}";

    assert_eq!(
        reindent_replacement(matched_block, replacement),
        "        if ready {\n            let nested = || {\n                execute();\n            };\n            nested();\n        }"
    );
}
