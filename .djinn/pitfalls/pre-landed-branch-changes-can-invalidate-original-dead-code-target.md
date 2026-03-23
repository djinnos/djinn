---
title: Pre-landed branch changes can invalidate original dead_code target
type: pitfall
tags: []
---

The branch already contained the intended djinn-db dead_code cleanup: TASK_SUCCESS was no longer the active exported constant in note/scoring.rs and obsolete helpers in task/verification.rs were already removed. Before making fixes, re-check the current branch state against the original issue to avoid redoing completed work and to focus only on remaining verification regressions.

---
*Extracted from session 019d17d2-4b8c-72d3-a51e-7a235eb68908. Confidence: 0.5 (session-extracted).*