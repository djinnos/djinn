use super::*;

fn complete_config(
    first_id: &str,
    first_path: &Path,
    second_id: &str,
    second_path: &Path,
) -> VerificationInputFingerprintConfig {
    let mut config = VerificationInputFingerprintConfig::default();
    for (id, locator, path) in [
        (first_id, format!("host://{first_id}"), first_path),
        (second_id, format!("host://{second_id}"), second_path),
    ] {
        config.manifest.read_only_external_inputs.push(
            djinn_core::canonical_verify::DeclaredExternalInputV1 {
                id: id.to_owned(),
                locator,
            },
        );
        config.external_inputs.push(ResolvedExternalInputV1 {
            id: id.to_owned(),
            path: path.to_path_buf(),
        });
    }
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_complete_v1_stream_frames_gitlinks_and_externals() {
    let fixture = make_nested_submodule_fixture("vendor", "nested");
    let mounts = tempfile::tempdir().expect("create external mount parent");
    let alpha = mounts.path().join("alpha");
    let zeta = mounts.path().join("zeta");
    write_str(&alpha, "a/first.txt", "alpha-first\n");
    write_str(&alpha, "z/last.txt", "alpha-last\n");
    write_str(&zeta, "a/first.txt", "zeta-first\n");
    write_str(&zeta, "z/last.txt", "zeta-last\n");
    let config = complete_config("alpha", &alpha, "zeta", &zeta);

    // The configured public API is the subject under test. Rebuild its complete
    // byte stream independently below as a framing golden: this pins the two
    // gitlink/external sections that the historical header-only check omitted.
    let actual = digest(configured_fingerprint(fixture.outer.path(), &config).await);
    let head = try_rev_parse(fixture.outer.path(), "HEAD")
        .await
        .unwrap()
        .unwrap();
    let resolved_base = resolve_base_ref(fixture.outer.path(), "main")
        .await
        .unwrap()
        .unwrap();
    let merge_base = try_merge_base(fixture.outer.path(), &resolved_base)
        .await
        .unwrap()
        .unwrap();
    let index_output = git_binary_stdout(
        fixture.outer.path(),
        vec!["ls-files".into(), "-s".into(), "-z".into()],
    )
    .await
    .unwrap();
    let mut index_entries = parse_index_entries(&index_output);
    let anchor = PermittedRootAnchor::capture(fixture.outer.path(), b".").unwrap();
    let mut tracked_states = Vec::new();
    let mut gitlink_states = Vec::new();
    for entry in &index_entries {
        if entry.mode == MODE_GITLINK_TAG {
            gitlink_states.push(
                collect_gitlink_state(&anchor, &entry.path, &entry.blob_sha)
                    .await
                    .unwrap(),
            );
        } else {
            tracked_states.push(classify_worktree_entry(&anchor, &entry.path, false).unwrap());
        }
    }
    let mut extra_states = Vec::new();
    for path in collect_extra_paths(fixture.outer.path()).await.unwrap() {
        extra_states.push(classify_worktree_entry(&anchor, &path, true).unwrap());
    }
    index_entries.sort_by(|a, b| a.path.cmp(&b.path));
    tracked_states.sort_by(|a, b| a.path.cmp(&b.path));
    gitlink_states.sort_by(|a, b| a.path.cmp(&b.path));
    extra_states.sort_by(|a, b| a.path.cmp(&b.path));
    let external_states = collect_external_states(&config).unwrap();
    assert_eq!(
        gitlink_states.len(),
        1,
        "fixture must frame its outer gitlink"
    );
    assert!(
        gitlink_states[0]
            .submodule_stream
            .windows(b"nested".len())
            .any(|window| window == b"nested"),
        "outer gitlink payload must contain the nested gitlink stream"
    );
    assert_eq!(
        external_states.len(),
        4,
        "fixture must frame both external mounts"
    );

    fn field(bytes: &mut Vec<u8>, value: &[u8]) {
        bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
        bytes.extend_from_slice(value);
    }
    fn states(bytes: &mut Vec<u8>, values: &[WorktreeState]) {
        bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for value in values {
            field(bytes, &value.path);
            field(bytes, value.type_tag);
            field(bytes, value.mode_tag);
            field(bytes, &value.content);
        }
    }

    let mut golden = Vec::new();
    field(&mut golden, STREAM_MAGIC);
    field(&mut golden, STREAM_VERSION_TAG);
    golden.extend_from_slice(&VERIFICATION_INPUT_FINGERPRINT_VERSION_V1.to_le_bytes());
    field(&mut golden, merge_base.as_bytes());
    field(&mut golden, head.as_bytes());
    golden.extend_from_slice(&(index_entries.len() as u64).to_le_bytes());
    for entry in &index_entries {
        field(&mut golden, &entry.path);
        field(&mut golden, &entry.mode);
        golden.extend_from_slice(&entry.stage.to_le_bytes());
        field(&mut golden, entry.blob_sha.as_bytes());
    }
    states(&mut golden, &tracked_states);
    golden.extend_from_slice(&(gitlink_states.len() as u64).to_le_bytes());
    for gitlink in &gitlink_states {
        field(&mut golden, &gitlink.path);
        field(&mut golden, gitlink.committed_sha.as_bytes());
        field(&mut golden, &gitlink.submodule_stream);
    }
    states(&mut golden, &extra_states);
    golden.extend_from_slice(&(external_states.len() as u64).to_le_bytes());
    for external in &external_states {
        field(&mut golden, &external.id);
        field(&mut golden, &external.locator);
        field(&mut golden, &external.path);
        field(&mut golden, external.state.type_tag);
        field(&mut golden, external.state.mode_tag);
        field(&mut golden, &external.state.content);
    }
    assert_eq!(actual.canonical_stream_len, golden.len() as u64);
    assert_eq!(actual.fingerprint, sha256_hex(&golden));

    let submission_before = crate::compute_submission_diff_fingerprint(fixture.outer.path())
        .await
        .expect("compute submission fingerprint before external mutation");
    write_str(&alpha, "a/first.txt", "alpha-mutated\n");
    let changed = digest(configured_fingerprint(fixture.outer.path(), &config).await);
    let submission_after = crate::compute_submission_diff_fingerprint(fixture.outer.path())
        .await
        .expect("compute submission fingerprint after external mutation");
    assert_ne!(actual.fingerprint, changed.fingerprint);
    assert_eq!(
        submission_before, submission_after,
        "external manifest inputs must not change submission-diff fingerprint behavior"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_complete_v1_matrix_is_canonical_and_mutation_sensitive() {
    let fixture = make_nested_submodule_fixture("vendor", "nested");
    write_str(fixture.outer.path(), ".gitignore", "*.generated\n");
    git(fixture.outer.path(), ["add", ".gitignore"]);
    git(
        fixture.outer.path(),
        ["commit", "-m", "ignore generated input"],
    );
    write_str(fixture.outer.path(), "input.generated", "generated-v1\n");
    write(fixture.outer.path(), "input.bin", &[0, 1, 255, 254]);

    let mounts = tempfile::tempdir().expect("create external mount parent");
    let alpha = mounts.path().join("alpha");
    let zeta = mounts.path().join("zeta");
    std::fs::create_dir_all(&alpha).expect("create alpha mount");
    std::fs::create_dir_all(&zeta).expect("create zeta mount");
    // Deliberately create each tree in reverse bytewise order.
    write_str(&alpha, "z/last.txt", "alpha-last\n");
    write_str(&alpha, "a/first.txt", "alpha-first\n");
    write_str(&zeta, "z/last.txt", "zeta-last\n");
    write_str(&zeta, "a/first.txt", "zeta-first\n");

    let reversed_mounts = tempfile::tempdir().expect("create reverse external mount parent");
    let reverse_zeta = reversed_mounts.path().join("zeta");
    let reverse_alpha = reversed_mounts.path().join("alpha");
    // This equivalent fixture reverses both mount creation and child insertion.
    std::fs::create_dir_all(&reverse_zeta).expect("create reverse zeta mount");
    std::fs::create_dir_all(&reverse_alpha).expect("create reverse alpha mount");
    write_str(&reverse_zeta, "a/first.txt", "zeta-first\n");
    write_str(&reverse_zeta, "z/last.txt", "zeta-last\n");
    write_str(&reverse_alpha, "a/first.txt", "alpha-first\n");
    write_str(&reverse_alpha, "z/last.txt", "alpha-last\n");

    let ordered = complete_config("alpha", &alpha, "zeta", &zeta);
    let reversed = complete_config("zeta", &zeta, "alpha", &alpha);
    let reverse_tree = complete_config("zeta", &reverse_zeta, "alpha", &reverse_alpha);
    let baseline = digest(configured_fingerprint(fixture.outer.path(), &ordered).await);
    let reordered = digest(configured_fingerprint(fixture.outer.path(), &reversed).await);
    let reverse_tree_digest =
        digest(configured_fingerprint(fixture.outer.path(), &reverse_tree).await);
    assert_eq!(
        baseline.fingerprint, reordered.fingerprint,
        "manifest declaration and external enumeration order must not affect the complete V1 stream"
    );
    assert_eq!(
        baseline.fingerprint, reverse_tree_digest.fingerprint,
        "external mount and child creation order must not affect the complete V1 stream"
    );

    write_str(fixture.outer.path(), "input.generated", "generated-v2\n");
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "ignored generated input must affect the configured public API digest"
    );
    write_str(fixture.outer.path(), "input.generated", "generated-v1\n");
    write(fixture.outer.path(), "input.bin", &[0, 2, 255, 254]);
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "untracked binary bytes must affect the configured public API digest"
    );
    write(fixture.outer.path(), "input.bin", &[0, 1, 255, 254]);

    write_str(
        &fixture.outer.path().join("vendor"),
        "local.txt",
        "submodule dirty\n",
    );
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "submodule dirtiness must affect the configured public API digest"
    );
    std::fs::remove_file(fixture.outer.path().join("vendor/local.txt")).expect("restore submodule");
    write_str(
        &fixture.outer.path().join("vendor/nested"),
        "local.txt",
        "nested submodule dirty\n",
    );
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "nested-submodule dirtiness must affect the configured public API digest"
    );
    std::fs::remove_file(fixture.outer.path().join("vendor/nested/local.txt"))
        .expect("restore nested submodule");

    for (mount, path, changed) in [
        (&alpha, "a/first.txt", "alpha changed\n"),
        (&zeta, "a/first.txt", "zeta changed\n"),
    ] {
        write_str(mount, path, changed);
        assert_ne!(
            baseline.fingerprint,
            digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
            "each declared external mount must affect the configured public API digest"
        );
        write_str(
            mount,
            path,
            if mount == &alpha {
                "alpha-first\n"
            } else {
                "zeta-first\n"
            },
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_output_only_absence_and_invalid_declarations_are_stable() {
    let fixture = init_repo_with_main_commit();
    let mut output_config = VerificationInputFingerprintConfig::default();
    output_config
        .manifest
        .output_only_globs
        .push("generated/**".into());
    assert!(
        !fixture.path().join("generated/result.txt").exists(),
        "validated generated outputs must begin absent before F0"
    );
    let absent = digest(configured_fingerprint(fixture.path(), &output_config).await);
    write_str(
        fixture.path(),
        "generated/result.txt",
        "output from prior pass\n",
    );
    let cleaned = digest(configured_fingerprint(fixture.path(), &output_config).await);
    assert_eq!(absent.fingerprint, cleaned.fingerprint);
    assert!(!fixture.path().join("generated/result.txt").exists());

    let mut overlap = VerificationInputFingerprintConfig::default();
    overlap.manifest.repo_paths.push("src".into());
    overlap.manifest.output_only_globs.push("src".into());
    let first_overlap = configured_fingerprint(fixture.path(), &overlap).await;
    let second_overlap = configured_fingerprint(fixture.path(), &overlap).await;
    assert!(first_overlap.is_unavailable());
    assert_eq!(
        first_overlap, second_overlap,
        "overlap failure must be stable"
    );

    let mut escape = VerificationInputFingerprintConfig::default();
    escape
        .manifest
        .output_only_globs
        .push("../generated/**".into());
    let first_escape = configured_fingerprint(fixture.path(), &escape).await;
    let second_escape = configured_fingerprint(fixture.path(), &escape).await;
    assert!(first_escape.is_unavailable());
    assert_eq!(
        first_escape, second_escape,
        "escaping failure must be stable"
    );
}
