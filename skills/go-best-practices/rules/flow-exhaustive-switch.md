---
title: Handle All Cases in Switch Statements
impact: MEDIUM
impactDescription: Catches bugs when new enum values are added
tags: control-flow, switch, enums, exhaustiveness
---

## Handle All Cases in Switch Statements

**Impact: MEDIUM**

When switching on a type or enum, handle all cases. Use `default` to catch unexpected values and return an error. This catches bugs when new enum values are added.

**Incorrect (missing cases, no default):**

```go
// BAD: Missing cases, silent skip
type PaymentStatus string

const (
    PaymentStatusPending   PaymentStatus = "pending"
    PaymentStatusCompleted PaymentStatus = "completed"
    PaymentStatusFailed    PaymentStatus = "failed"
    PaymentStatusRefunded  PaymentStatus = "refunded"  // Added later
)

func (s PaymentStatus) String() string {
    switch s {
    case PaymentStatusPending:
        return "pending"
    case PaymentStatusCompleted:
        return "completed"
    case PaymentStatusFailed:
        return "failed"
    }
    return ""  // Silent bug: new status returns empty string
}
```

**Correct (all cases handled with error default):**

```go
// GOOD: All cases handled, default returns error
func (s PaymentStatus) String() (string, error) {
    switch s {
    case PaymentStatusPending:
        return "pending", nil
    case PaymentStatusCompleted:
        return "completed", nil
    case PaymentStatusFailed:
        return "failed", nil
    case PaymentStatusRefunded:
        return "refunded", nil
    default:
        return "", fmt.Errorf("unhandled payment status: %s", s)
    }
}
```

**Compile-time check for exhaustiveness:**

```go
// Pattern: Add sentinel for compile-time exhaustiveness checking
type PaymentStatus string

const (
    PaymentStatusPending   PaymentStatus = "pending"
    PaymentStatusCompleted PaymentStatus = "completed"
    PaymentStatusFailed    PaymentStatus = "failed"
)

// compileCheckPaymentStatus ensures all statuses are handled
// Update this function when adding new statuses
func compileCheckPaymentStatus() {
    var s PaymentStatus
    switch s {
    case PaymentStatusPending, PaymentStatusCompleted, PaymentStatusFailed:
    default:
    }
}

// If you add PaymentStatusRefunded, the above won't compile until updated
```

**Use exhaustive linter:**

```yaml
# .golangci.yml
linters:
  enable:
    - exhaustive  # Checks enum switch statements are exhaustive
```

**Type switches:**

```go
// GOOD: Handle all types, error on unknown
func ProcessEvent(event Event) error {
    switch e := event.(type) {
    case *InvoiceCreated:
        return handleInvoiceCreated(e)
    case *PaymentReceived:
        return handlePaymentReceived(e)
    case *PaymentFailed:
        return handlePaymentFailed(e)
    default:
        return fmt.Errorf("unknown event type: %T", event)
    }
}
```

**The tradeoff:**

```go
// This signature doesn't implement fmt.Stringer (which requires String() string)
// The tradeoff is worth it: we don't panic on unknown values
func (s Status) String() (string, error) { ... }

// Can't use: fmt.Printf("%s", status) directly
// Instead: str, err := status.String()
// Or provide a helper that panics only when you explicitly call it
```

**Error vs panic decision:**

```go
// Return error when: value comes from external source (API, DB, config)
func ParseStatus(s string) (Status, error) { ... }

// Return error when: value is computed at runtime
func (s Status) String() (string, error) { ... }

// Panic may be OK when: value is set at compile time
const defaultStatus = PaymentStatusPending // Compile-time constant
```

Reference: [Exhaustive Linter](https://github.com/nishanths/exhaustive)
