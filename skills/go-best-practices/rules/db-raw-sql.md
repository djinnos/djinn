---
title: Use Raw SQL Over ORM Magic
impact: HIGH
impactDescription: Explicit SQL prevents surprises and makes performance visible
tags: database, sql, orm, performance
---

## Use Raw SQL Over ORM Magic

**Impact: HIGH**

We use raw SQL for queries. ORMs hide what's happening; SQL makes it explicit. You can see the exact query, optimize it, and review it.

**Incorrect (ORM with hidden query):**

```go
// BAD: What query runs? How many queries?
func (s *Service) GetInvoices(userID string) ([]Invoice, error) {
    var invoices []Invoice
    s.db.Model(&Invoice{}).
        Preload("Customer").
        Preload("LineItems").
        Where("user_id = ?", userID).
        Find(&invoices)
    return invoices
}
// Hidden: N+1 query problem? Extra joins? Who knows?
```

**Correct (explicit raw SQL):**

```go
// GOOD: Clear, reviewable, optimizable
const getInvoicesByCustomer = `
    SELECT id, customer_id, amount_cents, currency, status, created_at
    FROM invoices
    WHERE customer_id = $1
      AND status = ANY($2)
    ORDER BY created_at DESC
    LIMIT $3
`

func (r *InvoiceRepo) GetByCustomer(
    ctx context.Context,
    customerID CustomerID,
    statuses []InvoiceStatus,
    limit int,
) ([]Invoice, error) {
    rows, err := r.db.QueryContext(ctx, getInvoicesByCustomer,
        customerID, pq.Array(statuses), limit)
    if err != nil {
        return nil, fmt.Errorf("query invoices: %w", err)
    }
    defer rows.Close()

    var invoices []Invoice
    for rows.Next() {
        var inv Invoice
        if err := rows.Scan(&inv.ID, &inv.CustomerID, /* ... */); err != nil {
            return nil, fmt.Errorf("scan invoice: %w", err)
        }
        invoices = append(invoices, inv)
    }
    return invoices, nil
}
```

**Named queries for complex cases:**

```go
const getInvoiceDetails = `
    SELECT 
        i.id,
        i.customer_id,
        i.amount_cents,
        i.currency,
        i.status,
        c.name as customer_name,
        c.email as customer_email
    FROM invoices i
    JOIN customers c ON i.customer_id = c.id
    WHERE i.id = $1
`

func (r *InvoiceRepo) GetDetails(ctx context.Context, id InvoiceID) (*InvoiceDetails, error) {
    row := r.db.QueryRowContext(ctx, getInvoiceDetails, id)
    // Scan into struct...
}
```

**Test SQL against real Postgres:**

```go
func TestInvoiceRepo_GetByCustomer(t *testing.T) {
    db := testdb.New(t)  // Real Postgres container
    repo := NewInvoiceRepo(db)

    customer := testdata.CreateCustomer(t, db)
    otherCustomer := testdata.CreateCustomer(t, db)

    // Setup test data
    invoice1 := testdata.CreateInvoice(t, db, customer.ID, InvoiceStatusPaid)
    _ = testdata.CreateInvoice(t, db, customer.ID, InvoiceStatusDraft)
    _ = testdata.CreateInvoice(t, db, otherCustomer.ID, InvoiceStatusPaid)

    // Test filtering works
    invoices, err := repo.GetByCustomer(ctx, customer.ID, []InvoiceStatus{InvoiceStatusPaid}, 10)

    require.NoError(t, err)
    require.Len(t, invoices, 1)
    assert.Equal(t, invoice1.ID, invoices[0].ID)
}
```

**Migration considerations:**

```go
// Don't embed schema changes in application code
// Use proper migration tools (golang-migrate, goose, etc.)

// migrations/001_create_invoices_table.up.sql
CREATE TABLE invoices (
    id VARCHAR(32) PRIMARY KEY,
    customer_id VARCHAR(32) NOT NULL,
    amount_cents BIGINT NOT NULL,
    currency VARCHAR(3) NOT NULL,
    status VARCHAR(20) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_invoices_customer ON invoices(customer_id);
```

**When ORMs are acceptable:**

```go
// Migrations - schema changes, not runtime queries
// Simple CRUD that generates obvious queries
// Prototyping (with plan to rewrite)
```

Reference: [Why I Write SQL Using PSQL](https://gajus.com/blog/why-i-write-sql-using-psql)
