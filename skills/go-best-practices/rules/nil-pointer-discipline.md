---
title: Pointer Discipline for Nil Safety
impact: HIGH
impactDescription: Prevents nil pointer dereferences, a common source of panics
tags: nil, pointers, safety, defensive-programming
---

## Pointer Discipline for Nil Safety

**Impact: HIGH**

Nil pointer dereferences are the second most common crash in Go. Use pointers only when necessary, and check them at reception if nil is not acceptable.

**When to use pointer (`*T`):**

| Use pointer | Example |
|-------------|---------|
| Optional field (can be absent) | `PaidAt *time.Time` - payment may not be paid yet |
| Large struct (>64 bytes) passed frequently | `ProcessLargeConfig(*Config)` |
| Need to modify original | `func (u *User) SetName(name string)` |
| Interface compliance required | Returning `nil` for "not found" cases |

**When to use value (`T`):**

| Use value | Example |
|-----------|---------|
| Required field (always present) | `ID string` for entities with mandatory IDs |
| Small, immutable types | `Point{X, Y float64}` |
| Read-only access | `user.Name()` getter |
| Primitive types | `int`, `string`, `bool` |

**Incorrect (unnecessary pointer):**

```go
// BAD: Pointer for no reason - nil check needed everywhere
func ProcessOrder(order *Order) error {
    if order == nil { // Defensive check needed
        return errors.New("order is nil")
    }
    // ...
}

// BAD: Pointer when value is sufficient
func (s *Service) ValidateName(name *string) error {
    if name == nil {
        return errors.New("name is nil")
    }
    if len(*name) < 3 { // Dereferencing everywhere
        return errors.New("name too short")
    }
    // ...
}
```

**Correct (use value when nil doesn't make sense):**

```go
// GOOD: Value type - no nil check needed
func ProcessOrder(order Order) error {
    // order is always valid, no nil check needed
    if order.ID == "" {
        return errors.New("order ID is empty")
    }
    // ...
}

// GOOD: Pointer for "not found" case
func FindOrder(ctx context.Context, id OrderID) (*Order, error) {
    // Returns (nil, ErrNotFound) when order doesn't exist
    // Returns (*Order, nil) when found
    // Never returns (nil, nil)
    order, err := db.Get(ctx, id)
    if err == sql.ErrNoRows {
        return nil, fmt.Errorf("order %s: %w", id, ErrNotFound)
    }
    if err != nil {
        return nil, fmt.Errorf("get order %s: %w", id, err)
    }
    return order, nil
}
```

**Check pointers at reception:**

```go
func (s *Service) UpdateCustomer(ctx context.Context, customer *Customer) error {
    if customer == nil {
        return errors.New("customer is required")
    }
    // Safe to use customer from here
}
```

**Optional fields pattern:**

```go
type InvoiceFilter struct {
    CustomerID *CustomerID  // nil means "don't filter by customer"
    MinAmount  *Money       // nil means "no minimum"
    Status     *InvoiceStatus
}

// Usage is straightforward
filter := InvoiceFilter{
    CustomerID: &customerID,  // Filter by this customer
    MinAmount:  nil,          // No minimum
}
```

**Avoid `(nil, nil)` returns:**

```go
// BAD: Caller can't distinguish between "not found" and "found with nil value"
func GetConfig() (*Config, error) {
    if !hasConfig() {
        return nil, nil  // Ambiguous!
    }
    return loadConfig()
}

// GOOD: Always return error or valid pointer
func GetConfig() (*Config, error) {
    if !hasConfig() {
        return nil, ErrNotConfigured  // Clear!
    }
    return loadConfig()
}
```

Reference: [Go Code Review Comments - Receiver Type](https://go.dev/wiki/CodeReviewComments#receiver-type)
