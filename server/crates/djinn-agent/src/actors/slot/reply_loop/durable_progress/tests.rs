use super::*;

/// Helper to build a `CommandIdentity` with a stable digest.
fn cmd_identity(tool: &str, args: &str) -> CommandIdentity {
    CommandIdentity {
        tool_name: tool.to_string(),
        normalized_args: args.to_string(),
        digest: format!("{}:{}", tool, args),
    }
}

/// Helper to build a tracked `FileEntry`.
fn tracked_file(path: &str, hash: &str) -> FileEntry {
    FileEntry {
        path: path.to_string(),
        classification: FileClassification::Tracked,
        content_hash: hash.to_string(),
    }
}

/// Helper to build a generated `FileEntry`.
fn generated_file(path: &str, hash: &str) -> FileEntry {
    FileEntry {
        path: path.to_string(),
        classification: FileClassification::Generated,
        content_hash: hash.to_string(),
    }
}

/// Helper to build an ignored `FileEntry`.
fn ignored_file(path: &str, hash: &str) -> FileEntry {
    FileEntry {
        path: path.to_string(),
        classification: FileClassification::Ignored,
        content_hash: hash.to_string(),
    }
}

/// Helper to build an untracked `FileEntry`.
fn untracked_file(path: &str, hash: &str) -> FileEntry {
    FileEntry {
        path: path.to_string(),
        classification: FileClassification::Untracked,
        content_hash: hash.to_string(),
    }
}

// ── AC test: diff grow ──────────────────────────────────────────────

#[test]
fn diff_grow_with_tracked_changes_is_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let before = WorktreeSnapshot {
        entries: vec![tracked_file("src/lib.rs", "aaa")],
    };
    let after = WorktreeSnapshot {
        entries: vec![
            tracked_file("src/lib.rs", "aaa"),
            tracked_file("src/main.rs", "bbb"),
        ],
    };
    let obs = detector.evaluate(TurnInput {
        before: Some(before),
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::WorktreeChanged)
    );
    assert_eq!(obs.worktree_outcome.tracked_changes, 1);
    assert!(!obs.no_progress_streak.should_increment);
}

// ── AC test: diff shrink ────────────────────────────────────────────

#[test]
fn diff_shrink_with_tracked_changes_is_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let before = WorktreeSnapshot {
        entries: vec![
            tracked_file("src/lib.rs", "aaa"),
            tracked_file("src/main.rs", "bbb"),
        ],
    };
    let after = WorktreeSnapshot {
        entries: vec![tracked_file("src/lib.rs", "aaa")],
    };
    let obs = detector.evaluate(TurnInput {
        before: Some(before),
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    // Diff shrink (deletion of src/main.rs) is tracked-change progress.
    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::WorktreeChanged)
    );
    assert_eq!(obs.worktree_outcome.tracked_changes, 1);
}

// ── AC test: generated/ignored-only changes ─────────────────────────

