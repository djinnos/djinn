---
title: Package Names Should Be Short and Clear
impact: HIGH
impactDescription: Improves code readability at call sites
tags: naming, packages, style
---

## Package Names Should Be Short and Clear

**Impact: HIGH**

Package names should be short, concise, lowercase, and single-word. Avoid underscores, mixedCaps, and generic names like `util`, `common`, `misc`.

Remember: the package name becomes a prefix at call sites (`http.Get`, `json.Marshal`).

**Incorrect (bad package names):**

```go
// BAD: Generic, meaningless names
package util
package common
package misc
package helpers
package base

// BAD: Redundant with content
package httputil  // if it only contains HTTP stuff
package stringutils

// BAD: Underscores and mixedCaps
package my_package
package myPackage

// BAD: Plural (usually)
package models  // unless it's about data modeling itself
package handlers
```

**Correct (good package names):**

```go
// GOOD: Short, clear, purposeful
package http
package json
package time
package user     // domain entity
package auth     // authentication logic
package storage  // storage abstraction

// Call sites read naturally
http.Get(url)
json.Marshal(data)
user.Create(ctx, u)
auth.Validate(token)
```

**Avoid stuttering - don't repeat package name in identifiers:**

```go
// BAD: Stuttering
package user
type UserService struct{}  // user.UserService reads poorly

// GOOD: No repetition
package user
type Service struct{}      // user.Service reads well

// BAD
package http
func HTTPGet() // http.HTTPGet

// GOOD
package http
func Get()     // http.Get
```

Reference: [Go Blog - Package Names](https://go.dev/blog/package-names)
