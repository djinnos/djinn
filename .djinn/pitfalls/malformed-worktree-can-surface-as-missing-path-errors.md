---
title: Malformed worktree can surface as missing-path errors
type: pitfall
tags: []
---

A malformed worktree produced a `No such file or directory` failure. If this appears during task continuation, treat the worktree as unreliable and recreate or restart from a clean checkout instead of trying to repair in place.

---
*Extracted from session 019d17d7-9c59-7b43-855a-a414667d9edd. Confidence: 0.5 (session-extracted).*