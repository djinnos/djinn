---
title: Task scope can be invalidated by current main already matching requested layout
type: pitfall
tags: []
---

Before making changes for the extension params split, check current main from a fresh branch to see whether the requested module layout is already present. If no task-specific diff is needed, explicitly report that and avoid code edits to prevent unnecessary churn.

---
*Extracted from session 019d0c67-ffcd-7182-a966-244f36fd7ecc. Confidence: 0.5 (session-extracted).*