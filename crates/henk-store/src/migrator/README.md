The schema of the run store, as one sequence of `sea-orm-migration` steps for
SQLite and `PostgreSQL` alike (#598; the choice of layer is in
`docs/decisions/0001-run-store-layer.md`).

# The steps

One file per step, `m001_baseline.rs` onwards, registered in order in
[`Migrator`]. `sea-orm-migration` records each applied step by name in the
`seaql_migrations` table, so a database runs only the steps it lacks.

- `m001_baseline`: every table with its indexes, the final schema of SQLite
  019 and `PostgreSQL` 015.
- `m002_run_instant`: the expression index of SQLite 013 (#216), SQLite only.
- `m003_run_children`: SQLite 020 / `PostgreSQL` 016.
- `m004_run_indexes`: three indexes (`events_run`, `findings_run`,
  `sessions_run`) that `PostgreSQL` had from its first migration and SQLite
  never got; now both have them.
- `m005_session_constraint_names`: `PostgreSQL` only; the `sessions` table was
  `lanes` once, and a rename keeps the old names of its constraints.
- `m006_inbound_events_instant`: SQLite only; the expression index the events
  page lists by (#216 for inbound events), as `m002` is for runs.

# Adding a migration

1. Create `m<next number>_<what_it_does>.rs` next to the others (`m007_...`
   after `m006`): a unit struct `Step` that implements `MigrationName` (the
   file's name, without `.rs`) and `MigrationTrait`. Copy `m004_run_indexes.rs`;
   it is the smallest.
2. Add `mod m007_<what_it_does>;` and `Box::new(m007_<what_it_does>::Step)` at
   the **end** of [`Migrator::migrations`]. Never edit, reorder or remove a
   step that has shipped: a database that ran it keeps its name, and a
   changed step runs only on new databases.
3. Say in the step what changed and why. Use the schema DSL where it can say
   it. Where it cannot, write raw SQL and mark it `RAW` with the reason.
4. A change that needs different DDL per database says so inside its step:
   ask `manager.get_database_backend()` (`DbBackend::Sqlite` or
   `DbBackend::Postgres`). There is one counter and one step per change,
   never a step per database. `time`, `number`, `key`, `text`, `run_fk` and
   `index` in `mod.rs` are what the baseline uses for the columns that
   differ.
5. Set `use_transaction` to `Some(true)` so a step that fails halfway leaves
   nothing behind.
6. If the change touches a table `tests/golden/seed.sql` fills, change the
   seed in the same pull request and bless the goldens again
   (`HENK_BLESS=1`).
7. Run the tests on SQLite (`cargo test -p henk-store`) and, by hand, on
   `PostgreSQL` (below).

Times stay RFC 3339 text on SQLite and `TIMESTAMPTZ` on `PostgreSQL`, numbers
`INTEGER` and `BIGINT`: a new column asks `time` or `number` for its type.

# Databases the previous migrator made

Before this sequence, SQLite kept its version in `PRAGMA user_version`
(20 steps) and `PostgreSQL` in the table `henk_schema_migrations` (16 steps),
each with its own SQL files. [`migrate`] adopts a database that is at the end
of that history: it records `m001` to `m003` as applied in `seaql_migrations`
and leaves the old bookkeeping untouched, then runs what is pending. A
database that is at any other version of the old history is refused with a
message, and nothing is changed: the old steps are not carried, so open it once
with a Henk that still has the previous migrator, then start this one. A
database with no history of either kind is new, and every step runs.

# Checking `PostgreSQL`

The `PostgreSQL` tests are `#[ignore]`d and need a server, and CI starts none
(#598). Run them by hand against a throwaway container before a release and
after any change to this module or to the store:

```text
docker run -d --rm --name henk-pg -e POSTGRES_PASSWORD=henk-ci -p 127.0.0.1:55436:5432 postgres:17-alpine
HENK_TEST_DATABASE_URL='postgres://postgres:henk-ci@127.0.0.1:55436/postgres?sslmode=disable' \
  cargo test -p henk-store --all-features --locked -- --ignored
```
