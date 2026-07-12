//! Per-turn inline-character budget policy for collected tool results.
//!
//! Kept separate from collection/dispatch to retain the latter's file-size
//! guard while preserving the post-merge, pre-transcript policy boundary.

use djinn_provider::message::ContentBlock;

use super::tool_dispatch::{CollectedToolResult, ToolDispatchContext};

/// Environment variable overriding the per-turn inline-character budget.
pub(super) const TURN_INLINE_CHAR_BUDGET_ENV: &str = "DJINN_TURN_INLINE_CHAR_BUDGET";
/// Environment variable overriding the per-turn preview floor.
pub(super) const TURN_INLINE_PREVIEW_FLOOR_ENV: &str = "DJINN_TURN_INLINE_PREVIEW_FLOOR";

/// Default per-turn inline-character budget: 100,000 chars.
pub(super) const DEFAULT_TURN_INLINE_CHAR_BUDGET: usize = 100_000;
/// Default preview floor for turn-budget externalization: 10,000 chars.
pub(super) const DEFAULT_TURN_INLINE_PREVIEW_FLOOR: usize = 10_000;

/// Configuration for the per-turn inline-character budget post-pass.
///
/// Reads validated environment overrides on construction so a single process
/// observes a consistent configuration across all turns. Under-budget turns are
/// left byte-for-byte unchanged.
#[derive(Debug, Clone, Copy)]
pub(super) struct TurnInlineBudgetConfig {
    pub(super) budget: usize,
    pub(super) preview_floor: usize,
}

impl TurnInlineBudgetConfig {
    /// Read the configuration from the environment, falling back to the
    /// defaults when the variables are unset or non-parseable.
    pub(super) fn from_env() -> Self {
        Self {
            budget: read_positive_env_usize(
                TURN_INLINE_CHAR_BUDGET_ENV,
                DEFAULT_TURN_INLINE_CHAR_BUDGET,
            ),
            preview_floor: read_positive_env_usize(
                TURN_INLINE_PREVIEW_FLOOR_ENV,
                DEFAULT_TURN_INLINE_PREVIEW_FLOOR,
            ),
        }
    }
}

pub(super) fn read_positive_env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => default,
        },
        Err(_) => default,
    }
}

/// Sum the inline character count across the text content blocks of one
/// collected result. Non-text content blocks (images, documents) contribute
/// zero to the character accounting, consistent with the char-unit policy.
fn result_inline_chars(result: &CollectedToolResult) -> usize {
    result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().count(),
            _ => 0,
        })
        .sum()
}

/// Total inline characters across all collected results for the turn.
fn total_inline_chars(results: &[CollectedToolResult]) -> usize {
    results.iter().map(result_inline_chars).sum()
}

/// Replace the single text block of `result` with the externalized `stub`,
/// preserving the originating `tool_use_id` and tool name. Returns the
/// original inline-char count and the new inline-char count.
fn apply_externalized_stub(result: &mut CollectedToolResult, stub: String) -> (usize, usize) {
    let original_chars = result_inline_chars(result);
    result.content = vec![ContentBlock::Text { text: stub }];
    let new_chars = result_inline_chars(result);
    (original_chars, new_chars)
}

/// Per-turn inline-character budget post-pass.
///
/// Runs immediately after the merged results are sorted and before conversion
/// to transcript `ContentBlock`s. When the projected inline-character total
/// exceeds the configured budget, it greedily externalizes the largest
/// shrinking tool-result candidates first via the landed
/// `SlotToolDispatcher::externalize_rendered_result` seam, using the configured
/// preview floor as the stub size. A candidate is skipped when the returned
/// canonical stub is not smaller than the original. Selection stops when the
/// projected total fits the budget or no remaining candidate can shrink below
/// the preview floor; residual overflow is permitted when the floor prevents
/// fitting. A budget trip emits distinct char-unit telemetry.
pub(super) fn apply_turn_inline_budget_pass(
    results: &mut [CollectedToolResult],
    ctx: &ToolDispatchContext<'_>,
) {
    apply_turn_inline_budget_pass_with_config(results, ctx, TurnInlineBudgetConfig::from_env());
}

