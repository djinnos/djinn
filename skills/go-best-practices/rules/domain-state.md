---
title: Enforce State Transitions Through Methods
impact: HIGH
impactDescription: Prevents invalid state combinations and enforces business rules
tags: domain, state-machine, transitions, invariants
---

## Enforce State Transitions Through Methods

**Impact: HIGH**

Design state changes as methods that enforce valid transitions. This prevents invalid state combinations and ensures related fields are updated together.

**Incorrect (direct field mutation):**

```go
type Payment struct {
    ID           PaymentID
    Amount       Money
    Status       PaymentStatus
    PaidAt       *time.Time
    FailedAt     *time.Time
    FailedReason string
}

// BAD: Caller can create inconsistent state
payment.Status = PaymentStatusCompleted
// Forgot to set PaidAt - now we have a completed payment with no timestamp

payment.Status = PaymentStatusFailed
payment.FailedAt = nil // Invalid: failed but no failure time
```

**Correct (state transitions through methods):**

```go
type PaymentStatus string

const (
    PaymentStatusPending   PaymentStatus = "pending"
    PaymentStatusCompleted PaymentStatus = "completed"
    PaymentStatusFailed    PaymentStatus = "failed"
)

type Payment struct {
    ID           PaymentID
    Amount       Money
    Status       PaymentStatus
    PaidAt       *time.Time  // Set only when completed
    FailedAt     *time.Time  // Set only when failed
    FailedReason string      // Set only when failed
}

// State transitions enforce consistency
func (p *Payment) MarkCompleted(paidAt time.Time) error {
    if p.Status != PaymentStatusPending {
        return fmt.Errorf("cannot complete payment in status %s", p.Status)
    }
    p.Status = PaymentStatusCompleted
    p.PaidAt = &paidAt
    return nil
}

func (p *Payment) MarkFailed(failedAt time.Time, reason string) error {
    if p.Status != PaymentStatusPending {
        return fmt.Errorf("cannot fail payment in status %s", p.Status)
    }
    if reason == "" {
        return errors.New("failure reason is required")
    }
    p.Status = PaymentStatusFailed
    p.FailedAt = &failedAt
    p.FailedReason = reason
    return nil
}
```

**Document valid transitions:**

```go
// Payment state machine:
//
//   ┌─────────┐
//   │ Pending │
//   └────┬────┘
//        │
//   ┌────┴────┐
//   │         │
//   ▼         ▼
// ┌─────┐  ┌──────┐
// │ Paid│  │Failed│
// └─────┘  └──────┘
//
// Valid transitions:
//   Pending → Completed (via MarkCompleted)
//   Pending → Failed (via MarkFailed)
//
// Invalid transitions:
//   Completed → anything
//   Failed → anything
```

**Use typed enums:**

```go
// BAD: String allows typos
payment.Status = "completd" // Typo compiles fine

// GOOD: Typed enum catches typos at compile time
type PaymentStatus string

const (
    PaymentStatusPending   PaymentStatus = "pending"
    PaymentStatusCompleted PaymentStatus = "completed"
    PaymentStatusFailed    PaymentStatus = "failed"
)

payment.Status = PaymentStatusCompletd // Won't compile
```

**Query methods for state:**

```go
func (p *Payment) IsPending() bool {
    return p.Status == PaymentStatusPending
}

func (p *Payment) IsTerminal() bool {
    return p.Status == PaymentStatusCompleted || p.Status == PaymentStatusFailed
}

func (p *Payment) CanRetry() bool {
    return p.Status == PaymentStatusFailed
}
```

**Tradeoff:** Go allows direct field access. Accept this and use transition methods consistently. The compiler won't enforce it, but code review should.

Reference: [State Pattern](https://refactoring.guru/design-patterns/state)
