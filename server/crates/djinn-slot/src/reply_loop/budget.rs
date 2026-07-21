//! Role/model-aware budget policy for a single agent session.
//!
//! This module is intentionally a policy/config front-end only. It resolves the
//! turn and cumulative-token budgets, plus soft/hard threshold ratios, that later
//! reply-loop tasks can evaluate against the in-memory usage accumulator. It does
//! not read session-row token counters and it does not inject reminders or route
//! hard-threshold wind-downs.

use std::collections::HashMap;
use std::env;
use std::fmt;

use djinn_provider::provider::TokenUsage;

const DEFAULT_FALLBACK_CONTEXT_WINDOW_TOKENS: u32 = 64_000;
const DEFAULT_SOFT_THRESHOLD_RATIO: f64 = 0.75;
const DEFAULT_HARD_THRESHOLD_RATIO: f64 = 0.92;

/// Apply one provider usage report to reply-loop lifetime spend and occupancy.
///
/// Provider adapters normalize `TokenUsage::input`: OpenAI/Google input already
/// includes cache tokens while Anthropic-format adapters report cache fields
/// separately. Cache fields are therefore intentionally not added to billed
/// lifetime input here. They remain in the normalized cache-inclusive
/// `context_total` occupancy snapshot used for compaction pressure.
pub(crate) fn record_provider_usage(
    lifetime_tokens_in: &mut u32,
    lifetime_tokens_out: &mut u32,
    lifetime_cache_read: &mut u32,
    lifetime_cache_write: &mut u32,
    lifetime_reasoning_out: &mut u32,
    current_context_tokens: &mut u32,
    usage: &TokenUsage,
) {
    *lifetime_tokens_in = lifetime_tokens_in.saturating_add(usage.input);
    *lifetime_tokens_out = lifetime_tokens_out.saturating_add(usage.output);
    *lifetime_cache_read = lifetime_cache_read.saturating_add(usage.cache_read);
    *lifetime_cache_write = lifetime_cache_write.saturating_add(usage.cache_write);
    *lifetime_reasoning_out = lifetime_reasoning_out.saturating_add(usage.reasoning_output);
    *current_context_tokens = usage.context_total();
}

/// Stable test-support seam for the production usage-accounting operation.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageAccountingForTest {
    pub lifetime_tokens_in: u32,
    pub lifetime_tokens_out: u32,
    pub lifetime_cache_read: u32,
    pub lifetime_cache_write: u32,
    pub lifetime_reasoning_out: u32,
    pub current_context_tokens: u32,
}

#[cfg(any(test, feature = "test-support"))]
impl UsageAccountingForTest {
    pub fn record(&mut self, usage: &TokenUsage) {
        record_provider_usage(
            &mut self.lifetime_tokens_in,
            &mut self.lifetime_tokens_out,
            &mut self.lifetime_cache_read,
            &mut self.lifetime_cache_write,
            &mut self.lifetime_reasoning_out,
            &mut self.current_context_tokens,
            usage,
        );
    }

    /// A reactive compaction clears only the old request's occupancy snapshot.
    pub fn clear_occupancy_after_reactive_compaction(&mut self) {
        self.current_context_tokens = 0;
    }

    /// A proactive compaction clears only the old request's occupancy snapshot.
    pub fn clear_occupancy_after_proactive_compaction(&mut self) {
        self.current_context_tokens = 0;
    }

    /// Evaluate the production soft cumulative-spend predicate for a fixed test
    /// budget. Context occupancy is deliberately not an input to this decision.
    pub fn exceeds_soft_lifetime_budget(
        &self,
        max_cumulative_tokens: u64,
        soft_threshold_ratio: f64,
    ) -> bool {
        lifetime_budget_threshold_exceeded(
            self.lifetime_tokens_in,
            self.lifetime_tokens_out,
            max_cumulative_tokens,
            soft_threshold_ratio,
        )
    }

