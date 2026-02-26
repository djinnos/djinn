---
title: Test Failures Should Be Useful
impact: MEDIUM
impactDescription: Faster debugging when tests fail
tags: testing, debugging, error-messages
---

## Test Failures Should Be Useful

**Impact: MEDIUM**

Tests should fail with helpful messages saying what was wrong, what inputs were used, what was actually received, and what was expected. Someone debugging your test may not be familiar with the code.

**Incorrect (unhelpful failure messages):**

```go
func TestUser(t *testing.T) {
    u := GetUser("123")
    if u == nil {
        t.Fail() // What failed? Why?
    }
    
    if u.Name != "Alice" {
        t.Error("wrong") // Wrong what? What did we get?
    }
    
    // Using assert without context
    assert.Equal(t, "Alice", u.Name) // Slightly better but still missing input context
}
```

**Correct (helpful failure messages):**

```go
func TestUser(t *testing.T) {
    userID := "123"
    u, err := GetUser(userID)
    
    if err != nil {
        t.Fatalf("GetUser(%q) failed: %v", userID, err)
    }
    
    if u == nil {
        t.Fatalf("GetUser(%q) returned nil user", userID)
    }
    
    // Format: Function(input) = got; want expected
    if u.Name != "Alice" {
        t.Errorf("GetUser(%q).Name = %q; want %q", userID, u.Name, "Alice")
    }
    
    if u.Age != 30 {
        t.Errorf("GetUser(%q).Age = %d; want %d", userID, u.Age, 30)
    }
}

// Output on failure:
// user_test.go:15: GetUser("123").Name = "Bob"; want "Alice"
```

**Pattern: "got X; want Y" with context:**

```go
// Standard pattern
t.Errorf("FunctionName(args) = %v; want %v", got, want)

// With more context
t.Errorf("FunctionName(%q, %d) returned %v; want %v", 
    stringArg, intArg, got, want)

// For errors
t.Errorf("FunctionName(%v) error = %v; want error containing %q",
    input, err, wantErrSubstring)
```

**Use t.Helper() for test utilities:**

```go
func assertEqual(t *testing.T, got, want interface{}) {
    t.Helper() // Marks this as helper - failures report caller's line
    if !reflect.DeepEqual(got, want) {
        t.Errorf("got %v; want %v", got, want)
    }
}
```

Reference: [Go Code Review Comments - Useful Test Failures](https://go.dev/wiki/CodeReviewComments#useful-test-failures)