#[test]
fn generated_only_changes_are_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let before = WorktreeSnapshot::default();
    let after = WorktreeSnapshot {
        entries: vec![
            generated_file("target/debug/main", "a1"),
            generated_file("Cargo.lock", "b2"),
        ],
    };
    let obs = detector.evaluate(TurnInput {
        before: Some(before),
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(
        obs.no_reset_reason,
        Some(DurableProgressNoResetReason::GeneratedOnlyChange)
    );
    assert!(obs.worktree_outcome.is_generated_only);
    assert!(obs.no_progress_streak.should_increment);
}

#[test]
fn ignored_only_changes_are_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let after = WorktreeSnapshot {
        entries: vec![ignored_file(".idea/workspace.xml", "c3")],
    };
    let obs = detector.evaluate(TurnInput {
        before: None,
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(
        obs.no_reset_reason,
        Some(DurableProgressNoResetReason::GeneratedOnlyChange)
    );
    assert!(obs.worktree_outcome.is_ignored_only);
}

// ── AC test: no-op/read-only turns ──────────────────────────────────

#[test]
fn read_only_turn_with_no_command_is_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let after = WorktreeSnapshot {
        entries: vec![tracked_file("src/lib.rs", "aaa")],
    };
    let obs = detector.evaluate(TurnInput {
        before: None,
        after,
        command_result: None,
        is_read_only_turn: true,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(
        obs.no_reset_reason,
        Some(DurableProgressNoResetReason::ReadOnlyOrNoOpToolSuccess)
    );
}

#[test]
fn empty_worktree_and_no_command_is_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot::default(),
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(
        obs.no_reset_reason,
        Some(DurableProgressNoResetReason::ReadOnlyOrNoOpToolSuccess)
    );
}

// ── AC test: already-green command reruns ───────────────────────────

#[test]
fn already_green_command_rerun_is_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    // First run: green (newly-green → durable progress).
    let obs1 = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: None,
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });
    assert!(obs1.is_durable_progress);
    assert_eq!(
        obs1.reset_reason,
        Some(DurableProgressResetReason::NewlyGreenVerification)
    );

    // Second run: green→green (already-green rerun → no progress).
    let obs2 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 1,
    });
    assert!(!obs2.is_durable_progress);
    assert_eq!(
        obs2.no_reset_reason,
        Some(DurableProgressNoResetReason::AlreadyGreenVerificationRerun)
    );
}

// ── AC test: newly-green commands ───────────────────────────────────

#[test]
fn newly_green_command_from_red_is_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: Some(CommandResultClass::Red),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::NewlyGreenVerification)
    );
}

#[test]
fn newly_green_command_from_unknown_is_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: Some(CommandResultClass::Unknown),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::NewlyGreenVerification)
    );
}

#[test]
fn newly_green_command_from_no_prior_result_is_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: None,
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::NewlyGreenVerification)
    );
}

// ── AC test: long-command suspension/unknown ────────────────────────

#[test]
fn long_command_duration_suspends_evaluation() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test --all");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: None,
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: Some(700), // > 600s default threshold
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(
        obs.no_reset_reason,
        Some(DurableProgressNoResetReason::LongCommandSuspended)
    );
    assert!(obs.long_command_suspended);
    // Streak should NOT increment (suspended = don't count).
    assert!(!obs.no_progress_streak.should_increment);
}

#[test]
fn long_running_command_result_suspends_evaluation() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "npm run build");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/index.ts", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: None,
            after: CommandResultClass::LongRunning,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(
        obs.no_reset_reason,
        Some(DurableProgressNoResetReason::LongCommandSuspended)
    );
}

#[test]
fn command_duration_exactly_at_threshold_is_suspended() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot::default(),
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: None,
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: Some(600), // exactly at default threshold
        turn_index: 0,
    });

    assert!(obs.long_command_suspended);
    assert!(!obs.is_durable_progress);
}

// ── AC test: flaky result classification ────────────────────────────

#[test]
fn intermittent_green_red_is_flaky_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test flaky_test");

    // Turn 0: green
    let obs0 = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: None,
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });
    assert!(obs0.is_durable_progress);
    assert!(!obs0.flaky_observation);

    // Turn 1: red (oscillation detected → flaky)
    let obs1 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Red,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 1,
    });
    assert!(!obs1.is_durable_progress);
    assert!(obs1.flaky_observation);
    assert_eq!(
        obs1.no_reset_reason,
        Some(DurableProgressNoResetReason::FlakyCommandResult)
    );
    assert_eq!(obs1.command_flaky_streak, Some(1));

    // Turn 2: green again (still oscillating → flaky)
    let obs2 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Red),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 2,
    });
    assert!(!obs2.is_durable_progress);
    assert!(obs2.flaky_observation);
    assert_eq!(obs2.command_flaky_streak, Some(2));

    // Turn 3: green (same as previous → NOT flaky anymore, stable green)
    let obs3 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v2")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 3,
    });
    // Stable green→green with tracked content change.
    assert!(!obs3.flaky_observation);
}

