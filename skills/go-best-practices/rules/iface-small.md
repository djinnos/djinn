---
title: Prefer Small Interfaces
impact: HIGH
impactDescription: Increases flexibility and reusability
tags: interfaces, design, composition
---

## Prefer Small Interfaces

**Impact: HIGH**

The bigger the interface, the weaker the abstraction. Prefer small interfaces with 1-3 methods. Compose larger behaviors from smaller interfaces when needed.

Small interfaces are easier to implement, easier to mock in tests, and more reusable across packages.

**Incorrect (large interface):**

```go
// BAD: Too many methods, hard to implement, hard to mock
type Repository interface {
    Get(id string) (*Entity, error)
    List(filter Filter) ([]*Entity, error)
    Create(e *Entity) error
    Update(e *Entity) error
    Delete(id string) error
    Count(filter Filter) (int, error)
    Exists(id string) (bool, error)
    Transaction(fn func(tx Repository) error) error
}

// Consumer only needs Get, but must mock 8 methods
func LoadEntity(repo Repository, id string) (*Entity, error) {
    return repo.Get(id)
}
```

**Correct (small, composable interfaces):**

```go
// GOOD: Small, focused interfaces
type Getter interface {
    Get(id string) (*Entity, error)
}

type Lister interface {
    List(filter Filter) ([]*Entity, error)
}

type Creator interface {
    Create(e *Entity) error
}

type Updater interface {
    Update(e *Entity) error
}

// Compose when you need multiple capabilities
type ReadWriter interface {
    Getter
    Creator
    Updater
}

// Consumer declares exactly what it needs
func LoadEntity(g Getter, id string) (*Entity, error) {
    return g.Get(id) // Easy to mock - just implement Get
}

func SyncEntities(r Getter, w Creator, ids []string) error {
    for _, id := range ids {
        e, err := r.Get(id)
        if err != nil {
            return err
        }
        if err := w.Create(e); err != nil {
            return err
        }
    }
    return nil
}
```

**Standard library examples:**

```go
// io.Reader - 1 method
type Reader interface {
    Read(p []byte) (n int, err error)
}

// io.Writer - 1 method
type Writer interface {
    Write(p []byte) (n int, err error)
}

// Composed when needed
type ReadWriter interface {
    Reader
    Writer
}
```

Reference: [Go Proverbs - The bigger the interface, the weaker the abstraction](https://go-proverbs.github.io/)
