---
title: Keep dead_code regression fixes scoped to branch-local verification failures
type: pitfall
tags: []
---

When validating an older cleanup branch against current main, remaining failures may come from drift rather than the originally reported dead_code items. The safe approach is to limit changes to fixes required for green verification on the branch and avoid unrelated refactors during reconciliation.

---
*Extracted from session 019d17d2-4b8c-72d3-a51e-7a235eb68908. Confidence: 0.5 (session-extracted).*