//! Tests for the truncation utilities.
//!
//! Moved from `djinn-agent::extension::tests::lsp_dispatch_tests` during
//! the Phase 4 extraction — these test `crate::truncate::floor_char_boundary`
//! directly.

use crate::truncate::floor_char_boundary;

#[test]
fn floor_char_boundary_ascii() {
    assert_eq!(floor_char_boundary("hello", 3), 3);
}

#[test]
fn floor_char_boundary_multibyte_interior() {
    // '─' (U+2500) is 3 bytes: E2 94 80
    let s = "─";
    assert_eq!(floor_char_boundary(s, 1), 0);
    assert_eq!(floor_char_boundary(s, 2), 0);
    assert_eq!(floor_char_boundary(s, 3), 3);
}

#[test]
fn floor_char_boundary_emoji() {
    // '🔥' is 4 bytes
    let s = "🔥x";
    assert_eq!(floor_char_boundary(s, 1), 0);
    assert_eq!(floor_char_boundary(s, 2), 0);
    assert_eq!(floor_char_boundary(s, 3), 0);
    assert_eq!(floor_char_boundary(s, 4), 4);
    assert_eq!(floor_char_boundary(s, 5), 5);
}

#[test]
fn floor_char_boundary_beyond_len() {
    assert_eq!(floor_char_boundary("hi", 100), 2);
}

#[test]
fn floor_char_boundary_zero() {
    assert_eq!(floor_char_boundary("hello", 0), 0);
}
