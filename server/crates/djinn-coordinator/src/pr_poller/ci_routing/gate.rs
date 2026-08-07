//! The `ci_evidence_routing` runtime gate (proposal `nafu`).
//!
//! Wave 3 landed the classifier, the durable schema, and the provider-action
//! scope with **no flag anywhere in the tree** — the feature was off only
//! because nothing called it. That is not a gate: the moment a call site
//! appeared, evidence routing would be live in production on the next deploy,
//! with no way to turn it off short of a binary rollback. This module is the
//! actual gate.
//!
//! # Three states, not two
//!
//! The proposal's rollback contract names three, and they are genuinely
//! different postures rather than a bool with a nicer spelling:
//!
//! | State | New routes | In-flight `calling` rows | Legacy path |
//! | --- | --- | --- | --- |
//! | [`CiRoutingGate::Enabled`] | admitted | finalized by their owner | suppressed for routed evidence |
//! | [`CiRoutingGate::Quiescing`] | refused | finalized by their owner | **suppressed while a route is `calling`** |
//! | [`CiRoutingGate::DisabledClean`] | refused | none can exist | authoritative |
//!
//! `Quiescing` is the rollback state and the middle row is why it exists. A
//! two-state flag flipped to `off` while a `rerun_failed_jobs` future is in
//! flight would hand the same evidence straight back to the legacy
//! `handle_queue_failure` → `PrCiFailed` path: the task gets reopened for
//! rework *and* the queue gets re-entered, from one failure. `Quiescing` stops
//! admitting new routes while still letting existing owners finalize
//! authoritatively, and it withholds the legacy path from exactly the evidence
//! whose route is still live. Rollback to `DisabledClean` is legal once the
//! quiescence report reads zero.
//!
//! # Default-off by construction
//!
//! [`CiRoutingGate::default`] is `DisabledClean` and [`CiRoutingGate::from_env`]
//! returns it for an absent variable, an empty one, an unparseable one, and any
//! spelling that is not exactly one of the two opt-ins. There is no value of
//! the environment that turns this on by accident.

/// Environment variable holding the `ci_evidence_routing` gate state.
pub(crate) const ENV_CI_EVIDENCE_ROUTING: &str = "DJINN_CI_EVIDENCE_ROUTING";

/// The runtime posture of the `ci_evidence_routing` feature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CiRoutingGate {
    /// Off, and no route row of this feature can be in flight. The legacy CI
    /// path is authoritative. This is the default in every environment.
    #[default]
    DisabledClean,
    /// Draining. No new route may be reserved or charged; existing owners
    /// finalize their own `calling` rows; the legacy path stays withheld from
    /// evidence whose route is still live.
    Quiescing,
    /// Fully on.
    Enabled,
}

impl CiRoutingGate {
    /// Parse a gate value. Anything that is not exactly `enabled` or
    /// `quiescing` (case- and whitespace-insensitive) is
    /// [`CiRoutingGate::DisabledClean`].
    ///
    /// Deliberately *not* the `1/true/yes` idiom used by the older boolean
    /// flags in this crate: this gate has three states, and accepting `true`
    /// would leave "which of the two on-ish states did you mean" answered by a
    /// default rather than by the operator.
    pub(crate) fn from_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "enabled" => Self::Enabled,
            "quiescing" => Self::Quiescing,
            _ => Self::DisabledClean,
        }
    }

    /// Resolve the gate from a lookup, absent included.
    ///
    /// # Why the default lives here and not in [`Self::from_env`]
    ///
    /// It used to be `std::env::var(..).map(from_value).unwrap_or_default()`,
    /// with the whole expression inside `from_env` — and `from_env` had no
    /// test, because a test would have had to mutate process-global
    /// environment. `from_value` was covered exhaustively and covered nothing
    /// that mattered: changing that one `unwrap_or_default()` to
    /// `unwrap_or(Enabled)` turned the feature on fleet-wide and the entire
    /// 2148-test suite stayed green.
    ///
    /// The *absent* case is the production default, so it belongs in a function
    /// a test can call. `from_env` is now a one-line delegation with nothing
    /// left in it to mutate, and [`the wiring test`] asserts it stays that way.
    ///
    /// [`the wiring test`]: super::executor::tests
    pub(crate) fn from_lookup(lookup: impl FnOnce(&str) -> Option<String>) -> Self {
        lookup(ENV_CI_EVIDENCE_ROUTING)
            .map(|value| Self::from_value(&value))
            .unwrap_or(Self::DisabledClean)
    }

    /// Read the gate from [`ENV_CI_EVIDENCE_ROUTING`]. Absent is off.
    pub(crate) fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Whether a *new* route may be reserved, charged, or handed a provider
    /// call. False for both off-ish states.
    pub(crate) fn admits_new_routes(self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// Whether the feature owns any durable state at all.
    ///
    /// `Quiescing` answers true: its rows exist, its owners must finalize them,
    /// and the recovery sweep must keep running over them. Only
    /// `DisabledClean` may skip the route layer entirely.
    pub(crate) fn owns_routes(self) -> bool {
        !matches!(self, Self::DisabledClean)
    }

    /// The durable spelling used in reports and logs.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Quiescing => "quiescing",
            Self::DisabledClean => "disabled_clean",
        }
    }
}