    /// Evaluate the production hard cumulative-spend predicate for a fixed test
    /// budget. Context occupancy is deliberately not an input to this decision.
    pub fn exceeds_hard_lifetime_budget(
        &self,
        max_cumulative_tokens: u64,
        hard_threshold_ratio: f64,
    ) -> bool {
        lifetime_budget_threshold_exceeded(
            self.lifetime_tokens_in,
            self.lifetime_tokens_out,
            max_cumulative_tokens,
            hard_threshold_ratio,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionBudgetPolicy {
    fallback_context_window_tokens: u32,
    soft_threshold_ratio: f64,
    hard_threshold_ratio: f64,
    role_overrides: HashMap<SessionBudgetRole, RoleBudgetOverride>,
}

/// Resolved, immutable session budget with metadata needed by evaluators.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedSessionBudget {
    pub(crate) role_name: String,
    pub(crate) role: SessionBudgetRole,
    pub(crate) model_id: String,
    pub(crate) context_window_tokens: u32,
    pub(crate) context_window_known: bool,
    pub(crate) effective_max_turns: u32,
    pub(crate) max_cumulative_tokens: u64,
    pub(crate) soft_threshold_ratio: f64,
    pub(crate) hard_threshold_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionBudgetConfigError {
    var_name: String,
    reason: String,
}

impl fmt::Display for SessionBudgetConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.var_name, self.reason)
    }
}

impl std::error::Error for SessionBudgetConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SessionBudgetRole {
    Worker,
    Planner,
    Architect,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct RoleBudgetDefault {
    max_turns: u32,
    cumulative_context_windows: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct RoleBudgetOverride {
    max_turns: Option<u32>,
    max_cumulative_tokens: Option<u64>,
    soft_threshold_ratio: Option<f64>,
    hard_threshold_ratio: Option<f64>,
}

impl SessionBudgetPolicy {
    pub(crate) fn from_env() -> Result<Self, SessionBudgetConfigError> {
        Self::from_env_iter(env::vars())
    }
    #[cfg(test)]
    fn from_env_iter<I, K, V>(vars: I) -> Result<Self, SessionBudgetConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let env: HashMap<String, String> = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        Self::from_env_map(&env)
    }
    #[cfg(not(test))]
    fn from_env_iter<I, K, V>(vars: I) -> Result<Self, SessionBudgetConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let env: HashMap<String, String> = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        Self::from_env_map(&env)
    }
    fn from_env_map(env: &HashMap<String, String>) -> Result<Self, SessionBudgetConfigError> {
        let mut policy = Self::default();
        for role in [
            SessionBudgetRole::Worker,
            SessionBudgetRole::Planner,
            SessionBudgetRole::Architect,
        ] {
            let prefix = role.env_prefix();
            let mut override_value = RoleBudgetOverride::default();
            let max_turns_var = format!("DJINN_SESSION_BUDGET_{prefix}_MAX_TURNS");
            override_value.max_turns = parse_optional_u32(env, &max_turns_var, |v| {
                if v >= 2 {
                    Ok(v)
                } else {
                    Err("must be at least 2 so the wind-down turn has room to run".to_string())
                }
            })?;
            let max_tokens_var = format!("DJINN_SESSION_BUDGET_{prefix}_MAX_CUMULATIVE_TOKENS");
            override_value.max_cumulative_tokens = parse_optional_u64(env, &max_tokens_var, |v| {
                if v > 0 {
                    Ok(v)
                } else {
                    Err("must be greater than zero".to_string())
                }
            })?;
            let soft_var = format!("DJINN_SESSION_BUDGET_{prefix}_SOFT_THRESHOLD_RATIO");
            override_value.soft_threshold_ratio = parse_optional_ratio(env, &soft_var)?;
            let hard_var = format!("DJINN_SESSION_BUDGET_{prefix}_HARD_THRESHOLD_RATIO");
            override_value.hard_threshold_ratio = parse_optional_ratio(env, &hard_var)?;
            if let (Some(soft), Some(hard)) = (
                override_value.soft_threshold_ratio,
                override_value.hard_threshold_ratio,
            ) && soft >= hard
            {
                return Err(SessionBudgetConfigError {
                    var_name: soft_var,
                    reason: format!("must be less than {hard_var}; got soft={soft}, hard={hard}"),
                });
            }
            if override_value != RoleBudgetOverride::default() {
                policy.role_overrides.insert(role, override_value);
            }
        }
        Ok(policy)
    }
    pub(crate) fn resolve(
        &self,
        role_name: &str,
        model_id: &str,
        context_window: i64,
        max_turns_override: Option<u32>,
    ) -> ResolvedSessionBudget {
        let role = SessionBudgetRole::from_role_name(role_name);
        let defaults = role.defaults();
        let overrides = self.role_overrides.get(&role).copied().unwrap_or_default();
        let context_window_known = context_window > 0;
        let context_window_tokens = if context_window_known {
            context_window as u32
        } else {
            self.fallback_context_window_tokens
        };
        let default_max_cumulative_tokens =
            u64::from(context_window_tokens) * u64::from(defaults.cumulative_context_windows);
        let soft_threshold_ratio = overrides
            .soft_threshold_ratio
            .unwrap_or(self.soft_threshold_ratio);
        let hard_threshold_ratio = overrides
            .hard_threshold_ratio
            .unwrap_or(self.hard_threshold_ratio);
        debug_assert!(soft_threshold_ratio > 0.0);
        debug_assert!(soft_threshold_ratio < hard_threshold_ratio);
        debug_assert!(hard_threshold_ratio <= 1.0);
        ResolvedSessionBudget {
            role_name: role_name.to_string(),
            role,
            model_id: model_id.to_string(),
            context_window_tokens,
            context_window_known,
            effective_max_turns: max_turns_override
                .map(|v| v.max(2))
                .unwrap_or_else(|| overrides.max_turns.unwrap_or(defaults.max_turns)),
            max_cumulative_tokens: overrides
                .max_cumulative_tokens
                .unwrap_or(default_max_cumulative_tokens),
            soft_threshold_ratio,
            hard_threshold_ratio,
        }
    }
}

