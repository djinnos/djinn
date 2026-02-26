---
title: Use Constructors for Types with Invariants
impact: HIGH
impactDescription: Ensures objects are always created in a valid state
tags: domain, constructors, invariants, validation
---

## Use Constructors for Types with Invariants

**Impact: HIGH**

Use constructors (New* functions) for types that have rules about what makes them valid. Skip constructors for simple value objects with no invariants.

**When to use constructors:**

| Condition | Example | Constructor? |
|-----------|---------|--------------|
| Derived fields | `Total` computed from line items | Yes |
| Validation rules | "must have at least one item" | Yes |
| Field interdependencies | "if status is X, field Y must be set" | Yes |
| Default values needed | ID generation, timestamps | Yes |
| Simple data bag | Just holds values, no rules | No |

**Correct (constructor for type with invariants):**

```go
type Invoice struct {
    ID         InvoiceID
    CustomerID CustomerID
    LineItems  []LineItem
    Total      Money        // Derived from LineItems
    DueDate    time.Time
    Status     InvoiceStatus
    CreatedAt  time.Time
}

func NewInvoice(customerID CustomerID, items []LineItem, dueDate time.Time) (*Invoice, error) {
    // Validate invariants
    if len(items) == 0 {
        return nil, errors.New("invoice must have at least one line item")
    }
    if dueDate.Before(time.Now()) {
        return nil, errors.New("due date must be in the future")
    }

    // Compute derived fields
    total := Money{Currency: items[0].UnitPrice.Currency}
    for _, item := range items {
        itemTotal, err := item.UnitPrice.Multiply(item.Quantity)
        if err != nil {
            return nil, fmt.Errorf("calculate item total: %w", err)
        }
        total, err = total.Add(itemTotal)
        if err != nil {
            return nil, fmt.Errorf("sum totals: %w", err)
        }
    }

    return &Invoice{
        ID:         NewInvoiceID(),      // Generate ID
        CustomerID: customerID,
        LineItems:  items,
        Total:      total,               // Derived field
        DueDate:    dueDate,
        Status:     InvoiceStatusDraft,  // Default status
        CreatedAt:  time.Now(),          // Default timestamp
    }, nil
}
```

**Correct (no constructor for simple value object):**

```go
// No constructor needed - just a data bag with no invariants
type LineItem struct {
    Description string
    Quantity    int
    UnitPrice   Money
}

// Direct struct literal is fine
item := LineItem{
    Description: "Consulting services",
    Quantity:    2,
    UnitPrice:   Money{Cents: 10000, Currency: USD},
}
```

**Pass time as parameter for testability:**

```go
// BAD: Hard to test - time is non-deterministic
func NewInvoice(customerID CustomerID, items []LineItem, dueDate time.Time) (*Invoice, error) {
    if dueDate.Before(time.Now()) { // Can't control "now" in tests
        return nil, errors.New("due date must be in the future")
    }
    // ...
}

// GOOD: Pass time as parameter
func NewInvoice(customerID CustomerID, items []LineItem, dueDate, now time.Time) (*Invoice, error) {
    if dueDate.Before(now) { // Testable
        return nil, errors.New("due date must be in the future")
    }
    // ...
}
```

**Tradeoff:** Go doesn't enforce using constructors. Callers could still set fields directly. Accept this limitation and rely on code review. The constructor documents intent.

Reference: [Effective Go - Constructors](https://go.dev/doc/effective_go#composite_literals)
