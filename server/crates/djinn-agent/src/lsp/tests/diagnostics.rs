use crate::lsp::format_diagnostics_xml;

use super::make_diag;

#[test]
fn format_diagnostics_xml_empty() {
    assert_eq!(format_diagnostics_xml(vec![]), "");
}

#[test]
fn format_diagnostics_xml_filters_non_errors() {
    let diags = vec![
        make_diag("file://a.rs", 1, 1, 2, "warning"),
        make_diag("file://a.rs", 2, 1, 3, "info"),
        make_diag("file://a.rs", 3, 1, 4, "hint"),
    ];
    assert_eq!(format_diagnostics_xml(diags), "");
}

#[test]
fn format_diagnostics_xml_includes_errors() {
    let diags = vec![
        make_diag("file://a.rs", 10, 5, 1, "expected semicolon"),
        make_diag("file://a.rs", 20, 1, 2, "unused variable"),
    ];
    let xml = format_diagnostics_xml(diags);
    assert!(xml.contains("ERROR [10:5] expected semicolon"));
    assert!(!xml.contains("unused variable"));
}

#[test]
fn format_diagnostics_xml_groups_by_file() {
    let diags = vec![
        make_diag("file://b.rs", 1, 1, 1, "err b"),
        make_diag("file://a.rs", 1, 1, 1, "err a"),
    ];
    let xml = format_diagnostics_xml(diags);
    let a_pos = xml.find("file://a.rs").unwrap();
    let b_pos = xml.find("file://b.rs").unwrap();
    assert!(a_pos < b_pos);
}

#[test]
fn format_diagnostics_xml_truncates_files() {
    let diags: Vec<_> = (0..10)
        .map(|i| make_diag(&format!("file://f{i}.rs"), 1, 1, 1, "err"))
        .collect();
    let xml = format_diagnostics_xml(diags);
    let file_count = xml.matches("<diagnostics file=").count();
    assert_eq!(file_count, 5);
}

#[test]
fn format_diagnostics_xml_truncates_per_file() {
    let diags: Vec<_> = (0..30)
        .map(|i| make_diag("file://a.rs", i, 1, 1, &format!("err {i}")))
        .collect();
    let xml = format_diagnostics_xml(diags);
    let error_count = xml.matches("ERROR").count();
    assert_eq!(error_count, 20);
}