/// Decide whether the reply loop's lifetime spend has crossed the resolved
/// soft-threshold for the session budget.
pub(crate) fn soft_budget_threshold_exceeded(
    budget: &ResolvedSessionBudget,
    total_tokens_in: u32,
    total_tokens_out: u32,
) -> bool {
    lifetime_budget_threshold_exceeded(
        total_tokens_in,
        total_tokens_out,
        budget.max_cumulative_tokens,
        budget.soft_threshold_ratio,
    )
}

/// Decide whether the reply loop's lifetime spend has crossed the resolved
/// hard-threshold for the session budget.
pub(crate) fn hard_budget_threshold_exceeded(
    budget: &ResolvedSessionBudget,
    total_tokens_in: u32,
    total_tokens_out: u32,
) -> bool {
    lifetime_budget_threshold_exceeded(
        total_tokens_in,
        total_tokens_out,
        budget.max_cumulative_tokens,
        budget.hard_threshold_ratio,
    )
}

/// Shared cumulative-spend predicate used by the reply loop and its stable
/// test-support seam. Current-context occupancy is intentionally excluded:
/// `needs_compaction` owns context-window pressure decisions.
fn lifetime_budget_threshold_exceeded(
    total_tokens_in: u32,
    total_tokens_out: u32,
    max_cumulative_tokens: u64,
    threshold_ratio: f64,
) -> bool {
    if max_cumulative_tokens == 0 || threshold_ratio <= 0.0 {
        return false;
    }
    let cumulative_spend = total_tokens_in.saturating_add(total_tokens_out);
    let threshold_cap = (max_cumulative_tokens as f64) * threshold_ratio;
    (cumulative_spend as f64) >= threshold_cap
}

