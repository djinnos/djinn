//! CPU-only arithmetic for elastic build admission.
//!
//! This module deliberately knows nothing about Kubernetes clients or memory.
//! It is the single normative implementation used by the controller and tests.

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuMillicores(i64);

impl CpuMillicores {
    pub fn new(value: i64) -> Result<Self, CapacityError> {
        (value >= 0)
            .then_some(Self(value))
            .ok_or(CapacityError::NegativeCpu)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DerivationInputs {
    pub allocatable: CpuMillicores,
    pub protected: CpuMillicores,
    pub idle_cost: CpuMillicores,
    pub compile_cost: CpuMillicores,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DerivedCapacity {
    pub pods: i64,
    pub binding_cpu: CpuMillicores,
    pub compile_slots: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailSafeCapacity {
    pub pods: i64,
    pub compile_slots: i64,
}

/// Named compatibility vector shared with the standalone SCIP regression.
/// Production inputs are observed; these literals are test evidence only.
pub const SCIP_CAPACITY_FIXTURE: DerivationInputs = DerivationInputs {
    allocatable: CpuMillicores(12_000),
    protected: CpuMillicores(4_200),
    idle_cost: CpuMillicores(750),
    compile_cost: CpuMillicores(2_800),
};
pub const SCIP_FIXTURE_COMPILE_SLOTS: i64 = 2;
pub const SCIP_PROTECTED_REQUEST_CEILING_MILLICORES: i64 = 2_200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityOutcome {
    Derived(DerivedCapacity),
    Conservative {
        capacity: FailSafeCapacity,
        reason: CapacityError,
    },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CapacityError {
    #[error("CPU quantities cannot be negative")]
    NegativeCpu,
    #[error("protected workload population is incomplete")]
    IncompleteProtectedPopulation,
    #[error("protected requests exhaust allocatable CPU")]
    NoHeadroom,
    #[error("idle and compile costs must be positive")]
    ZeroCost,
    #[error("capacity arithmetic overflowed")]
    Overflow,
    #[error("derived Kubernetes CPU quantity is unrepresentable")]
    UnrepresentableCpu,
}

/// Derive pod count (`M`), binding CPU (`M*I`), and compile slots (`K`).
///
/// Successful results are never clamped. Invalid observations return the
/// explicitly configured, resource-typed fail-safe instead of an unbounded or
/// stale value.
#[must_use]
pub fn derive(inputs: DerivationInputs, fail_safe: FailSafeCapacity) -> CapacityOutcome {
    let conservative = |reason| CapacityOutcome::Conservative {
        capacity: FailSafeCapacity {
            pods: fail_safe.pods.max(0),
            compile_slots: fail_safe.compile_slots.max(0),
        },
        reason,
    };
    if inputs.idle_cost.get() == 0 || inputs.compile_cost.get() == 0 {
        return conservative(CapacityError::ZeroCost);
    }
    let Some(headroom) = inputs.allocatable.get().checked_sub(inputs.protected.get()) else {
        return conservative(CapacityError::Overflow);
    };
    if headroom <= 0 {
        return conservative(CapacityError::NoHeadroom);
    }
    let pods = headroom / inputs.idle_cost.get();
    let compile_slots = headroom / inputs.compile_cost.get();
    let Some(binding_cpu) = pods.checked_mul(inputs.idle_cost.get()) else {
        return conservative(CapacityError::Overflow);
    };
    let Ok(binding_cpu) = CpuMillicores::new(binding_cpu) else {
        return conservative(CapacityError::UnrepresentableCpu);
    };
    CapacityOutcome::Derived(DerivedCapacity {
        pods,
        binding_cpu,
        compile_slots,
    })
}

/// Kubernetes scheduler-effective request for a Pod:
/// `max(sum(regular containers), max(init containers))`.
pub fn scheduler_effective_request(
    regular: impl IntoIterator<Item = CpuMillicores>,
    init: impl IntoIterator<Item = CpuMillicores>,
) -> Result<CpuMillicores, CapacityError> {
    let regular = regular.into_iter().try_fold(0_i64, |sum, cpu| {
        sum.checked_add(cpu.get()).ok_or(CapacityError::Overflow)
    })?;
    let init = init.into_iter().map(CpuMillicores::get).max().unwrap_or(0);
    CpuMillicores::new(regular.max(init))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(a: i64, p: i64) -> DerivedCapacity {
        let CapacityOutcome::Derived(value) = derive(
            DerivationInputs {
                allocatable: CpuMillicores::new(a).unwrap(),
                protected: CpuMillicores::new(p).unwrap(),
                idle_cost: CpuMillicores::new(750).unwrap(),
                compile_cost: CpuMillicores::new(2_800).unwrap(),
            },
            FailSafeCapacity {
                pods: 1,
                compile_slots: 1,
            },
        ) else {
            panic!("fixture must derive")
        };
        value
    }

    #[test]
    fn capacity_derivation_literal_vectors_are_normative() {
        assert_eq!(
            vector(4_000, 1_000),
            DerivedCapacity {
                pods: 4,
                binding_cpu: CpuMillicores(3_000),
                compile_slots: 1
            }
        );
        assert_eq!(
            vector(8_000, 2_000),
            DerivedCapacity {
                pods: 8,
                binding_cpu: CpuMillicores(6_000),
                compile_slots: 2
            }
        );
        assert_eq!(
            vector(12_000, 4_200),
            DerivedCapacity {
                pods: 10,
                binding_cpu: CpuMillicores(7_500),
                compile_slots: 2
            }
        );
        assert_eq!(
            vector(16_000, 4_000),
            DerivedCapacity {
                pods: 16,
                binding_cpu: CpuMillicores(12_000),
                compile_slots: 4
            }
        );
        assert_eq!(
            vector(48_000, 12_000),
            DerivedCapacity {
                pods: 48,
                binding_cpu: CpuMillicores(36_000),
                compile_slots: 12
            }
        );
    }

    #[test]
    fn capacity_derivation_failsafe_covers_invalid_observations() {
        let fail = FailSafeCapacity {
            pods: 2,
            compile_slots: 1,
        };
        for (a, p, i, c, reason) in [
            (100, 100, 1, 1, CapacityError::NoHeadroom),
            (100, 101, 1, 1, CapacityError::NoHeadroom),
            (100, 0, 0, 1, CapacityError::ZeroCost),
            (100, 0, 1, 0, CapacityError::ZeroCost),
        ] {
            let outcome = derive(
                DerivationInputs {
                    allocatable: CpuMillicores(a),
                    protected: CpuMillicores(p),
                    idle_cost: CpuMillicores(i),
                    compile_cost: CpuMillicores(c),
                },
                fail,
            );
            assert_eq!(
                outcome,
                CapacityOutcome::Conservative {
                    capacity: fail,
                    reason
                }
            );
        }
        assert_eq!(
            scheduler_effective_request(
                [CpuMillicores(300), CpuMillicores(200)],
                [CpuMillicores(750)]
            )
            .unwrap(),
            CpuMillicores(750)
        );
    }
}
