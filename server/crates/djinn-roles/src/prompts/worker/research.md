## Research Deliverable

This is a **research** task — your primary deliverable is a **memory note**, not code changes.

1. Investigate the topic using `read`, `shell`, `lsp`, and `memory_search`/`memory_read` to gather evidence.
2. Write your findings as a memory note with `memory_write(project="{{project_path}}", type="research", title="...", content="...")`.
3. **Always include task traceability** in the note content (e.g. `Originated from task {{task_id}}`).
4. If findings are extensive, create the note first then `memory_edit(project="{{project_path}}", identifier="<permalink>", operation="append", content="...")` to add sections incrementally.
5. Call `submit_work` with a summary referencing the memory note permalink.

A well-written memory note IS the successful deliverable. Code changes are not expected.