impl Default for SessionBudgetPolicy {
    fn default() -> Self {
        Self {
            fallback_context_window_tokens: DEFAULT_FALLBACK_CONTEXT_WINDOW_TOKENS,
            soft_threshold_ratio: DEFAULT_SOFT_THRESHOLD_RATIO,
            hard_threshold_ratio: DEFAULT_HARD_THRESHOLD_RATIO,
            role_overrides: HashMap::new(),
        }
    }
}

impl SessionBudgetRole {
    fn from_role_name(role_name: &str) -> Self {
        match role_name {
            "worker" | "reviewer" | "task_reviewer" => Self::Worker,
            "planner" => Self::Planner,
            "architect" => Self::Architect,
            _ => Self::Other,
        }
    }
    fn defaults(self) -> RoleBudgetDefault {
        match self {
            Self::Worker => RoleBudgetDefault {
                max_turns: 1000,
                cumulative_context_windows: 24,
            },
            Self::Planner => RoleBudgetDefault {
                max_turns: 600,
                cumulative_context_windows: 12,
            },
            Self::Architect => RoleBudgetDefault {
                max_turns: 400,
                cumulative_context_windows: 8,
            },
            Self::Other => RoleBudgetDefault {
                max_turns: 1000,
                cumulative_context_windows: 24,
            },
        }
    }
    fn env_prefix(self) -> &'static str {
        match self {
            Self::Worker => "WORKER",
            Self::Planner => "PLANNER",
            Self::Architect => "ARCHITECT",
            Self::Other => "OTHER",
        }
    }
}

fn parse_optional_u32<F>(
    env: &HashMap<String, String>,
    var_name: &str,
    validate: F,
) -> Result<Option<u32>, SessionBudgetConfigError>
where
    F: FnOnce(u32) -> Result<u32, String>,
{
    let Some(raw) = env.get(var_name) else {
        return Ok(None);
    };
    let parsed = raw
        .parse::<u32>()
        .map_err(|e| config_error(var_name, format!("must be an unsigned integer: {e}")))?;
    validate(parsed)
        .map(Some)
        .map_err(|reason| config_error(var_name, reason))
}

fn parse_optional_u64<F>(
    env: &HashMap<String, String>,
    var_name: &str,
    validate: F,
) -> Result<Option<u64>, SessionBudgetConfigError>
where
    F: FnOnce(u64) -> Result<u64, String>,
{
    let Some(raw) = env.get(var_name) else {
        return Ok(None);
    };
    let parsed = raw
        .parse::<u64>()
        .map_err(|e| config_error(var_name, format!("must be an unsigned integer: {e}")))?;
    validate(parsed)
        .map(Some)
        .map_err(|reason| config_error(var_name, reason))
}

fn parse_optional_ratio(
    env: &HashMap<String, String>,
    var_name: &str,
) -> Result<Option<f64>, SessionBudgetConfigError> {
    let Some(raw) = env.get(var_name) else {
        return Ok(None);
    };
    let parsed = raw
        .parse::<f64>()
        .map_err(|e| config_error(var_name, format!("must be a decimal ratio: {e}")))?;
    if parsed.is_finite() && parsed > 0.0 && parsed <= 1.0 {
        Ok(Some(parsed))
    } else {
        Err(config_error(
            var_name,
            "must be finite and within the range (0.0, 1.0]".to_string(),
        ))
    }
}

