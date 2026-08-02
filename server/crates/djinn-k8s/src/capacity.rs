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
    pub protected_population_complete: bool,
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
    protected_population_complete: true,
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
    #[error("memory quantities cannot be negative")]
    NegativeMemory,
    #[error("pod quantities cannot be negative")]
    NegativePods,
    #[error("capacity observation is missing CPU")]
    MissingCpu,
    #[error("capacity observation is missing memory")]
    MissingMemory,
    #[error("capacity observation is missing pods")]
    MissingPods,
    #[error("capacity arithmetic underflowed")]
    Underflow,
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
    if !inputs.protected_population_complete {
        return conservative(CapacityError::IncompleteProtectedPopulation);
    }
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
    // `pods` and `idle_cost` are non-negative, so a successful checked
    // multiplication is already inside CpuMillicores' complete i64 domain.
    // Every value in that domain has an exact Kubernetes DecimalSI `m`
    // representation; there is no additional fallible conversion here.
    let binding_cpu = CpuMillicores(binding_cpu);
    CapacityOutcome::Derived(DerivedCapacity {
        pods,
        binding_cpu,
        compile_slots,
    })
}

/// Non-negative byte quantity used by vector capacity arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryBytes(i64);

impl MemoryBytes {
    pub fn new(value: i64) -> Result<Self, CapacityError> {
        (value >= 0)
            .then_some(Self(value))
            .ok_or(CapacityError::NegativeMemory)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Non-negative pod count used both for node capacity and PodSet cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PodCount(i64);

impl PodCount {
    pub fn new(value: i64) -> Result<Self, CapacityError> {
        (value >= 0)
            .then_some(Self(value))
            .ok_or(CapacityError::NegativePods)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// A complete, non-negative CPU, memory, and pod capacity vector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceVector {
    pub cpu: CpuMillicores,
    pub memory: MemoryBytes,
    pub pods: PodCount,
}

impl ResourceVector {
    pub const ZERO: Self = Self {
        cpu: CpuMillicores(0),
        memory: MemoryBytes(0),
        pods: PodCount(0),
    };

    /// Add each dimension without allowing an overflowing observation to wrap.
    pub fn checked_add(self, rhs: Self) -> Result<Self, CapacityError> {
        Ok(Self {
            cpu: CpuMillicores(
                self.cpu
                    .get()
                    .checked_add(rhs.cpu.get())
                    .ok_or(CapacityError::Overflow)?,
            ),
            memory: MemoryBytes(
                self.memory
                    .get()
                    .checked_add(rhs.memory.get())
                    .ok_or(CapacityError::Overflow)?,
            ),
            pods: PodCount(
                self.pods
                    .get()
                    .checked_add(rhs.pods.get())
                    .ok_or(CapacityError::Overflow)?,
            ),
        })
    }

    /// Subtract each dimension without allowing an exhausted dimension to wrap.
    pub fn checked_sub(self, rhs: Self) -> Result<Self, CapacityError> {
        if self.cpu.get() < rhs.cpu.get()
            || self.memory.get() < rhs.memory.get()
            || self.pods.get() < rhs.pods.get()
        {
            return Err(CapacityError::Underflow);
        }
        Ok(Self {
            cpu: CpuMillicores(
                self.cpu
                    .get()
                    .checked_sub(rhs.cpu.get())
                    .ok_or(CapacityError::Underflow)?,
            ),
            memory: MemoryBytes(
                self.memory
                    .get()
                    .checked_sub(rhs.memory.get())
                    .ok_or(CapacityError::Underflow)?,
            ),
            pods: PodCount(
                self.pods
                    .get()
                    .checked_sub(rhs.pods.get())
                    .ok_or(CapacityError::Underflow)?,
            ),
        })
    }
}

/// A partially observed resource vector. `None` is deliberately distinct from
/// zero: omitted dimensions make admission fail closed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceVectorInput {
    pub cpu: Option<CpuMillicores>,
    pub memory: Option<MemoryBytes>,
    pub pods: Option<PodCount>,
}

impl ResourceVectorInput {
    pub const fn complete(vector: ResourceVector) -> Self {
        Self {
            cpu: Some(vector.cpu),
            memory: Some(vector.memory),
            pods: Some(vector.pods),
        }
    }

    fn require_complete(self) -> Result<ResourceVector, CapacityError> {
        Ok(ResourceVector {
            cpu: self.cpu.ok_or(CapacityError::MissingCpu)?,
            memory: self.memory.ok_or(CapacityError::MissingMemory)?,
            pods: self.pods.ok_or(CapacityError::MissingPods)?,
        })
    }
}

/// Inputs at the arithmetic seam. Observation and rendering layers supply the
/// vectors; this module only validates and derives capacity from them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceVectorDerivationInputs {
    pub protected_population_complete: bool,
    pub allocatable: ResourceVectorInput,
    pub protected: ResourceVectorInput,
    pub headroom: ResourceVectorInput,
    pub podset_cost: ResourceVectorInput,
}

/// Raw post-reserve CPU/memory plus the whole-PodSet admission limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DerivedResourceCapacity {
    /// Allocatable minus protected minus configured headroom. CPU and memory
    /// remain raw quantities, rather than being rounded to PodSet multiples.
    pub raw: ResourceVector,
    /// The minimum whole-PodSet fit across raw CPU, memory, and pods.
    pub admitted_podsets: PodCount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceVectorOutcome {
    Derived(DerivedResourceCapacity),
    Conservative {
        capacity: ResourceVector,
        reason: CapacityError,
    },
}

/// Derive vector capacity with checked component-wise reservation arithmetic.
///
/// Any incomplete input, exhausted resource, overflow, or zero PodSet cost is
/// conservative. The conservative result contains no positive capacity.
#[must_use]
pub fn derive_resource_vector(inputs: ResourceVectorDerivationInputs) -> ResourceVectorOutcome {
    let derived = (|| {
        if !inputs.protected_population_complete {
            return Err(CapacityError::IncompleteProtectedPopulation);
        }
        let allocatable = inputs.allocatable.require_complete()?;
        let protected = inputs.protected.require_complete()?;
        let headroom = inputs.headroom.require_complete()?;
        let cost = inputs.podset_cost.require_complete()?;
        if cost.cpu.get() == 0 || cost.memory.get() == 0 || cost.pods.get() == 0 {
            return Err(CapacityError::ZeroCost);
        }
        let raw = allocatable.checked_sub(protected)?.checked_sub(headroom)?;
        let admitted_podsets = PodCount(
            (raw.cpu.get() / cost.cpu.get())
                .min(raw.memory.get() / cost.memory.get())
                .min(raw.pods.get() / cost.pods.get()),
        );
        Ok(DerivedResourceCapacity {
            raw,
            admitted_podsets,
        })
    })();
    match derived {
        Ok(capacity) => ResourceVectorOutcome::Derived(capacity),
        Err(reason) => ResourceVectorOutcome::Conservative {
            capacity: ResourceVector::ZERO,
            reason,
        },
    }
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
                protected_population_complete: true,
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
                    protected_population_complete: true,
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
        assert_eq!(CpuMillicores::new(-1), Err(CapacityError::NegativeCpu));
        assert_eq!(
            format!("{}m", CpuMillicores::new(i64::MAX).unwrap().get()),
            "9223372036854775807m"
        );
        assert_eq!(
            scheduler_effective_request(
                [CpuMillicores(i64::MAX), CpuMillicores(1)],
                std::iter::empty(),
            ),
            Err(CapacityError::Overflow)
        );
        let incomplete = derive(
            DerivationInputs {
                protected_population_complete: false,
                allocatable: CpuMillicores(100),
                protected: CpuMillicores(0),
                idle_cost: CpuMillicores(1),
                compile_cost: CpuMillicores(1),
            },
            fail,
        );
        assert_eq!(
            incomplete,
            CapacityOutcome::Conservative {
                capacity: fail,
                reason: CapacityError::IncompleteProtectedPopulation,
            }
        );
    }

