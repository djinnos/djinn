---
title: E2E Tests for Critical Behaviors
impact: HIGH
impactDescription: Catches bugs that unit tests miss: idempotency, races, integration issues
tags: testing, e2e, idempotency, race-conditions
---

## E2E Tests for Critical Behaviors

**Impact: HIGH**

E2E tests use real dependencies to verify complete behaviors. They catch bugs that mocked unit tests miss: idempotency failures, race conditions, and transaction issues. Run them even though they're slow.

**Idempotency test:**

```go
func TestPaymentService_ProcessPayment_Idempotent(t *testing.T) {
    db := testdb.New(t)
    repo := NewPaymentRepo(db)
    gateway := &FakePaymentGateway{
        ChargeFunc: func(ctx context.Context, req ChargeRequest) (*ChargeResponse, error) {
            return &ChargeResponse{ID: "ch_123", Status: "succeeded"}, nil
        },
    }
    service := NewPaymentService(repo, gateway)

    params := ProcessPaymentParams{
        IdempotencyKey: "key-123",
        Amount:         Money{Cents: 1000},
    }

    // First call creates payment
    payment1, err := service.ProcessPayment(ctx, params)
    require.NoError(t, err)

    // Second call with same key returns same payment
    payment2, err := service.ProcessPayment(ctx, params)
    require.NoError(t, err)

    assert.Equal(t, payment1.ID, payment2.ID)
    assert.Equal(t, 1, gateway.ChargeCallCount, "gateway should only be called once")

    // Verify only one payment in database
    payments, _ := repo.ListByIdempotencyKey(ctx, "key-123")
    assert.Len(t, payments, 1)
}
```

**Race condition test:**

```go
func TestPaymentService_ProcessPayment_Concurrent(t *testing.T) {
    db := testdb.New(t)
    repo := NewPaymentRepo(db)
    gateway := &FakePaymentGateway{
        ChargeFunc: func(ctx context.Context, req ChargeRequest) (*ChargeResponse, error) {
            time.Sleep(10 * time.Millisecond)  // Simulate latency
            return &ChargeResponse{ID: uuid.New().String()}, nil
        },
    }
    service := NewPaymentService(repo, gateway)

    params := ProcessPaymentParams{
        IdempotencyKey: "concurrent-key",
        Amount:         Money{Cents: 1000},
    }

    // Fire 10 concurrent requests
    var wg sync.WaitGroup
    results := make(chan *Payment, 10)
    errs := make(chan error, 10)

    for i := 0; i < 10; i++ {
        wg.Add(1)
        go func() {
            defer wg.Done()
            payment, err := service.ProcessPayment(ctx, params)
            if err != nil {
                errs <- err
            } else {
                results <- payment
            }
        }()
    }
    wg.Wait()
    close(results)
    close(errs)

    // Collect results
    var paymentIDs []PaymentID
    for p := range results {
        paymentIDs = append(paymentIDs, p.ID)
    }

    // All requests should return the same payment
    require.NotEmpty(t, paymentIDs)
    for _, id := range paymentIDs {
        assert.Equal(t, paymentIDs[0], id, "all requests should return same payment")
    }

    // Only one payment in database
    payments, _ := repo.ListByIdempotencyKey(ctx, "concurrent-key")
    assert.Len(t, payments, 1, "only one payment should be created")
}
```

**Transaction boundary test:**

```go
func TestInvoiceService_Create_RepositoryFailure(t *testing.T) {
    db := testdb.New(t)
    repo := NewFailingInvoiceRepo(db) // Injects failures
    service := NewInvoiceService(repo, customerRepo)

    _, err := service.Create(ctx, validParams)
    require.Error(t, err)

    // Verify no partial state exists
    invoices, _ := repo.ListByCustomer(ctx, validParams.CustomerID)
    assert.Empty(t, invoices, "no invoice should exist after failure")
}
```

**When to write E2E tests:**

```go
// DO write E2E tests for:
// - Payment processing
// - Financial calculations
// - Critical state transitions
// - Concurrent operations
// - Idempotency requirements

// DON'T write E2E tests for:
// - Simple CRUD operations (unit test sufficient)
// - Validation logic (unit test sufficient)
// - Mapping/transformation functions
```

**Keep E2E tests focused:**

```go
// GOOD: Tests one critical behavior
func TestPayment_IdempotentKey_PreventsDuplicateCharges(t *testing.T)

// BAD: Too many scenarios in one test
func TestPaymentService_AllScenarios(t *testing.T)  // Split into multiple tests
```

**Run E2E in CI even though they're slow:**

```yaml
# .github/workflows/test.yml
jobs:
  unit:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: go test ./... -short  # Fast unit tests

  e2e:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: go test ./... -run 'E2E$'  # Slow E2E tests
```

Reference: [E2E Testing Guide](https://microsoft.github.io/code-with-engineering-playbook/automated-testing/e2e-testing/)