fn config_error(var_name: &str, reason: String) -> SessionBudgetConfigError {
    SessionBudgetConfigError {
        var_name: var_name.to_string(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy_from_pairs(
        pairs: &[(&str, &str)],
    ) -> Result<SessionBudgetPolicy, SessionBudgetConfigError> {
        SessionBudgetPolicy::from_env_iter(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        )
    }
    #[test]
    fn role_defaults_distinguish_turns_and_scale_tokens_by_context_window() {
        let policy = SessionBudgetPolicy::default();
        let worker = policy.resolve("worker", "anthropic/claude", 200_000, None);
        let planner = policy.resolve("planner", "anthropic/claude", 200_000, None);
        let architect = policy.resolve("architect", "anthropic/claude", 200_000, None);
        assert_eq!(worker.effective_max_turns, 1000);
        assert_eq!(planner.effective_max_turns, 600);
        assert_eq!(architect.effective_max_turns, 400);
        assert_eq!(worker.max_cumulative_tokens, 4_800_000);
        assert_eq!(planner.max_cumulative_tokens, 2_400_000);
        assert_eq!(architect.max_cumulative_tokens, 1_600_000);
        assert_eq!(worker.soft_threshold_ratio, 0.75);
        assert_eq!(worker.hard_threshold_ratio, 0.92);
    }
    #[test]
    fn unknown_context_window_uses_deterministic_conservative_fallback() {
        let policy = SessionBudgetPolicy::default();
        let zero = policy.resolve("planner", "test/unknown", 0, None);
        let negative = policy.resolve("planner", "test/unknown", -1, None);
        assert!(!zero.context_window_known);
        assert_eq!(zero.context_window_tokens, 64_000);
        assert_eq!(zero.max_cumulative_tokens, 768_000);
        assert_eq!(negative.max_cumulative_tokens, zero.max_cumulative_tokens);
    }
    #[test]
    fn role_scoped_overrides_are_applied_and_test_turn_override_wins() {
        let policy = policy_from_pairs(&[
            ("DJINN_SESSION_BUDGET_WORKER_MAX_TURNS", "321"),
            (
                "DJINN_SESSION_BUDGET_WORKER_MAX_CUMULATIVE_TOKENS",
                "654321",
            ),
            ("DJINN_SESSION_BUDGET_WORKER_SOFT_THRESHOLD_RATIO", "0.70"),
            ("DJINN_SESSION_BUDGET_WORKER_HARD_THRESHOLD_RATIO", "0.90"),
        ])
        .expect("valid role-scoped overrides");
        let production = policy.resolve("worker", "test/model", 100_000, None);
        assert_eq!(production.effective_max_turns, 321);
        assert_eq!(production.max_cumulative_tokens, 654_321);
        assert_eq!(production.soft_threshold_ratio, 0.70);
        assert_eq!(production.hard_threshold_ratio, 0.90);
        let test = policy.resolve("worker", "test/model", 100_000, Some(1));
        assert_eq!(test.effective_max_turns, 2);
    }
    #[test]
    fn invalid_overrides_are_rejected() {
        let err = policy_from_pairs(&[("DJINN_SESSION_BUDGET_PLANNER_MAX_TURNS", "1")])
            .expect_err("turn cap below two is invalid");
        assert_eq!(err.var_name, "DJINN_SESSION_BUDGET_PLANNER_MAX_TURNS");
        let err =
            policy_from_pairs(&[("DJINN_SESSION_BUDGET_ARCHITECT_SOFT_THRESHOLD_RATIO", "1.2")])
                .expect_err("ratio above one is invalid");
        assert_eq!(
            err.var_name,
            "DJINN_SESSION_BUDGET_ARCHITECT_SOFT_THRESHOLD_RATIO"
        );
        let err = policy_from_pairs(&[
            ("DJINN_SESSION_BUDGET_WORKER_SOFT_THRESHOLD_RATIO", "0.95"),
            ("DJINN_SESSION_BUDGET_WORKER_HARD_THRESHOLD_RATIO", "0.90"),
        ])
        .expect_err("soft threshold must precede hard threshold");
        assert_eq!(
            err.var_name,
            "DJINN_SESSION_BUDGET_WORKER_SOFT_THRESHOLD_RATIO"
        );
    }
}
