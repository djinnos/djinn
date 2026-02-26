---
title: Validate at the Right Layer
impact: CRITICAL
impactDescription: Prevents invalid data from propagating through the system
tags: domain, validation, layers, architecture
---

## Validate at the Right Layer

**Impact: CRITICAL**

Validation isn't one thing — it happens at different layers, each with a different purpose. Mixing validation layers leads to incomplete checks or duplicated logic.

| Layer | What it checks | Can reject with |
|-------|----------------|-----------------|
| **Boundary** (handler) | Format, presence, type conversion | 400 Bad Request |
| **Domain** (constructor) | Structural invariants within the type | 400 Bad Request |
| **Service** | Business rules requiring DB/context | 400/409/422 depending on rule |

**Layer 1: Boundary validation (handlers)**

Check that the request is well-formed before doing any work. Parse and convert to domain types.

```go
func (h *Handler) CreateInvoice(ctx context.Context, req *pb.CreateInvoiceRequest) (*pb.Invoice, error) {
    // Format validation - can we even parse this?
    customerID, err := domain.ParseCustomerID(req.CustomerId)
    if err != nil {
        return nil, status.Errorf(codes.InvalidArgument, "invalid customer_id: %v", err)
    }

    items, err := parseLineItems(req.Items)
    if err != nil {
        return nil, status.Errorf(codes.InvalidArgument, "invalid items: %v", err)
    }

    dueDate, err := time.Parse(time.DateOnly, req.DueDate)
    if err != nil {
        return nil, status.Errorf(codes.InvalidArgument, "invalid due_date: %v", err)
    }

    // Pass parsed domain types to service
    invoice, err := h.service.CreateInvoice(ctx, customerID, items, dueDate)
    if err != nil {
        return nil, toGRPCError(err)
    }
    return toProto(invoice), nil
}
```

**Layer 2: Domain validation (constructors)**

Check invariants that are always true for the type, without needing external context.

```go
func NewInvoice(customerID CustomerID, items []LineItem, dueDate time.Time, now time.Time) (*Invoice, error) {
    // Structural invariants - these don't need a database
    if len(items) == 0 {
        return nil, errors.New("invoice must have at least one line item")
    }
    if dueDate.Before(now) {
        return nil, errors.New("due date must be in the future")
    }

    total := calculateTotal(items)
    if total.Cents <= 0 {
        return nil, errors.New("invoice total must be positive")
    }

    return &Invoice{
        ID:         NewInvoiceID(),
        CustomerID: customerID,
        LineItems:  items,
        Total:      total,
        DueDate:    dueDate,
        Status:     InvoiceStatusDraft,
    }, nil
}
```

**Layer 3: Service validation (business rules)**

Business rules that require database lookups or external context.

```go
func (s *InvoiceService) CreateInvoice(
    ctx context.Context,
    customerID CustomerID,
    items []LineItem,
    dueDate time.Time,
) (*Invoice, error) {
    // Business rule: customer must exist and be active
    customer, err := s.customerRepo.Get(ctx, customerID)
    if errors.Is(err, ErrNotFound) {
        return nil, fmt.Errorf("customer %s: %w", customerID, ErrNotFound)
    }
    if err != nil {
        return nil, fmt.Errorf("get customer: %w", err)
    }
    if customer.Status != CustomerStatusActive {
        return nil, fmt.Errorf("customer %s is not active", customerID)
    }

    // Business rule: total must not exceed customer's credit limit
    total := calculateTotal(items)
    if total.Cents > customer.CreditLimitCents {
        return nil, fmt.Errorf("amount %d exceeds credit limit %d", total.Cents, customer.CreditLimitCents)
    }

    // Domain validation happens inside NewInvoice
    invoice, err := domain.NewInvoice(customerID, items, dueDate, time.Now())
    if err != nil {
        return nil, err
    }

    if err := s.invoiceRepo.Create(ctx, invoice); err != nil {
        return nil, fmt.Errorf("save invoice: %w", err)
    }
    return invoice, nil
}
```

**What "trust your types" means:**

After parsing `CustomerID` at the boundary, you can trust it's *syntactically valid* (correct format). You cannot trust the customer *exists* until you've checked the database. Types encode format, not existence.

Reference: [Parse, Don't Validate](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/)
