# Task: resolve a rebase conflict

Pull request {{ref}} in {{repo}} is being rebased onto `{{base}}`. Replaying its commit "{{commit}}" stopped on conflicts in these files:

{{files}}

Each of them holds git's conflict markers: a block from `<<<<<<<` to `=======` is what `{{base}}` has now, and the block from `=======` to `>>>>>>>` is what the pull request's commit changes. When you are done, Henk's code checks the files, goes on with the rebase and pushes it; you do not run git yourself.

Work in this order:

1. Read each conflicted file whole (`read_file`), and the code around it where you need to (`search`, `list_files`).
2. Resolve each conflict with `{{edit_tool}}` or `write_file`: keep what `{{base}}` changed and what the commit changed, combined the way the code needs it. Remove every marker line. When the two cannot both hold, keep the commit's intent and adapt it to `{{base}}`.
3. Change nothing outside these files, and nothing in them beyond the conflicts.

If you cannot resolve a conflict with confidence, leave its file as it is and say why in one line: Henk then stops the rebase and pushes nothing. Plain text in English, no emoji, no em-dash.
