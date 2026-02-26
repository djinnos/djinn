---
title: Wrap Errors with Context
impact: CRITICAL
impactDescription: Enables debugging without losing original cause
tags: errors, debugging, context
---

## Wrap Errors with Context

**Impact: CRITICAL**

When returning an error from a function, wrap it with context about what operation failed. Use `fmt.Errorf` with `%w` verb to preserve the original error chain.

Without context, error messages like "file not found" are useless - which file? From which operation?

**Incorrect (bare error return):**

```go
func LoadConfig(path string) (*Config, error) {
    data, err := os.ReadFile(path)
    if err != nil {
        return nil, err // BAD: No context about what we were trying to do
    }
    
    var cfg Config
    if err := json.Unmarshal(data, &cfg); err != nil {
        return nil, err // BAD: Was this a read error or parse error?
    }
    return &cfg, nil
}

// Error message: "open config.json: no such file or directory"
// Missing: what function, what operation, what path was intended
```

**Correct (wrapped with context):**

```go
func LoadConfig(path string) (*Config, error) {
    data, err := os.ReadFile(path)
    if err != nil {
        return nil, fmt.Errorf("load config %s: %w", path, err)
    }
    
    var cfg Config
    if err := json.Unmarshal(data, &cfg); err != nil {
        return nil, fmt.Errorf("parse config %s: %w", path, err)
    }
    return &cfg, nil
}

// Error message: "load config /etc/app/config.json: open /etc/app/config.json: no such file or directory"
// Clear: LoadConfig failed, trying to read path, file doesn't exist
```

**Best practices for error wrapping:**

```go
// Include operation name
fmt.Errorf("create user: %w", err)

// Include key identifiers
fmt.Errorf("create user %q: %w", username, err)

// Use lowercase, no ending punctuation
fmt.Errorf("connect to database: %w", err) // Good
fmt.Errorf("Failed to connect to database.": %w", err) // Bad
```

Reference: [Go 1.13 Error Wrapping](https://go.dev/blog/go1.13-errors)