#[test]
fn stable_red_to_green_with_followup_green_is_not_flaky() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    // Turn 0: red
    detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: None,
            after: CommandResultClass::Red,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    // Turn 1: green (newly-green → durable progress; flaky detected but
    // this is the first oscillation and it IS newly-green, so durable
    // progress takes precedence because flaky detection happens BEFORE
    // newly-green check... wait, let me re-check the evaluation order.
    // Actually in the code: flaky detection (5b) happens before
    // newly-green (5d). So red→green where the last two results differ
    // would be flaky first. This is actually correct: a single red→green
    // flip IS a flaky pattern. It only becomes stable if the NEXT result
    // is also green.
    let obs1 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v2")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Red),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 1,
    });
    // This IS flaky (Red, Green oscillation).
    assert!(obs1.flaky_observation);

    // Turn 2: green (Green, Green → stable → NOT flaky → falls through
    // to already-green check or worktree check).
    let obs2 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v2")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v3")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 2,
    });
    assert!(!obs2.flaky_observation);
}

// ── Additional edge-case tests ──────────────────────────────────────

#[test]
fn first_turn_with_tracked_changes_and_no_command_is_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let after = WorktreeSnapshot {
        entries: vec![tracked_file("src/lib.rs", "new_hash")],
    };
    let obs = detector.evaluate(TurnInput {
        before: None,
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::WorktreeChanged)
    );
    assert_eq!(obs.worktree_outcome.tracked_changes, 1);
    assert_eq!(obs.evaluated_turn_index, 0);
}

#[test]
fn untracked_new_file_is_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let after = WorktreeSnapshot {
        entries: vec![untracked_file("new_file.txt", "hash")],
    };
    let obs = detector.evaluate(TurnInput {
        before: None,
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(obs.worktree_outcome.untracked_changes, 1);
}

#[test]
fn mixed_generated_and_tracked_changes_are_durable_progress() {
    let mut detector = DurableProgressDetector::new();
    let after = WorktreeSnapshot {
        entries: vec![
            tracked_file("src/lib.rs", "new"),
            generated_file("Cargo.lock", "lock_hash"),
        ],
    };
    let obs = detector.evaluate(TurnInput {
        before: None,
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert!(!obs.worktree_outcome.is_generated_only);
    assert_eq!(obs.worktree_outcome.tracked_changes, 1);
    assert_eq!(obs.worktree_outcome.generated_changes, 1);
}

#[test]
fn content_hash_change_is_detected() {
    let mut detector = DurableProgressDetector::new();
    let before = WorktreeSnapshot {
        entries: vec![tracked_file("src/lib.rs", "old_hash")],
    };
    let after = WorktreeSnapshot {
        entries: vec![tracked_file("src/lib.rs", "new_hash")],
    };
    let obs = detector.evaluate(TurnInput {
        before: Some(before),
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(obs.worktree_outcome.total_changes, 1);
    assert_eq!(obs.worktree_outcome.tracked_changes, 1);
}

#[test]
fn identical_worktree_snapshots_are_no_changes() {
    let mut detector = DurableProgressDetector::new();
    let snapshot = WorktreeSnapshot {
        entries: vec![tracked_file("src/lib.rs", "same_hash")],
    };
    let obs = detector.evaluate(TurnInput {
        before: Some(snapshot.clone()),
        after: snapshot,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(obs.worktree_outcome.total_changes, 0);
}

#[test]
fn observation_includes_shadow_mode_flag() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot::default(),
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });
    assert!(obs.shadow_mode);
}

#[test]
fn detector_from_threshold_config_respects_custom_values() {
    let config = NoProgressThresholdConfig {
        min_evaluated_turns: 2,
        warning_turns: 5,
        model_rotation_turns: None,
        forced_exit_turns: None,
        long_command_suspension_secs: 300,
        flaky_command_grace_turns: 5,
    };
    let mut detector = DurableProgressDetector::from_threshold_config(&config);

    // A 350s command should be suspended with the custom 300s threshold.
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot::default(),
        command_result: Some(CommandResultTransition {
            command: cmd_identity("shell", "cargo test"),
            before: None,
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: Some(350),
        turn_index: 0,
    });
    assert!(obs.long_command_suspended);
}

#[test]
fn turn_index_propagated_to_observation() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot::default(),
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 42,
    });
    assert_eq!(obs.evaluated_turn_index, 42);
}

#[test]
fn evaluated_turn_count_increments() {
    let mut detector = DurableProgressDetector::new();
    assert_eq!(detector.evaluated_turn_count(), 0);

    for i in 0..5 {
        detector.evaluate(TurnInput {
            before: None,
            after: WorktreeSnapshot::default(),
            command_result: None,
            is_read_only_turn: false,
            command_duration_secs: None,
            turn_index: i,
        });
    }
    assert_eq!(detector.evaluated_turn_count(), 5);
}

#[test]
fn command_transition_preserved_in_observation() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Red),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    let transition = obs.command_transition.as_ref().unwrap();
    assert_eq!(transition.command, cmd);
    assert_eq!(transition.before, Some(CommandResultClass::Red));
    assert_eq!(transition.after, CommandResultClass::Green);
}

