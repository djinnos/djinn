---
title: Context as First Parameter
impact: HIGH
impactDescription: Enables cancellation and timeout propagation
tags: concurrency, context, api-design
---

## Context as First Parameter

**Impact: HIGH**

Functions that use a Context should accept it as their first parameter, named `ctx`. This is a universal Go convention that enables cancellation signals and deadlines to flow through your call chain.

**Incorrect (context in wrong position or stored in struct):**

```go
// BAD: Context not first parameter
func FetchUser(userID string, ctx context.Context) (*User, error) {
    // ...
}

// BAD: Context stored in struct
type Service struct {
    ctx    context.Context // Don't do this
    client *http.Client
}

func (s *Service) Fetch(url string) (*Response, error) {
    req, _ := http.NewRequestWithContext(s.ctx, "GET", url, nil)
    // Context is stale - was set at construction time
}

// BAD: Using context.Background() when context is available
func handler(w http.ResponseWriter, r *http.Request) {
    user, err := fetchUser(context.Background(), userID) // Should use r.Context()
}
```

**Correct (context as first parameter):**

```go
// GOOD: Context is first parameter
func FetchUser(ctx context.Context, userID string) (*User, error) {
    req, err := http.NewRequestWithContext(ctx, "GET", userURL+userID, nil)
    if err != nil {
        return nil, err
    }
    // Request will be cancelled if ctx is cancelled
}

// GOOD: Pass context to each method
type Service struct {
    client *http.Client
}

func (s *Service) Fetch(ctx context.Context, url string) (*Response, error) {
    req, err := http.NewRequestWithContext(ctx, "GET", url, nil)
    if err != nil {
        return nil, err
    }
    return s.client.Do(req)
}

// GOOD: Propagate context from request
func handler(w http.ResponseWriter, r *http.Request) {
    ctx := r.Context() // Use request's context
    
    user, err := fetchUser(ctx, userID)
    if err != nil {
        if ctx.Err() != nil {
            // Client disconnected, don't write response
            return
        }
        http.Error(w, err.Error(), 500)
        return
    }
}

// GOOD: Use context.Background() only at the top level
func main() {
    ctx := context.Background()
    ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
    defer cancel()
    
    if err := run(ctx); err != nil {
        log.Fatal(err)
    }
}
```

Reference: [Go Code Review Comments - Contexts](https://go.dev/wiki/CodeReviewComments#contexts)