/// Config-injectable core of [`apply_turn_inline_budget_pass`] so unit tests can
/// drive the policy deterministically without touching process-wide environment
/// variables (which race under parallel test execution).
pub(super) fn apply_turn_inline_budget_pass_with_config(
    results: &mut [CollectedToolResult],
    ctx: &ToolDispatchContext<'_>,
    config: TurnInlineBudgetConfig,
) {
    let tool_count = results.len();
    if tool_count == 0 {
        return;
    }
    let inline_chars_pre = total_inline_chars(results);
    if inline_chars_pre <= config.budget {
        // Under-budget: leave the turn byte-for-byte unchanged.
        return;
    }
    let largest_result_chars = results.iter().map(result_inline_chars).max().unwrap_or(0);

    // Compute the group-level `tool_name_missing` flag across ALL collected
    // results before selection. A nameless result may exist in the batch without
    // being selected for externalization (e.g. an orphan streaming result that
    // is smaller than a larger named result), so this flag must reflect the
    // entire group, not just the selected candidates.
    let tool_name_missing = results.iter().any(|r| r.name_missing);

    // Greedy largest-first selection. Iterate while over budget; each pass
    // picks the largest remaining candidate that can still shrink.
    let mut externalized_count: usize = 0;
    // Track which results have already been externalized this pass so we do not
    // re-select them.
    let mut externalized: Vec<bool> = vec![false; results.len()];

    loop {
        let current_total = total_inline_chars(results);
        if current_total <= config.budget {
            break;
        }
        // Find the largest non-externalized candidate whose content is a single
        // text block large enough that a preview-floor stub can shrink it.
        let mut best: Option<usize> = None;
        let mut best_chars = 0usize;
        for (i, result) in results.iter().enumerate() {
            if externalized[i] {
                continue;
            }
            let chars = result_inline_chars(result);
            // Only candidates larger than the preview floor can shrink.
            if chars <= config.preview_floor {
                continue;
            }
            if best.is_none() || chars > best_chars {
                best = Some(i);
                best_chars = chars;
            }
        }
        let Some(idx) = best else {
            // No candidate can shrink below the preview floor; permit residual
            // overflow rather than shrinking previews further.
            break;
        };
        let result = &mut results[idx];
        // Render the current content text for the seam. Only single text-block
        // results are eligible (multi-block or non-text results are skipped).
        let Some(rendered) = single_text_content(&result.content) else {
            externalized[idx] = true;
            continue;
        };
        let stub = ctx.tool_dispatcher.externalize_rendered_result(
            &result.tool_use_id,
            &result.tool_name,
            rendered,
            config.preview_floor,
        );
        // Non-shrinking guard: skip when the stub is not smaller than the
        // original candidate.
        if stub.chars().count() >= rendered.chars().count() {
            externalized[idx] = true;
            continue;
        }
        let (_original, _new) = apply_externalized_stub(result, stub);
        externalized[idx] = true;
        externalized_count += 1;
    }

    let inline_chars_post = total_inline_chars(results);
    // Emit distinct structured char-budget telemetry only when the budget trips.
    tracing::info!(
        target: "djinn_slot::reply_loop::turn_budget",
        task_id = %ctx.task_id,
        inline_chars_pre,
        inline_chars_post,
        tool_count,
        externalized_count,
        largest_result_chars,
        tool_name_missing,
        budget = config.budget,
        preview_floor = config.preview_floor,
        "ReplyLoop: per-turn inline-character budget post-pass applied"
    );
}

/// Return the text of a single `ContentBlock::Text` content vector, or `None`
/// when the result has zero, multiple, or non-text blocks.
fn single_text_content(content: &[ContentBlock]) -> Option<&str> {
    match content {
        [ContentBlock::Text { text }] => Some(text.as_str()),
        _ => None,
    }
}