#[test]
fn flaky_observation_tracks_command_streak() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    // green
    detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: None,
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    // red → flaky streak = 1
    let obs1 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Red,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 1,
    });
    assert_eq!(obs1.command_flaky_streak, Some(1));

    // green → flaky streak = 2
    let obs2 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd.clone(),
            before: Some(CommandResultClass::Red),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 2,
    });
    assert_eq!(obs2.command_flaky_streak, Some(2));

    // red → flaky streak = 3
    let obs3 = detector.evaluate(TurnInput {
        before: Some(WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        }),
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Red,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 3,
    });
    assert_eq!(obs3.command_flaky_streak, Some(3));
}

#[test]
fn read_only_turn_with_command_is_not_caught_by_read_only_guard() {
    // If is_read_only_turn=true but a command_result is present,
    // the read-only guard should NOT fire (the command is the signal).
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: Some(CommandResultClass::Red),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: true,
        command_duration_secs: None,
        turn_index: 0,
    });

    // Should be durable progress (newly-green), not read-only.
    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::NewlyGreenVerification)
    );
}

#[test]
fn no_progress_observation_streak_field_says_increment() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot::default(),
        command_result: None,
        is_read_only_turn: true,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert!(obs.no_progress_streak.should_increment);
}

#[test]
fn durable_progress_observation_streak_field_says_reset() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "new")],
        },
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert!(!obs.no_progress_streak.should_increment);
    assert_eq!(obs.no_progress_streak.streak_after, 0);
}

// ── Serialization round-trip test ───────────────────────────────────

#[test]
fn observation_serializes_and_deserializes() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd_identity("shell", "cargo test"),
            before: Some(CommandResultClass::Red),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 5,
    });

    let json = serde_json::to_string(&obs).expect("serialize");
    let round_tripped: DurableProgressObservation =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(round_tripped.is_durable_progress, obs.is_durable_progress);
    assert_eq!(round_tripped.reset_reason, obs.reset_reason);
    assert_eq!(round_tripped.no_reset_reason, obs.no_reset_reason);
    assert_eq!(round_tripped.evaluated_turn_index, obs.evaluated_turn_index);
    assert_eq!(round_tripped.shadow_mode, obs.shadow_mode);
    assert_eq!(round_tripped.flaky_observation, obs.flaky_observation);
    assert_eq!(
        round_tripped.long_command_suspended,
        obs.long_command_suspended
    );
}

