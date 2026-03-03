---
tags:
    - adr
    - milestone-2
    - licensing
title: 'ADR-002: Feature Licensing Deferred — Metabase-Style Open Core'
type: adr
---
# ADR-002: Feature Licensing Deferred — Metabase-Style Open Core

## Status: Accepted

## Context

The original design conflated user authentication (Clerk) with server access control — the server wouldn't start without a valid Clerk token. This was partly motivated by future paywalling of premium server features.

Discussion clarified the actual business model: Metabase-style open core. The server is closed-source but runs fully functional without any license. Premium features (TBD) are gated behind a license key entered in Settings. The desktop may be open source.

Building licensing infrastructure now (before premium features exist) would be premature complexity.

## Decision

**Defer all licensing and feature-gating to post-v1. Design the license key system when premium features are defined.**

### What's Deferred
- License key generation, validation, and storage
- Feature flag system gated by license tier
- Server-side license enforcement
- Any phone-home or license verification API
- Stripe/billing integration

### What's Decided Now
- **Business model**: Metabase-style — free server works fully, license key unlocks premium features
- **License entry point**: Settings page in desktop (not a startup gate)
- **Server stays functional without license** — never blocks on missing license
- **Clerk is for identity, not licensing** — separate concerns entirely

### When to Revisit
When the first premium feature is defined and ready for implementation. At that point, design:
- License key format (signed JWT? simple key + API validation?)
- Validation mechanism (offline-capable? periodic online check?)
- Feature flag system in server
- License management UI in Settings

## Consequences

### Positive
- No premature infrastructure — no license server, no key generation, no enforcement code
- v1 ships faster without licensing complexity
- Business model decision is captured for when it's needed
- Clean separation: Clerk = identity, license key = entitlement

### Negative
- Server has no access control in v1 (acceptable — it's localhost)
- License system must be designed later (could be harder to retrofit, but unlikely given the simple model)

## Relations
- [[Roadmap]] — Phase 2 scope reduction
- [[ADR-001: Desktop-Only Clerk Authentication via System Browser PKCE]] — auth/licensing separation
- [[Milestone 2 Scope]] — licensing explicitly out of scope