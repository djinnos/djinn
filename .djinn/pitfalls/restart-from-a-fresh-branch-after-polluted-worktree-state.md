---
title: Restart from a fresh branch after polluted worktree state
type: pitfall
tags: []
---

If a prior task branch/worktree accumulated interrupted-session commits or malformed worktree errors like 'No such file or directory', do not trust that branch state. Recreate the work on a clean branch from current main and keep scope limited to the specified files plus only verifier-required adjustments.

---
*Extracted from session 019d17e8-8317-7633-8775-63f96e3a5dbb. Confidence: 0.5 (session-extracted).*