//! Asymmetric damping shared by admission quota and compile-slot capacity.

use std::time::Duration;
use tokio::time::Instant;

pub const REQUIRED_AGREEING_SAMPLES: u8 = 3;
pub const MINIMUM_GROWTH_DWELL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingQuota {
    Pods(i64),
    CpuMillicores(i64),
}

impl BindingQuota {
    fn same_resource(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Pods(_), Self::Pods(_)) | (Self::CpuMillicores(_), Self::CpuMillicores(_))
        )
    }
    fn value(self) -> i64 {
        match self {
            Self::Pods(v) | Self::CpuMillicores(v) => v,
        }
    }
    fn with_value(self, value: i64) -> Self {
        match self {
            Self::Pods(_) => Self::Pods(value),
            Self::CpuMillicores(_) => Self::CpuMillicores(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityVector {
    pub binding: BindingQuota,
    pub compile_slots: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleKind {
    Periodic,
    Watch,
}

#[derive(Debug)]
pub struct CapacityDamper {
    enforced: CapacityVector,
    candidate: Option<CapacityVector>,
    agreeing: u8,
    leader_since: Instant,
}

impl CapacityDamper {
    #[must_use]
    pub fn new(live_binding: BindingQuota, fail_safe_k: i64, now: Instant) -> Self {
        Self {
            enforced: CapacityVector {
                binding: live_binding,
                compile_slots: fail_safe_k,
            },
            candidate: None,
            agreeing: 0,
            leader_since: now,
        }
    }

    #[must_use]
    pub const fn enforced(&self) -> CapacityVector {
        self.enforced
    }

    pub fn reset_after_error(&mut self, fail_safe_k: i64) -> CapacityVector {
        self.candidate = None;
        self.agreeing = 0;
        self.enforced.compile_slots = self.enforced.compile_slots.min(fail_safe_k);
        self.enforced
    }

    pub fn observe(
        &mut self,
        observed: CapacityVector,
        kind: SampleKind,
        now: Instant,
    ) -> CapacityVector {
        if !self.enforced.binding.same_resource(observed.binding) {
            return self.enforced;
        }
        let binding_shrink = observed.binding.value() < self.enforced.binding.value();
        let k_shrink = observed.compile_slots < self.enforced.compile_slots;
        if binding_shrink {
            self.enforced.binding = self.enforced.binding.with_value(observed.binding.value());
        }
        if k_shrink {
            self.enforced.compile_slots = observed.compile_slots;
        }

        if kind == SampleKind::Watch {
            return self.enforced;
        }
        if self.candidate == Some(observed) {
            self.agreeing = self.agreeing.saturating_add(1);
        } else {
            self.candidate = Some(observed);
            self.agreeing = 1;
        }

        if self.agreeing >= REQUIRED_AGREEING_SAMPLES
            && now.duration_since(self.leader_since) >= MINIMUM_GROWTH_DWELL
        {
            if observed.binding.value() > self.enforced.binding.value() {
                self.enforced.binding = observed.binding;
            }
            if observed.compile_slots > self.enforced.compile_slots {
                self.enforced.compile_slots = observed.compile_slots;
            }
        }
        self.enforced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_damping_shrinks_components_immediately_and_grows_together_slowly() {
        let t0 = Instant::now();
        let mut d = CapacityDamper::new(BindingQuota::Pods(10), 4, t0);
        assert_eq!(
            d.observe(
                CapacityVector {
                    binding: BindingQuota::Pods(8),
                    compile_slots: 6
                },
                SampleKind::Periodic,
                t0
            ),
            CapacityVector {
                binding: BindingQuota::Pods(8),
                compile_slots: 4
            }
        );
        let high = CapacityVector {
            binding: BindingQuota::Pods(12),
            compile_slots: 6,
        };
        d.observe(high, SampleKind::Watch, t0 + MINIMUM_GROWTH_DWELL);
        assert_eq!(
            d.observe(high, SampleKind::Periodic, t0 + MINIMUM_GROWTH_DWELL),
            CapacityVector {
                binding: BindingQuota::Pods(8),
                compile_slots: 4
            }
        );
        assert_eq!(
            d.observe(
                high,
                SampleKind::Periodic,
                t0 + MINIMUM_GROWTH_DWELL + Duration::from_secs(30)
            ),
            CapacityVector {
                binding: BindingQuota::Pods(8),
                compile_slots: 4
            }
        );
        assert_eq!(
            d.observe(
                high,
                SampleKind::Periodic,
                t0 + MINIMUM_GROWTH_DWELL + Duration::from_secs(60)
            ),
            high
        );
    }

    #[test]
    fn quota_damping_restart_requires_fresh_agreement() {
        let t0 = Instant::now();
        let mut d = CapacityDamper::new(BindingQuota::CpuMillicores(7_500), 1, t0);
        let high = CapacityVector {
            binding: BindingQuota::CpuMillicores(36_000),
            compile_slots: 12,
        };
        for n in 0..2 {
            d.observe(
                high,
                SampleKind::Periodic,
                t0 + MINIMUM_GROWTH_DWELL + Duration::from_secs(30 * n),
            );
        }
        assert_eq!(d.enforced().compile_slots, 1);
        assert_eq!(d.reset_after_error(1).compile_slots, 1);
        let low = CapacityVector {
            binding: BindingQuota::CpuMillicores(6_000),
            compile_slots: 0,
        };
        assert_eq!(
            d.observe(low, SampleKind::Periodic, t0 + MINIMUM_GROWTH_DWELL),
            low
        );
    }

    #[test]
    fn quota_damping_protected_gap_cannot_raise_k() {
        let t0 = Instant::now();
        let mut d = CapacityDamper::new(BindingQuota::Pods(10), 2, t0);
        let optimistic = CapacityVector {
            binding: BindingQuota::Pods(12),
            compile_slots: 4,
        };
        d.observe(optimistic, SampleKind::Periodic, t0 + MINIMUM_GROWTH_DWELL);
        d.observe(
            optimistic,
            SampleKind::Periodic,
            t0 + MINIMUM_GROWTH_DWELL + Duration::from_secs(30),
        );
        assert_eq!(d.enforced().compile_slots, 2);
        assert_eq!(d.reset_after_error(2).compile_slots, 2);
        let contraction = CapacityVector {
            binding: BindingQuota::Pods(8),
            compile_slots: 1,
        };
        assert_eq!(
            d.observe(
                contraction,
                SampleKind::Periodic,
                t0 + MINIMUM_GROWTH_DWELL + Duration::from_secs(60)
            ),
            contraction
        );
    }
}
