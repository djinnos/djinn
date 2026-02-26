---
title: Document Goroutine Lifetimes
impact: HIGH
impactDescription: Prevents goroutine leaks and data races
tags: concurrency, goroutines, leaks
---

## Document Goroutine Lifetimes

**Impact: HIGH**

When you spawn goroutines, make it clear when - or whether - they exit. Goroutines can leak by blocking on channel sends or receives. The garbage collector will NOT terminate a blocked goroutine.

**Incorrect (unclear lifetime, potential leak):**

```go
// BAD: Goroutine may leak if ctx is never cancelled
func StartWorker(ctx context.Context, jobs <-chan Job) {
    go func() {
        for {
            select {
            case job := <-jobs:
                process(job)
            case <-ctx.Done():
                return
            }
        }
    }()
    // When does this goroutine exit? What if jobs channel is never closed?
}

// BAD: Goroutine leaks if no one reads from results
func ProcessAll(items []Item) <-chan Result {
    results := make(chan Result)
    go func() {
        for _, item := range items {
            results <- process(item) // Blocks forever if no reader
        }
        close(results)
    }()
    return results
}
```

**Correct (clear lifetime, documented):**

```go
// GOOD: Clear lifetime with WaitGroup
func StartWorker(ctx context.Context, jobs <-chan Job) func() {
    var wg sync.WaitGroup
    wg.Add(1)
    
    go func() {
        defer wg.Done()
        for {
            select {
            case job, ok := <-jobs:
                if !ok {
                    return // jobs closed, exit
                }
                process(job)
            case <-ctx.Done():
                return // context cancelled, exit
            }
        }
    }()
    
    // Return cleanup function
    return func() {
        wg.Wait()
    }
}

// GOOD: Buffered channel prevents blocking
func ProcessAll(items []Item) <-chan Result {
    results := make(chan Result, len(items)) // Buffered
    go func() {
        defer close(results)
        for _, item := range items {
            results <- process(item)
        }
    }()
    return results
}

// GOOD: errgroup manages goroutine lifetime
func FetchAll(ctx context.Context, urls []string) ([]Response, error) {
    g, ctx := errgroup.WithContext(ctx)
    responses := make([]Response, len(urls))
    
    for i, url := range urls {
        i, url := i, url // Capture for goroutine
        g.Go(func() error {
            resp, err := fetch(ctx, url)
            if err != nil {
                return err
            }
            responses[i] = resp
            return nil
        })
    }
    
    if err := g.Wait(); err != nil {
        return nil, err
    }
    return responses, nil
}
```

Reference: [Go Code Review Comments - Goroutine Lifetimes](https://go.dev/wiki/CodeReviewComments#goroutine-lifetimes)
