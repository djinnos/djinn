---
title: Use Table-Driven Tests
impact: MEDIUM
impactDescription: More maintainable, comprehensive test coverage
tags: testing, patterns, maintainability
---

## Use Table-Driven Tests

**Impact: MEDIUM**

Table-driven tests use a slice of test cases to test multiple scenarios with minimal code duplication. Each test case is a struct with inputs and expected outputs.

**Incorrect (repetitive tests):**

```go
func TestAdd(t *testing.T) {
    result := Add(1, 2)
    if result != 3 {
        t.Errorf("Add(1, 2) = %d; want 3", result)
    }
    
    result = Add(0, 0)
    if result != 0 {
        t.Errorf("Add(0, 0) = %d; want 0", result)
    }
    
    result = Add(-1, 1)
    if result != 0 {
        t.Errorf("Add(-1, 1) = %d; want 0", result)
    }
    // Lots of duplication, hard to add new cases
}
```

**Correct (table-driven):**

```go
func TestAdd(t *testing.T) {
    tests := []struct {
        name string
        a, b int
        want int
    }{
        {"positive numbers", 1, 2, 3},
        {"zeros", 0, 0, 0},
        {"negative and positive", -1, 1, 0},
        {"both negative", -2, -3, -5},
        {"large numbers", 1000000, 2000000, 3000000},
    }

    for _, tt := range tests {
        t.Run(tt.name, func(t *testing.T) {
            got := Add(tt.a, tt.b)
            if got != tt.want {
                t.Errorf("Add(%d, %d) = %d; want %d", tt.a, tt.b, got, tt.want)
            }
        })
    }
}
```

**For complex cases, use named fields:**

```go
func TestParseConfig(t *testing.T) {
    tests := []struct {
        name    string
        input   string
        want    *Config
        wantErr bool
    }{
        {
            name:  "valid config",
            input: `{"port": 8080}`,
            want:  &Config{Port: 8080},
        },
        {
            name:    "invalid json",
            input:   `{invalid}`,
            wantErr: true,
        },
        {
            name:    "missing required field",
            input:   `{}`,
            wantErr: true,
        },
    }

    for _, tt := range tests {
        t.Run(tt.name, func(t *testing.T) {
            got, err := ParseConfig(tt.input)
            
            if tt.wantErr {
                if err == nil {
                    t.Fatal("expected error, got nil")
                }
                return
            }
            
            if err != nil {
                t.Fatalf("unexpected error: %v", err)
            }
            
            if !reflect.DeepEqual(got, tt.want) {
                t.Errorf("got %+v; want %+v", got, tt.want)
            }
        })
    }
}
```

Reference: [Go Wiki - Table Driven Tests](https://go.dev/wiki/TableDrivenTests)
