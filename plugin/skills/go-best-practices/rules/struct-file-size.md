---
title: Keep Files Under 400 Lines
impact: MEDIUM
impactDescription: Improves maintainability and code review
tags: structure, maintainability, organization
---

## Keep Files Under 400 Lines

**Impact: MEDIUM**

Large files create cognitive load, increase merge conflicts, and make code review difficult. Aim to keep Go source files under 400 lines, with 600 as a hard limit requiring justification.

**Guidelines:**

| Limit | Type | Action |
|-------|------|--------|
| 400 | Soft | Ideal target for most files |
| 600 | Hard | Requires justification, consider splitting |
| 800 | Tests | Higher tolerance for table-driven tests |

**Signs a file needs splitting:**

- Multiple unrelated types in one file
- Functions that could stand alone as a sub-module
- Difficult to find specific code
- Frequent merge conflicts
- Code review comments about file size

**Incorrect (monolithic file):**

```go
// service.go - 1500 lines
package service

type Service struct { ... }
func NewService() *Service { ... }
func (s *Service) CreateUser() { ... }      // User operations
func (s *Service) GetUser() { ... }
func (s *Service) UpdateUser() { ... }
func (s *Service) CreateOrder() { ... }     // Order operations
func (s *Service) GetOrder() { ... }
func (s *Service) ProcessPayment() { ... }  // Payment operations
// ... 50 more methods
```

**Correct (split by domain):**

```go
// service.go - ~100 lines, core types
package service

type Service struct {
    users    *UserService
    orders   *OrderService
    payments *PaymentService
}

func NewService(cfg Config) *Service { ... }
```

```go
// user.go - ~200 lines
package service

type UserService struct { ... }
func (u *UserService) Create() { ... }
func (u *UserService) Get() { ... }
func (u *UserService) Update() { ... }
```

```go
// order.go - ~250 lines
package service

type OrderService struct { ... }
func (o *OrderService) Create() { ... }
func (o *OrderService) Get() { ... }
```

**Naming convention for split files:**

```
{base}.go           - Core types and interfaces
{base}_{domain}.go  - Domain-specific logic
{base}_test.go      - Tests for base
{base}_{domain}_test.go - Tests for domain
```

Reference: [[ADR-20260123: Go File Size Guidelines]]
