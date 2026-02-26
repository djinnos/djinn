---
title: Explicit Transaction Boundaries
impact: HIGH
impactDescription: Makes transaction boundaries visible and ensures proper cleanup
tags: database, transactions, consistency, cleanup
---

## Explicit Transaction Boundaries

**Impact: HIGH**

Make transaction boundaries visible and always use proper cleanup. Hidden transactions lead to bugs; explicit ones make the code self-documenting.

**Incorrect (no transaction, inconsistent state possible):**

```go
func (s *Service) CreateInvoiceWithItems(ctx context.Context, inv Invoice, items []LineItem) error {
    if err := s.invoiceRepo.Create(ctx, inv); err != nil {
        return err
    }
    // If this fails, we have an invoice without items - inconsistent state!
    for _, item := range items {
        if err := s.lineItemRepo.Create(ctx, item); err != nil {
            return err  // Partial state!
        }
    }
    return nil
}
```

**Correct (explicit transaction with proper cleanup):**

```go
func (s *Service) CreateInvoiceWithItems(ctx context.Context, inv Invoice, items []LineItem) error {
    tx, err := s.db.BeginTx(ctx, nil)
    if err != nil {
        return fmt.Errorf("begin transaction: %w", err)
    }
    defer tx.Rollback()  // No-op if committed, safe cleanup if panics

    if err := s.invoiceRepo.CreateTx(ctx, tx, inv); err != nil {
        return fmt.Errorf("create invoice: %w", err)
    }

    for _, item := range items {
        if err := s.lineItemRepo.CreateTx(ctx, tx, item); err != nil {
            return fmt.Errorf("create line item: %w", err)
        }
    }

    if err := tx.Commit(); err != nil {
        return fmt.Errorf("commit: %w", err)
    }
    return nil
}
```

**Repository interface supports transactions:**

```go
type InvoiceRepo interface {
    Create(ctx context.Context, inv *Invoice) error
    CreateTx(ctx context.Context, tx *sql.Tx, inv *Invoice) error
    Get(ctx context.Context, id InvoiceID) (*Invoice, error)
    GetTx(ctx context.Context, tx *sql.Tx, id InvoiceID) (*Invoice, error)
}

// Implementation uses tx when provided
func (r *invoiceRepo) CreateTx(ctx context.Context, tx *sql.Tx, inv *Invoice) error {
    _, err := tx.ExecContext(ctx, createInvoiceSQL, inv.ID, inv.CustomerID, /* ... */)
    if err != nil {
        return fmt.Errorf("insert invoice: %w", err)
    }
    return nil
}
```

**Transaction wrapper for reusable patterns:**

```go
func WithTransaction(ctx context.Context, db *sql.DB, fn func(*sql.Tx) error) error {
    tx, err := db.BeginTx(ctx, nil)
    if err != nil {
        return fmt.Errorf("begin transaction: %w", err)
    }
    defer tx.Rollback()  // Safe cleanup

    if err := fn(tx); err != nil {
        return err  // Rollback happens in defer
    }

    if err := tx.Commit(); err != nil {
        return fmt.Errorf("commit: %w", err)
    }
    return nil
}

// Usage
func (s *Service) Transfer(ctx context.Context, from, to AccountID, amount Money) error {
    return WithTransaction(ctx, s.db, func(tx *sql.Tx) error {
        if err := s.debit(ctx, tx, from, amount); err != nil {
            return err
        }
        if err := s.credit(ctx, tx, to, amount); err != nil {
            return err
        }
        return nil
    })
}
```

**Test transaction rollback:**

```go
func TestService_CreateInvoice_RepositoryFailure(t *testing.T) {
    db := testdb.New(t)
    repo := &FailingInvoiceRepo{}
    service := NewInvoiceService(repo, customerRepo, db)

    _, err := service.CreateInvoiceWithItems(ctx, invoice, items)
    require.Error(t, err)

    // Verify no partial state
    var count int
    db.QueryRowContext(ctx, "SELECT COUNT(*) FROM invoices").Scan(&count)
    assert.Equal(t, 0, count, "no invoices should exist after rollback")
}
```

**Transaction isolation levels:**

```go
// Use appropriate isolation level for the use case
tx, err := db.BeginTx(ctx, &sql.TxOptions{
    Isolation: sql.LevelSerializable, // Strictest, use for financial ops
    // Isolation: sql.LevelReadCommitted, // Default, good for most reads
})
```

Reference: [Go database/sql Transactions](https://go.dev/doc/database/execute-transactions)
