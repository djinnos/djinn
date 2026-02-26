---
title: Use Distinct Types for Domain Concepts
impact: CRITICAL
impactDescription: Prevents argument swapping bugs and makes code self-documenting
tags: domain, types, safety, primitives
---

## Use Distinct Types for Domain Concepts

**Impact: CRITICAL**

Primitive obsession causes bugs. When functions take multiple `string` or `int64` parameters, it's easy to swap arguments accidentally. The compiler can't help you. Wrap primitives in distinct domain types.

**Incorrect (primitive obsession):**

```go
// BAD: Easy to mix up arguments - all are strings
func CreatePayment(fromAccountID, toAccountID string, amount int64) error {
    // ...
}

// Called incorrectly - compiles fine, fails at runtime (or worse, silently)
CreatePayment(toAccount, fromAccount, amountInDollars) // Args swapped, wrong unit
```

**Correct (distinct domain types):**

```go
// GOOD: Distinct types prevent misuse
type AccountID string
type UserID string
type Money struct {
    Cents    int64
    Currency Currency
}

func CreatePayment(from AccountID, to UserID, amount Money) error {
    // ...
}

// These won't compile:
CreatePayment(userID, accountID, amount)  // Wrong order: types don't match
CreatePayment(from, to, 1000)             // Raw int64 instead of Money
```

**Common domain types to define:**

```go
// IDs - prevent mixing different entity IDs
type CustomerID string
type InvoiceID string
type PaymentID string

// Money - always track currency and use smallest unit
type Money struct {
    Cents    int64
    Currency Currency
}

type Currency string

const (
    USD Currency = "USD"
    EUR Currency = "EUR"
)

// Time periods - distinguish between different time concepts
type Duration time.Duration
type Timestamp time.Time

// Quantities - prevent mixing units
type Quantity int
type Percentage float64
```

**Add methods to domain types:**

```go
type Money struct {
    Cents    int64
    Currency Currency
}

func (m Money) Add(other Money) (Money, error) {
    if m.Currency != other.Currency {
        return Money{}, fmt.Errorf("cannot add %s to %s", m.Currency, other.Currency)
    }
    return Money{Cents: m.Cents + other.Cents, Currency: m.Currency}, nil
}

func (m Money) IsPositive() bool {
    return m.Cents > 0
}
```

**Limitation:** Swapping two arguments of the same type still compiles. Use clear parameter names and code review to catch these cases.

Reference: [Domain-Driven Design](https://martinfowler.com/bliki/ValueObject.html)