#[test]
fn worktree_outcome_serializes_correctly() {
    let outcome = IgnoredGeneratedOutcome {
        total_changes: 5,
        tracked_changes: 2,
        untracked_changes: 1,
        ignored_changes: 1,
        generated_changes: 1,
        is_generated_only: false,
        is_ignored_only: false,
    };
    let json = serde_json::to_string(&outcome).expect("serialize");
    let round_tripped: IgnoredGeneratedOutcome = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(round_tripped, outcome);
}

// ── Edge: command goes red from no prior result ────────────────────

#[test]
fn command_red_with_no_prior_is_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: None,
            after: CommandResultClass::Red,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(
        obs.no_reset_reason,
        Some(DurableProgressNoResetReason::ReadOnlyOrNoOpToolSuccess)
    );
}

#[test]
fn command_unknown_result_is_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Unknown,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
}

// ── Edge: default detector ──────────────────────────────────────────

#[test]
fn default_detector_has_zero_evaluated_turns() {
    let detector = DurableProgressDetector::default();
    assert_eq!(detector.evaluated_turn_count(), 0);
}

// ── Edge: worktree_outcome on long-command suspension ───────────────

#[test]
fn long_command_suspension_still_reports_worktree_outcome() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![
                tracked_file("src/lib.rs", "v1"),
                generated_file("Cargo.lock", "l1"),
            ],
        },
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: Some(999),
        turn_index: 0,
    });

    assert!(obs.long_command_suspended);
    assert_eq!(obs.worktree_outcome.tracked_changes, 1);
    assert_eq!(obs.worktree_outcome.generated_changes, 1);
}

// ── Edge: green→green with no prior green (first time, both green) ──

#[test]
fn first_command_green_with_before_green_falls_through_to_worktree() {
    // Command was not previously seen. before=Green. after=Green.
    // was_previously_green=false → already-green check doesn't fire.
    // before_was_not_green = before==Green → false → newly-green check
    // doesn't fire. Falls through to worktree diff check.
    let mut detector = DurableProgressDetector::new();
    let cmd = cmd_identity("shell", "cargo test");

    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot {
            entries: vec![tracked_file("src/lib.rs", "v1")],
        },
        command_result: Some(CommandResultTransition {
            command: cmd,
            before: Some(CommandResultClass::Green),
            after: CommandResultClass::Green,
        }),
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    // Falls through to worktree diff check → tracked changes → progress.
    assert!(obs.is_durable_progress);
    assert_eq!(
        obs.reset_reason,
        Some(DurableProgressResetReason::WorktreeChanged)
    );
}

// ── Edge: multiple file types in single snapshot ────────────────────

#[test]
fn all_four_classifications_counted_correctly() {
    let mut detector = DurableProgressDetector::new();
    let after = WorktreeSnapshot {
        entries: vec![
            tracked_file("src/lib.rs", "v1"),
            untracked_file("tmp.txt", "v2"),
            ignored_file(".env", "v3"),
            generated_file("target/debug/binary", "v4"),
        ],
    };
    let obs = detector.evaluate(TurnInput {
        before: None,
        after,
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(obs.is_durable_progress);
    assert_eq!(obs.worktree_outcome.total_changes, 4);
    assert_eq!(obs.worktree_outcome.tracked_changes, 1);
    assert_eq!(obs.worktree_outcome.untracked_changes, 1);
    assert_eq!(obs.worktree_outcome.ignored_changes, 1);
    assert_eq!(obs.worktree_outcome.generated_changes, 1);
    assert!(!obs.worktree_outcome.is_generated_only);
    assert!(!obs.worktree_outcome.is_ignored_only);
}

// ── Edge: snapshot unavailable (no before, no after entries) ────────

#[test]
fn first_turn_empty_after_is_no_progress() {
    let mut detector = DurableProgressDetector::new();
    let obs = detector.evaluate(TurnInput {
        before: None,
        after: WorktreeSnapshot::default(),
        command_result: None,
        is_read_only_turn: false,
        command_duration_secs: None,
        turn_index: 0,
    });

    assert!(!obs.is_durable_progress);
    assert_eq!(obs.worktree_outcome.total_changes, 0);
}
