---
name: debugging
description: Find root cause before fixing. Use when tests fail, code crashes, behavior is unexpected, errors appear, something broke, "it was working yesterday", flaky tests, build failures, integration issues, performance problems. STOP if tempted to "just try this fix" or "quick patch" - investigate first. For business/product root cause analysis, use root-cause skill instead.
---

# Systematic Debugging

## Overview

Random fixes waste time and create new bugs. Quick patches mask underlying issues.

**Core principle:** ALWAYS find root cause before attempting fixes. Symptom fixes are failure.

## The Iron Law

```
NO FIXES WITHOUT ROOT CAUSE INVESTIGATION FIRST
```

If you haven't completed Phase 1, you cannot propose fixes.

**No exceptions:**
- Not for "obvious" bugs
- Not for "quick fixes"
- Not under time pressure
- Not after "I think I see it"

**Violating the letter of this process is violating the spirit of debugging.**

## When to Use

Use for ANY technical issue:
- Test failures
- Bugs in production
- Unexpected behavior
- Performance problems
- Build failures
- Integration issues

**Use ESPECIALLY when:**
- Under time pressure (emergencies make guessing tempting)
- "Just one quick fix" seems obvious
- You've already tried multiple fixes
- Previous fix didn't work

## The Four Phases

Complete each phase before proceeding to the next.

### Phase 1: Root Cause Investigation

**BEFORE attempting ANY fix:**

1. **Read Error Messages Carefully**
   - Don't skip past errors or warnings
   - Read stack traces completely
   - Note line numbers, file paths, error codes

2. **Reproduce Consistently**
   - Can you trigger it reliably?
   - What are the exact steps?
   - If not reproducible, gather more data - don't guess

3. **Check Recent Changes**
   - Git diff, recent commits
   - New dependencies, config changes
   - Environmental differences

4. **Gather Evidence in Multi-Component Systems**

   For systems with multiple components (CI -> build -> deploy, API -> service -> database):

   ```
   For EACH component boundary:
     - Log what data enters component
     - Log what data exits component
     - Verify environment/config propagation
   
   Run once to gather evidence showing WHERE it breaks
   THEN analyze evidence to identify failing component
   ```

5. **Trace Data Flow**

   When error is deep in call stack, trace backward to find origin.
   See [cookbook/root-cause-tracing.md](./cookbook/root-cause-tracing.md)

### Phase 2: Pattern Analysis

**Find the pattern before fixing:**

1. **Find Working Examples**
   - Locate similar working code in same codebase
   - What works that's similar to what's broken?

2. **Identify Differences**
   - What's different between working and broken?
   - List every difference, however small
   - Don't assume "that can't matter"

3. **Understand Dependencies**
   - What other components does this need?
   - What settings, config, environment?

### Phase 3: Hypothesis and Testing

**Scientific method:**

1. **Form Single Hypothesis**
   - State clearly: "I think X is the root cause because Y"
   - Be specific, not vague

2. **Test Minimally**
   - Make the SMALLEST possible change to test hypothesis
   - One variable at a time
   - Don't fix multiple things at once

3. **Verify Before Continuing**
   - Did it work? Yes -> Phase 4
   - Didn't work? Form NEW hypothesis
   - DON'T add more fixes on top

### Phase 4: Implementation

**Fix the root cause, not the symptom:**

1. **Create Failing Test Case**
   - Simplest possible reproduction
   - MUST have before fixing

2. **Implement Single Fix**
   - Address the root cause identified
   - ONE change at a time
   - No "while I'm here" improvements

3. **Verify Fix**
   - Test passes now?
   - No other tests broken?

4. **If Fix Doesn't Work**
   - Count: How many fixes have you tried?
   - If < 3: Return to Phase 1, re-analyze
   - **If >= 3: STOP and question the architecture**

5. **If 3+ Fixes Failed: Question Architecture**

   Pattern indicating architectural problem:
   - Each fix reveals new problem in different place
   - Fixes require "massive refactoring"
   - Each fix creates new symptoms elsewhere

   **STOP and discuss with user before attempting more fixes.**

## Red Flags - STOP and Follow Process

If you catch yourself thinking:
- "Quick fix for now, investigate later"
- "Just try changing X and see if it works"
- "Add multiple changes, run tests"
- "It's probably X, let me fix that"
- "I don't fully understand but this might work"
- Proposing solutions before tracing data flow
- "One more fix attempt" (when already tried 2+)

**ALL of these mean: STOP. Return to Phase 1.**

## Common Rationalizations

| Excuse | Reality |
|--------|---------|
| "Issue is simple, don't need process" | Simple issues have root causes too. Process is fast for simple bugs. |
| "Emergency, no time for process" | Systematic debugging is FASTER than guess-and-check thrashing. |
| "Just try this first, then investigate" | First fix sets the pattern. Do it right from the start. |
| "I'll write test after confirming fix works" | Untested fixes don't stick. Test first proves it. |
| "Multiple fixes at once saves time" | Can't isolate what worked. Causes new bugs. |
| "I see the problem, let me fix it" | Seeing symptoms != understanding root cause. |
| "One more fix attempt" (after 2+ failures) | 3+ failures = architectural problem. Question pattern, don't fix again. |

## Quick Reference

| Phase | Key Activities | Success Criteria |
|-------|---------------|------------------|
| **1. Root Cause** | Read errors, reproduce, check changes, gather evidence | Understand WHAT and WHY |
| **2. Pattern** | Find working examples, compare | Identify differences |
| **3. Hypothesis** | Form theory, test minimally | Confirmed or new hypothesis |
| **4. Implementation** | Create test, fix, verify | Bug resolved, tests pass |

## Supporting Techniques

Available in this skill's cookbook:

- **[root-cause-tracing.md](./cookbook/root-cause-tracing.md)** - Trace bugs backward through call stack
- **[defense-in-depth.md](./cookbook/defense-in-depth.md)** - Add validation at multiple layers after fix
- **[condition-based-waiting.md](./cookbook/condition-based-waiting.md)** - Replace arbitrary timeouts with condition polling

Scripts:

- **[find-polluter.sh](./scripts/find-polluter.sh)** - Bisection script for finding test pollution
