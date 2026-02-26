---
title: Use Bounded Loops
impact: MEDIUM
impactDescription: Prevents infinite loops and makes termination explicit
tags: control-flow, loops, safety, power-of-ten
---

## Use Bounded Loops

**Impact: MEDIUM**

From NASA's Power of Ten: all loops must have a bound. In practice, this means being deliberate about iteration and ensuring loops always terminate.

**Incorrect (unbounded retry):**

```go
// BAD: Could retry forever
func PollForStatus(ctx context.Context, id string) (Status, error) {
    for {
        status, err := checkStatus(id)
        if err != nil {
            return "", err
        }
        if status == StatusReady {
            return status, nil
        }
        time.Sleep(time.Second)
    }
}
```

**Correct (bounded retry):**

```go
// GOOD: Bounded retry with explicit limit
func PollForStatus(ctx context.Context, id string) (Status, error) {
    const maxRetries = 10
    for attempt := 0; attempt < maxRetries; attempt++ {
        status, err := checkStatus(id)
        if err != nil {
            return "", fmt.Errorf("check status: %w", err)
        }
        if status == StatusReady {
            return status, nil
        }

        // Exponential backoff
        backoff := time.Second * time.Duration(attempt+1)
        select {
        case <-ctx.Done():
            return "", ctx.Err()
        case <-time.After(backoff):
        }
    }
    return "", fmt.Errorf("status not ready after %d attempts", maxRetries)
}
```

**Context cancellation for loop termination:**

```go
// GOOD: Context provides the bound
func ProcessItems(ctx context.Context, items []Item) error {
    for _, item := range items {
        if err := ctx.Err(); err != nil {
            return err  // Context cancelled, stop processing
        }

        if err := process(item); err != nil {
            return fmt.Errorf("process item %s: %w", item.ID, err)
        }
    }
    return nil
}
```

**Collection iteration is bounded:**

```go
// GOOD: Range over slice is bounded by slice length
for _, item := range items {
    process(item)
}

// GOOD: Range over map is bounded (though order undefined)
for k, v := range m {
    process(k, v)
}
```

**Watch for accumulation:**

```go
// BAD: Could accumulate forever
for {
    item := queue.Dequeue()
    if item == nil {
        break
    }
    buffer = append(buffer, item)  // Unbounded growth
}

// GOOD: Bounded buffer
const maxBufferSize = 1000
for len(buffer) < maxBufferSize {
    item := queue.Dequeue()
    if item == nil {
        break
    }
    buffer = append(buffer, item)
}
if len(buffer) == maxBufferSize {
    return fmt.Errorf("buffer full, could not process all items")
}
```

**Error budget for retry:**

```go
// GOOD: Track error budget separately from attempts
func ProcessWithRetry(ctx context.Context, fn func() error) error {
    const (
        maxAttempts  = 5
        maxDuration  = 30 * time.Second
    )
    deadline := time.Now().Add(maxDuration)

    for attempt := 0; attempt < maxAttempts; attempt++ {
        if time.Now().After(deadline) {
            return fmt.Errorf("timeout after %v", maxDuration)
        }

        err := fn()
        if err == nil {
            return nil
        }

        if !isRetryable(err) {
            return err  // Don't retry permanent errors
        }

        time.Sleep(exponentialBackoff(attempt))
    }
    return fmt.Errorf("failed after %d attempts", maxAttempts)
}
```

Reference: [NASA Power of Ten Rules](https://en.wikipedia.org/wiki/The_Power_of_10:_Rules_for_Developing_Safety-Critical_Code)
