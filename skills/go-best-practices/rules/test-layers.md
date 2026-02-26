---
title: Testing Strategy by Layer
impact: HIGH
impactDescription: Different layers need different testing approaches for speed and confidence
tags: testing, layers, strategy, architecture
---

## Testing Strategy by Layer

**Impact: HIGH**

Use different testing strategies for different layers. Unit tests are fast but mock too much; E2E tests are slow but catch integration bugs. Layer your tests appropriately.

| Layer | Test Type | Dependencies | Speed |
|-------|-----------|--------------|-------|
| Repository | Integration | Real database | Slow |
| Service | Unit | Mocked repositories | Fast |
| Full flows | E2E | Real database + mocked externals | Slowest |

**Repository tests verify SQL:**

```go
func TestInvoiceRepo_Create(t *testing.T) {
    db := testdb.New(t)  // Spins up Postgres, runs migrations
    repo := NewInvoiceRepo(db)

    invoice := &Invoice{
        ID:         NewInvoiceID(),
        CustomerID: "cust_123",
        Total:      Money{Cents: 1000, Currency: USD},
        Status:     InvoiceStatusDraft,
    }

    err := repo.Create(ctx, invoice)
    require.NoError(t, err)

    // Verify it's actually in the database
    saved, err := repo.Get(ctx, invoice.ID)
    require.NoError(t, err)
    assert.Equal(t, invoice.CustomerID, saved.CustomerID)
}
```

**Service tests verify business logic:**

```go
func TestInvoiceService_Create(t *testing.T) {
    mockRepo := &MockInvoiceRepo{
        CreateFunc: func(ctx context.Context, inv *Invoice) error {
            return nil
        },
    }
    mockCustomerRepo := &MockCustomerRepo{
        GetFunc: func(ctx context.Context, id CustomerID) (*Customer, error) {
            return &Customer{
                ID: id,
                Status: CustomerStatusActive,
                CreditLimitCents: 100000,
            }, nil
        },
    }
    service := NewInvoiceService(mockRepo, mockCustomerRepo)

    invoice, err := service.Create(ctx, CreateInvoiceParams{
        CustomerID: "cust_123",
        Items:      []LineItem{{UnitPrice: Money{Cents: 1000}}},
    })

    require.NoError(t, err)
    assert.Equal(t, InvoiceStatusDraft, invoice.Status)
}
```

**E2E tests verify complete flows:**

```go
func TestInvoiceFlow_E2E(t *testing.T) {
    db := testdb.New(t)
    invoiceRepo := NewInvoiceRepo(db)
    customerRepo := NewCustomerRepo(db)
    service := NewInvoiceService(invoiceRepo, customerRepo)

    customer := testdata.CreateCustomer(t, db, WithCreditLimit(100000))

    // Create -> Send -> Pay
    invoice, err := service.Create(ctx, CreateInvoiceParams{
        CustomerID: customer.ID,
        Items:      []LineItem{{Description: "Service", UnitPrice: Money{Cents: 50000}}},
        DueDate:    time.Now().Add(30 * 24 * time.Hour),
    })
    require.NoError(t, err)

    err = service.Send(ctx, invoice.ID)
    require.NoError(t, err)

    sent, err := service.Get(ctx, invoice.ID)
    require.NoError(t, err)
    assert.Equal(t, InvoiceStatusSent, sent.Status)
}
```

**What each layer catches:**

```go
// Repository tests catch:
// - Wrong column names
// - Bad joins
// - Incorrect WHERE clauses
// - Postgres-specific behavior

// Service tests catch:
// - Business logic errors
// - Edge cases in validation
// - Error handling paths

// E2E tests catch:
// - Idempotency bugs
// - Race conditions
// - Integration issues
// - Transaction boundary problems
```

**Test pyramid:**

```
        /
       / \  E2E (few tests, slow)
      /   \
     /-----\  Service (many tests, fast)
    /       \
   /---------\  Repository (some tests, slow)
  /           \
 /_____________\ Unit helpers (many tests, instant)
```

Reference: [Testing Pyramid](https://martinfowler.com/articles/practical-test-pyramid.html)
