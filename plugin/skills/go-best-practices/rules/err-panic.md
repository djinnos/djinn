---
title: Avoid Panic - Return Errors Instead
impact: CRITICAL
impactDescription: Panics crash the program; errors allow graceful handling
tags: errors, panic, safety, control-flow
---

## Avoid Panic - Return Errors Instead

**Impact: CRITICAL**

**Default stance: don't panic.** Return errors instead. Panics crash the program (or at minimum, the goroutine), bypassing all your careful error handling.

**Simple heuristic:** If the code can reach production and the condition can occur at runtime (even due to a bug), return an error. Reserve panic only for initialization of compile-time constants.

**When to return an error (almost always):**

```go
// User provided bad input -> error
func ParseUserID(s string) (UserID, error) {
    if !strings.HasPrefix(s, "usr_") {
        return "", fmt.Errorf("invalid user ID format: %s", s)
    }
    return UserID(s), nil
}

// Database returned unexpected data -> error
func (r *Repo) GetUser(ctx context.Context, id UserID) (*User, error) {
    // Even if you "know" the user exists, the DB might disagree
    // Network issues, replication lag, deleted by another process...
    return user, err  // Let the caller decide what to do
}

// External service failed -> error
// File not found -> error
// JSON parsing failed -> error
// Timeout -> error

// Unhandled enum value -> error (not panic)
func (s Status) String() (string, error) {
    switch s {
    case StatusPending:
        return "pending", nil
    case StatusDone:
        return "done", nil
    default:
        return "", fmt.Errorf("unhandled status: %d", s)  // Bug, but still an error
    }
}
```

**The only time panic is acceptable:**

Package initialization with hardcoded constants where failure means a developer typo that must be fixed before deployment.

```go
// Package-level constants - if these fail, fix the typo and redeploy
var (
    emailRegex = regexp.MustCompile(`^[a-z]+@[a-z]+\.[a-z]+$`)
    defaultID  = uuid.MustParse("550e8400-e29b-41d4-a716-446655440000")
)
```

**Never panic on:**

- ❌ User input (even if "obviously" wrong)
- ❌ Database query results (even if you "know" the row exists)
- ❌ External API responses
- ❌ File operations
- ❌ JSON/XML parsing of external data
- ❌ Unhandled switch cases (return an error instead)
- ❌ Anything that could happen at runtime

**Incorrect (panic on runtime condition):**

```go
// BAD: Panic on user input
func (s *Service) CreateOrder(ctx context.Context, amount int64) (*Order, error) {
    if amount <= 0 {
        panic("amount must be positive")  // User error should return error
    }
    // ...
}

// BAD: Panic on "impossible" DB state
func (s *Service) GetActiveUser(ctx context.Context, id UserID) (*User, error) {
    user, err := s.repo.Get(ctx, id)
    if err != nil {
        return nil, err
    }
    if user.Status != StatusActive {
        panic("user should be active")  // DB state changed - return error
    }
    return user, nil
}

// BAD: Panic on unhandled enum
func (s Status) String() string {
    switch s {
    case StatusPending:
        return "pending"
    case StatusDone:
        return "done"
    default:
        panic(fmt.Sprintf("unhandled status: %d", s))  // Return error instead
    }
}
```

**Correct (return error for runtime conditions):**

```go
// GOOD: Return error for user input
func (s *Service) CreateOrder(ctx context.Context, amount int64) (*Order, error) {
    if amount <= 0 {
        return nil, fmt.Errorf("amount must be positive, got %d", amount)
    }
    // ...
}

// GOOD: Return error for unexpected DB state
func (s *Service) GetActiveUser(ctx context.Context, id UserID) (*User, error) {
    user, err := s.repo.Get(ctx, id)
    if err != nil {
        return nil, err
    }
    if user.Status != StatusActive {
        return nil, fmt.Errorf("user %s is %s, expected active", id, user.Status)
    }
    return user, nil
}

// GOOD: Return error for unhandled enum
func (s Status) String() (string, error) {
    switch s {
    case StatusPending:
        return "pending", nil
    case StatusDone:
        return "done", nil
    default:
        return "", fmt.Errorf("unhandled status: %d", s)
    }
}
```

**Why this matters:**

- Panics crash the entire request (or worse, the server)
- Panics are hard to recover from gracefully
- Errors allow callers to decide how to handle problems
- In a web server, one bad request shouldn't affect others

Reference: [Effective Go - Recover](https://go.dev/doc/effective_go#recover)
