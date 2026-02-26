---
title: Define Interfaces at Consumer, Not Producer
impact: HIGH
impactDescription: Enables loose coupling and testability
tags: interfaces, design, testing
---

## Define Interfaces at Consumer, Not Producer

**Impact: HIGH**

Go interfaces belong in the package that **uses** values of the interface type, not the package that **implements** them. The implementing package should return concrete types.

This pattern enables loose coupling: consumers define exactly what they need, implementations can add methods freely.

**Incorrect (interface defined by producer):**

```go
// producer/storage.go
package producer

// BAD: Producer defines interface and returns it
type Storage interface {
    Get(key string) ([]byte, error)
    Set(key string, value []byte) error
    Delete(key string) error
}

type RedisStorage struct { /* ... */ }

func NewStorage() Storage {
    return &RedisStorage{}
}
```

```go
// consumer/service.go
package consumer

import "producer"

// Consumer is forced to use producer's interface
func NewService(s producer.Storage) *Service {
    return &Service{storage: s}
}
```

**Correct (interface defined by consumer):**

```go
// producer/storage.go
package producer

// GOOD: Producer returns concrete type
type RedisStorage struct { /* ... */ }

func (r *RedisStorage) Get(key string) ([]byte, error) { /* ... */ }
func (r *RedisStorage) Set(key string, value []byte) error { /* ... */ }
func (r *RedisStorage) Delete(key string) error { /* ... */ }
func (r *RedisStorage) Ping() error { /* ... */ } // Can add methods freely

func NewStorage() *RedisStorage {
    return &RedisStorage{}
}
```

```go
// consumer/service.go
package consumer

// GOOD: Consumer defines only what it needs
type KeyValueStore interface {
    Get(key string) ([]byte, error)
    Set(key string, value []byte) error
}

type Service struct {
    store KeyValueStore
}

// Accepts interface - can be tested with mock
func NewService(store KeyValueStore) *Service {
    return &Service{store: store}
}
```

```go
// consumer/service_test.go
package consumer

type mockStore struct {
    data map[string][]byte
}

func (m *mockStore) Get(key string) ([]byte, error) {
    return m.data[key], nil
}

func (m *mockStore) Set(key string, value []byte) error {
    m.data[key] = value
    return nil
}

func TestService(t *testing.T) {
    svc := NewService(&mockStore{data: make(map[string][]byte)})
    // Test without real Redis
}
```

Reference: [Go Code Review Comments - Interfaces](https://go.dev/wiki/CodeReviewComments#interfaces)