    fn resources(cpu: i64, memory: i64, pods: i64) -> ResourceVectorInput {
        ResourceVectorInput::complete(ResourceVector {
            cpu: CpuMillicores::new(cpu).unwrap(),
            memory: MemoryBytes::new(memory).unwrap(),
            pods: PodCount::new(pods).unwrap(),
        })
    }

    fn vector_inputs(
        allocatable: ResourceVectorInput,
        protected: ResourceVectorInput,
        headroom: ResourceVectorInput,
        podset_cost: ResourceVectorInput,
    ) -> ResourceVectorDerivationInputs {
        ResourceVectorDerivationInputs {
            protected_population_complete: true,
            allocatable,
            protected,
            headroom,
            podset_cost,
        }
    }

    #[test]
    fn capacity_derivation_preserves_raw_cpu_memory_and_uses_vector_minimum() {
        let outcome = derive_resource_vector(vector_inputs(
            resources(16_000, 8 * 1024 * 1024 * 1024, 20),
            resources(0, 0, 0),
            resources(0, 0, 0),
            resources(1_050, 2_112 * 1024 * 1024, 1),
        ));
        assert_eq!(
            outcome,
            ResourceVectorOutcome::Derived(DerivedResourceCapacity {
                raw: ResourceVector {
                    cpu: CpuMillicores(16_000),
                    memory: MemoryBytes(8 * 1024 * 1024 * 1024),
                    pods: PodCount(20),
                },
                admitted_podsets: PodCount(3),
            })
        );
    }

