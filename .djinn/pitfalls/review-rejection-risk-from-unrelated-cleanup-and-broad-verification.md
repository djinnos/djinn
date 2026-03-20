---
title: Review rejection risk from unrelated cleanup and broad verification
type: pitfall
tags: []
---

Previous attempts accumulated clippy-driven edits and snapshot churn outside the requested params split. Keep the diff narrowly limited to moving parameter structs into extension/params.rs, and run only the specified verification commands from server/ to avoid unrelated changes.

---
*Extracted from session 019d0c67-ffcd-7182-a966-244f36fd7ecc. Confidence: 0.5 (session-extracted).*