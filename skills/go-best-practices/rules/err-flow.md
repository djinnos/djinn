---
title: Indent Error Handling, Not Happy Path
impact: HIGH
impactDescription: Improves code readability and scanability
tags: errors, readability, style
---

## Indent Error Handling, Not Happy Path

**Impact: HIGH**

Keep the normal code path at minimal indentation. Indent the error handling, dealing with it first. This improves readability by allowing visual scanning of the happy path.

**Incorrect (happy path indented):**

```go
func processFile(path string) error {
    f, err := os.Open(path)
    if err != nil {
        return err
    } else {
        defer f.Close()
        data, err := io.ReadAll(f)
        if err != nil {
            return err
        } else {
            return processData(data)
        }
    }
}
```

**Correct (early returns, happy path unindented):**

```go
func processFile(path string) error {
    f, err := os.Open(path)
    if err != nil {
        return fmt.Errorf("open %s: %w", path, err)
    }
    defer f.Close()

    data, err := io.ReadAll(f)
    if err != nil {
        return fmt.Errorf("read %s: %w", path, err)
    }

    return processData(data)
}
```

**For if-init statements, move declaration out:**

```go
// Incorrect
if x, err := f(); err != nil {
    return err
} else {
    // use x
}

// Correct
x, err := f()
if err != nil {
    return err
}
// use x
```

Reference: [Go Code Review Comments - Indent Error Flow](https://go.dev/wiki/CodeReviewComments#indent-error-flow)
