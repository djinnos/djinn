---
title: Use MixedCaps for Names
impact: HIGH
impactDescription: Consistency with Go ecosystem
tags: naming, style, conventions
---

## Use MixedCaps for Names

**Impact: HIGH**

Go uses MixedCaps (or mixedCaps for unexported) rather than underscores for multi-word names. This applies to constants, variables, functions, types, and methods.

**Incorrect (underscores):**

```go
// BAD: Underscores in identifiers
const MAX_RETRY_COUNT = 3
var user_name string
func get_user_by_id(user_id string) *User
type http_client struct{}
```

**Correct (MixedCaps):**

```go
// GOOD: MixedCaps for exported
const MaxRetryCount = 3
var UserName string
func GetUserByID(userID string) *User
type HTTPClient struct{}

// GOOD: mixedCaps for unexported
const maxRetryCount = 3
var userName string
func getUserByID(userID string) *user
type httpClient struct{}
```

**Special case - Initialisms:**

Keep initialisms in consistent case (all upper or all lower):

```go
// GOOD: Initialisms stay uppercase
var userID string       // not UserId
func ServeHTTP()        // not ServeHttp
type XMLHTTPRequest     // not XmlHttpRequest
var urlParser           // not URLParser (if unexported, all lower)

// GOOD: Multiple initialisms
type XMLHTTP struct{}   // or xmlHTTP for unexported
var httpURL string
```

Reference: [Effective Go - MixedCaps](https://go.dev/doc/effective_go#mixed-caps)
