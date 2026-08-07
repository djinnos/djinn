Each entry below is a **pointer to a note**, not the note. It was learned from
previous work in the code areas this task touches.

**Coverage map — what you have vs. what is one call away.**
In context: the note's type label, its permalink handle, the condition under
which it applies, and — on some entries — a bounded `action:` excerpt of its
prevention guidance. One `memory_read` call away: the full note body —
reproduction steps, exact commands, diagnostics, verification steps, and links
to related notes. None of that is printed here.

**Pull triggers — call `memory_read(identifier=<permalink>)` when:**
- an entry's excerpt ends in `… truncated; memory_read(<permalink>)`;
- an entry's condition matches what you are about to do and you do not already
  know the procedure it refers to;
- you are about to run a regeneration, migration, deploy, or release step that
  an entry claims to cover;
- a CI failure, panic, or hang resembles an entry's condition.

**Negative list — do NOT pull when:**
- the excerpt already fully answers the question you have;
- the entry's condition does not match this task;
- you already read that permalink earlier in this session;
- nothing has triggered — never sweep every entry pre-emptively "just in case".

**Worked example.** Given the entry:

    - **[Pitfall] pitfalls/warm-base-rustflags-mismatch**: applies when a build command sets RUSTFLAGS inline
      action: … truncated; memory_read(pitfalls/warm-base-rustflags-mismatch)

and you are about to edit a command that sets `RUSTFLAGS`, the trigger matched,
so call `memory_read(identifier="pitfalls/warm-base-rustflags-mismatch")` and
apply the procedure it gives *before* you make the edit.
**Empty-result branch:** if that call returns nothing or errors, continue the
task on your own judgement, say in your summary that the note could not be read,
and do **not** invent its contents. Retry the identifier at most once, then move
on — never loop.

**Anti-refusal.** When a `memory_read` call would resolve the gap, you must not
reply with any of:
"I don't have access to that note",
"I cannot read files",
"this appears to be truncated",
"the note is not available to me",
or any equivalent. Those statements are only honest *after* a call came back
empty. Make the call first, then report what it returned.

**Handles come from this index.** A permalink must be copied verbatim from an
entry below. Never guess one, never slugify a title into one, never construct
one that was not printed here. If the knowledge you need is not covered by any
entry, use `memory_search(query=...)` instead.

**Budget is asymmetric.** Grounded pulls — `memory_read` on a permalink printed
below — are unlimited and encouraged; they cost far less than the failure the
note prevents. Speculative `memory_search` calls are metered: at most 3 for this
task, and only once the grounded pulls that apply are exhausted.
Always prefer a grounded pull to a speculative search.
