#[cfg(test)]
mod detect_changes_helper_tests {
    use super::super::shared;
    use djinn_control_plane::bridge::PagerankTier;

    #[test]
    fn bucket_pagerank_uses_q33_q67() {
        let thresholds = (0.10, 0.20);
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.05),
            PagerankTier::Low
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.10),
            PagerankTier::Medium
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.15),
            PagerankTier::Medium
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.20),
            PagerankTier::High
        );
        assert_eq!(
            shared::bucket_pagerank(&thresholds, 0.99),
            PagerankTier::High
        );
    }

    #[test]
    fn tier_rank_orders_high_first() {
        assert!(shared::tier_rank(PagerankTier::High) < shared::tier_rank(PagerankTier::Medium));
        assert!(shared::tier_rank(PagerankTier::Medium) < shared::tier_rank(PagerankTier::Low));
    }

    #[test]
    fn quartile_thresholds_handles_empty_ranking() {
        let ranking = djinn_graph::repo_graph::RepoGraphRanking { nodes: vec![] };
        assert_eq!(shared::quartile_thresholds(&ranking), (0.0, 0.0));
    }
}

#[cfg(test)]
mod helper_tests {
    use super::super::shared;

    #[test]
    fn scip_crate_name_extracts_cargo_package() {
        let sym = "scip-rust cargo my-crate 0.1.0 foo/Bar#";
        assert_eq!(shared::scip_crate_name(sym), Some("my-crate"));
    }

    #[test]
    fn scip_crate_name_extracts_go_module() {
        let sym = "scip-go gomod github.com/acme/foo v1 pkg/Thing#";
        assert_eq!(shared::scip_crate_name(sym), Some("github.com/acme/foo"));
    }

    #[test]
    fn scip_crate_name_returns_none_for_short_input() {
        assert_eq!(shared::scip_crate_name(""), None);
        assert_eq!(shared::scip_crate_name("scip-rust"), None);
        assert_eq!(shared::scip_crate_name("scip-rust cargo"), None);
        assert_eq!(shared::scip_crate_name("scip-rust cargo pkg"), None);
    }

    #[test]
    fn scip_crate_name_skips_locals_and_dot_placeholder() {
        // Local symbols have no crate identity.
        assert_eq!(shared::scip_crate_name("local 42"), None);
        // Some SCIP scheme/manager slots use "." when missing — and
        // the package slot does the same. In that case we have no
        // identity to compare against.
        let sym = "scip-rust cargo . 0.1.0 foo/Bar#";
        assert_eq!(shared::scip_crate_name(sym), None);
    }

    #[test]
    fn is_deprecated_text_matches_rust_attribute() {
        assert!(shared::is_deprecated_text(
            Some("#[deprecated] fn foo()"),
            &[]
        ));
        assert!(shared::is_deprecated_text(
            Some(r#"#[deprecated(since = "0.1", note = "use bar")] fn foo()"#),
            &[]
        ));
    }

    #[test]
    fn is_deprecated_text_matches_jsdoc_marker_case_insensitive() {
        let doc = vec!["/**".into(), " * @Deprecated use `bar` instead".into()];
        assert!(shared::is_deprecated_text(None, &doc));
    }

    #[test]
    fn is_deprecated_text_ignores_unrelated_text() {
        let doc = vec!["A documented symbol.".into()];
        assert!(!shared::is_deprecated_text(Some("fn foo()"), &doc));
    }
}
