---
title: Choose Receiver Type Carefully
impact: MEDIUM
impactDescription: Affects memory allocation and mutation semantics
tags: methods, receivers, performance
---

## Choose Receiver Type Carefully

**Impact: MEDIUM**

Choosing between value and pointer receivers affects whether mutations are visible to callers and memory allocation patterns. Use pointer receivers for mutations and large structs.

**When to use pointer receiver (`*T`):**

```go
// 1. Method needs to mutate the receiver
func (u *User) SetName(name string) {
    u.Name = name // Mutation visible to caller
}

// 2. Receiver contains sync.Mutex or similar
type SafeCounter struct {
    mu    sync.Mutex
    count int
}

func (c *SafeCounter) Increment() {
    c.mu.Lock()
    defer c.mu.Unlock()
    c.count++ // Must be pointer to avoid copying mutex
}

// 3. Receiver is a large struct
type LargeConfig struct {
    // Many fields...
}

func (c *LargeConfig) Validate() error {
    // Pointer avoids copying large struct
}

// 4. Consistency - if some methods need pointer, use pointer for all
func (u *User) Save() error { ... }
func (u *User) Validate() error { ... } // Pointer for consistency
```

**When to use value receiver (`T`):**

```go
// 1. Small, immutable types
type Point struct {
    X, Y float64
}

func (p Point) Distance(other Point) float64 {
    dx := p.X - other.X
    dy := p.Y - other.Y
    return math.Sqrt(dx*dx + dy*dy)
}

// 2. Basic types
type Celsius float64

func (c Celsius) ToFahrenheit() Fahrenheit {
    return Fahrenheit(c*9/5 + 32)
}

// 3. Slices (the slice header is small, underlying array isn't copied)
type StringList []string

func (s StringList) Contains(val string) bool {
    for _, v := range s {
        if v == val {
            return true
        }
    }
    return false
}
```

**Don't mix receiver types:**

```go
// BAD: Mixed receivers
func (u User) GetName() string { ... }
func (u *User) SetName(name string) { ... }

// GOOD: Consistent receivers (use pointer since SetName needs it)
func (u *User) GetName() string { ... }
func (u *User) SetName(name string) { ... }
```

**Never use pointer to interface or map/channel:**

```go
// BAD
func (m *map[string]int) Get(key string) int { ... }
func (r *io.Reader) Read() { ... }

// GOOD - these are already reference types
func (m MyMap) Get(key string) int { ... }
type MyReader struct { r io.Reader }
```

Reference: [Go Code Review Comments - Receiver Type](https://go.dev/wiki/CodeReviewComments#receiver-type)