    #[test]
    fn capacity_derivation_underflow_is_zero_vector_not_wrapped() {
        let outcome = derive_resource_vector(vector_inputs(
            resources(2_000, 2 * 1024 * 1024 * 1024, 4),
            resources(1_500, 1_536 * 1024 * 1024, 3),
            resources(750, 1024 * 1024 * 1024, 2),
            resources(1, 1, 1),
        ));
        assert_eq!(
            outcome,
            ResourceVectorOutcome::Conservative {
                capacity: ResourceVector::ZERO,
                reason: CapacityError::Underflow,
            }
        );
    }

    #[test]
    fn capacity_derivation_rejects_missing_zero_cost_and_checked_arithmetic() {
        let valid = resources(1, 1, 1);
        for (input, reason) in [
            (
                vector_inputs(
                    ResourceVectorInput { cpu: None, ..valid },
                    valid,
                    resources(0, 0, 0),
                    valid,
                ),
                CapacityError::MissingCpu,
            ),
            (
                vector_inputs(
                    valid,
                    ResourceVectorInput {
                        memory: None,
                        ..valid
                    },
                    resources(0, 0, 0),
                    valid,
                ),
                CapacityError::MissingMemory,
            ),
            (
                vector_inputs(
                    valid,
                    valid,
                    ResourceVectorInput {
                        pods: None,
                        ..valid
                    },
                    valid,
                ),
                CapacityError::MissingPods,
            ),
            (
                vector_inputs(valid, valid, resources(0, 0, 0), resources(0, 1, 1)),
                CapacityError::ZeroCost,
            ),
        ] {
            assert_eq!(
                derive_resource_vector(input),
                ResourceVectorOutcome::Conservative {
                    capacity: ResourceVector::ZERO,
                    reason,
                }
            );
        }

        let max = ResourceVector {
            cpu: CpuMillicores(i64::MAX),
            memory: MemoryBytes(i64::MAX),
            pods: PodCount(i64::MAX),
        };
        let one = resources(1, 1, 1).require_complete().unwrap();
        assert_eq!(max.checked_add(one), Err(CapacityError::Overflow));
        assert_eq!(
            ResourceVector::ZERO.checked_sub(one),
            Err(CapacityError::Underflow)
        );
        assert_eq!(MemoryBytes::new(-1), Err(CapacityError::NegativeMemory));
        assert_eq!(PodCount::new(-1), Err(CapacityError::NegativePods));
    }
}
