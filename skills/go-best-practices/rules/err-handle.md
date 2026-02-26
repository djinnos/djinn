---
title: Always Handle Errors
impact: CRITICAL
impactDescription: Prevents silent failures and data corruption
tags: errors, reliability, debugging
---

## Always Handle Errors

**Impact: CRITICAL**

Never discard errors using `_`. If a function returns an error, check it to make sure the function succeeded. Handle the error, return it, or in truly exceptional situations, panic.

Discarding errors leads to silent failures that are extremely difficult to debug in production.

**Incorrect (discarding error):**

```go
// BAD: Error is silently ignored
val, _ := strconv.Atoi(input)
fmt.Println(val) // val is 0 if parsing failed, but we don't know

// BAD: File might not have been written
f.Write(data)

// BAD: Database might be in inconsistent state
db.Exec("UPDATE users SET status = 'active'")
```

**Correct (handling error):**

```go
// GOOD: Error is handled
val, err := strconv.Atoi(input)
if err != nil {
    return fmt.Errorf("invalid input %q: %w", input, err)
}
fmt.Println(val)

// GOOD: Check write succeeded
if _, err := f.Write(data); err != nil {
    return fmt.Errorf("write failed: %w", err)
}

// GOOD: Check database operation
if _, err := db.Exec("UPDATE users SET status = 'active'"); err != nil {
    return fmt.Errorf("failed to update users: %w", err)
}
```

**Exception:** It's acceptable to discard errors in deferred cleanup calls where the primary operation already succeeded:

```go
// Acceptable: logging cleanup failure in defer
defer func() {
    if err := f.Close(); err != nil {
        log.Printf("warning: failed to close file: %v", err)
    }
}()
```

Reference: [Go Code Review Comments - Handle Errors](https://go.dev/wiki/CodeReviewComments#handle-errors)
