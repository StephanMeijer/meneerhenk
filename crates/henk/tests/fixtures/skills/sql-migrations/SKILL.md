---
name: sql-migrations
description: "Use when a change adds or edits a SQL migration."
---

# SQL migrations

- Every migration can be rolled back; see examples/rollback.sql.
- A new column on a large table is nullable or has a default.
- Never rename a column in the same release that stops reading it.
