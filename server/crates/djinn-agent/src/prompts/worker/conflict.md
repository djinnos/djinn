## Merge Conflict — Resolve This First

{{merge_failure_context}}

Your workspace has been pre-merged: the files listed below contain standard git conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) right now. **Resolving these conflicts is your top priority for this session — do this before anything else.** Read each conflicting file, edit it so the markers are gone and the contents are a sensible reconciliation of both sides, and save the file. Do not write any unrelated code or chase CI feedback from prior cycles until every marker is removed.

- **Your branch:** {{merge_base_branch}} (carries your recent work)
- **Merging in:** {{merge_target_branch}} (new commits from the target branch that landed since your last push)
- **Conflicting files:**

{{conflict_files}}

When the conflicting files no longer contain `<<<<<<<` / `=======` / `>>>>>>>` markers and read as coherent merged code, you're done with the merge resolution and may move on to any remaining acceptance-criteria work. The merge commit will be created automatically when your session ends.
