# 0001: The run store on one implementation over sea-orm

Status: proposed; it is decided when this pull request is merged. The paragraph on `henk-store` in `ARCHITECTURE.md` points here and says "proposed"; change it in the same pull request that changes this status.
Date: 2026-10-16. Issues: #598 (the move), #601 (this spike).

## The question

`crates/henk-store` implements the `RunStore` trait twice, `sqlite.rs` over `rusqlite` and `postgres.rs` over `tokio-postgres`, with a migration history for each. #598 decided that it becomes one implementation over an ORM or query builder, with one set of migrations, so that CI tests the store on SQLite alone and needs no PostgreSQL server. This record is the answer to *which layer*, and how the migrations and the existing databases move. It does not reopen whether.

## The decision

Use **sea-orm 2.0** (`sea-query` for the queries, `sea-orm-migration` for the schema) as the one layer over SQLite and PostgreSQL.

- One `Store` type implements `RunStore` once. Queries are built with `sea-query`; where the builder cannot say something, a raw fragment stays, behind one named place (three in the spike's queries, listed below).
- The schema is one numbered sequence of migrations written in sea-orm's schema DSL, a step asking which database it runs on where the DDL differs.
- Databases made by the current code (SQLite `user_version` 20, PostgreSQL `henk_schema_migrations` at 16) are **adopted**: recognised as already at the first steps of the new sequence, never re-run, their old bookkeeping left as it is.
- TLS stays on rustls with ring and the platform's native roots (`deny.toml` bans `webpki-roots`, `aws-lc-*`, `openssl-sys`); this needs `sqlx` named as a dependency with `tls-rustls-ring-native-roots`, since sea-orm's own `runtime-tokio-rustls` feature pulls `webpki-roots`.

The runner-up is **a thin layer of our own** (no new dependency; it passes the whole conformance suite today). If the maintainers weigh build weight over avoiding hand-written SQL, take that; the section "What would change this" says when.

## What was tried

Four candidates went through the same gates. Three were ported far enough to run the golden fixtures of #612 (`crates/henk-store/tests/golden.rs`), on SQLite and, by hand, on PostgreSQL 17.11. The goldens come from #612 and are not on this tree: `golden.rs` exists only on `test/store-golden-fixtures` and the spike branches built on it, so #612 must be merged before the goldens can be run from main.

| | diesel | sqlx 0.9 (`Any` driver) | **sea-orm 2.0** | thin layer of our own |
|---|---|---|---|---|
| One implementation for both databases | no: `MultiConnection` is synchronous and its PostgreSQL connection needs libpq | yes | yes | yes |
| No database at build time | yes | yes (`query()`, not `query!`) | yes | yes |
| No system library, no banned crate | no: libpq through `pkg-config`, or a bundled libpq with OpenSSL (banned) or without TLS | yes | yes | yes |
| `cargo deny check` | passes in a probe | passes | passes | passes |
| Golden fixtures, SQLite, fresh | not tried | pass | pass | pass |
| Golden fixtures, SQLite, adopted (a file the current code made, 020) | | pass | pass | pass |
| Golden fixtures, PostgreSQL, fresh | | pass | pass | pass |
| Golden fixtures, PostgreSQL, adopted (a schema the current store made, 016) | | pass | pass | pass |
| Eight writers, fifty writes each, one SQLite file | | pass, no "database is locked" | pass | pass |
| Whole trait ported | | 33 of about 60 methods | 30 of about 60 | all (the conformance suite passes on both databases) |
| Hand-written SQL | | 45 statements, plus 5 per-dialect fragments | none: the builder; 3 raw fragments in queries | 63 statements, plus 2 per-dialect fragments |
| Per-dialect migration text | | whole-schema sections per dialect (356 lines) | the DSL, with 3 helpers for the column types, identity and times; 1 raw (the expression index) | 30 files, 8 of the 17 steps differ by dialect |
| Crates in `cargo tree -p henk-store` (today 130) | not measured in the crate (112 in a probe) | 211 (+81); 175 (+45) once the old PostgreSQL driver is gone | 272 (+142); 238 (+108) once it is gone | 130 (+0) |
| Cold `cargo build -p henk-store` (today 10.0 s) | | 12.6 s | 24.1 s | 10.9 s |
| Then `cargo test --no-run --test golden` (today 6.6 s) | | 8.4 s | 18.9 s | 7.5 s |
| Warm, after touching `lib.rs` (today 2.5 s) | | 2.8 s | 4.6 s | 3.4 s |
| Lines (today 3,423 for the two stores) | | 1,602 + 383 of SQL for 33 methods | 2,234 for 30 methods | 2,400 for all of it |

How the numbers were made, on each branch: a private `--target-dir` and `CARGO_BUILD_BUILD_DIR`, sccache off (`RUSTC_WRAPPER=`), `CARGO_INCREMENTAL=0`, 8 jobs; `cargo build --locked -p henk-store` and then `cargo test --locked -p henk-store --no-run --test golden`, from nothing, three times; and the same test build after `touch crates/henk-store/src/lib.rs`, three times. The three runs of each differ by at most 1.3 s. The crate counts are `cargo tree --locked -p henk-store -e normal --prefix none | sort -u | wc -l`; the "once the old PostgreSQL driver is gone" counts are the same with `tokio-postgres`, `deadpool-postgres`, `rustls-platform-verifier` and `rustls` taken out of `henk-store`'s `Cargo.toml`. The cold builds of sqlx and sea-orm include the old PostgreSQL store, which the final state drops, so they are a little high. The "lines" of sqlx and sea-orm are for the methods they ported; the rest of the trait is mostly one `INSERT` or `UPDATE` each, so I expect about 2,300 to 2,600 lines for sqlx and 2,900 to 3,200 for sea-orm for the whole trait, against 3,423 today.

### diesel: out at the gates

diesel's way to serve both databases from one implementation is `MultiConnection`, which takes synchronous connections; its PostgreSQL one is `PgConnection`, which needs `libpq`: a system library found through `pkg-config` (the Docker build stage and every developer machine would need it), or a bundled one that either builds with OpenSSL (`deny.toml` bans `openssl-sys`) or has no TLS (`bundled_without_openssl`), and Henk needs TLS to PostgreSQL. `diesel-async` covers PostgreSQL over `tokio-postgres` without libpq and has no multi-backend connection (`diesel-async` 0.9.2 has no `AsyncMultiConnection`). With `postgres_backend` and `diesel-async` it builds and passes `cargo deny` (112 crates in a probe), and its `libsqlite3-sys` is 0.38.2, the same as `rusqlite` 0.40, but that gives two code paths, not one.

### sqlx: works, but the `Any` driver pushes the work back to us

sqlx's `Any` driver has no time type and no `u64`, so on PostgreSQL every time is read through `to_char` and trimmed of trailing zeros (`rtrim(rtrim(to_char(...)))`) and written through a `::timestamptz` cast, and a `Dialect` seam with five fragments holds that. Its migrator runs plain SQL files and cannot hold a step with different text per database, so the spike wrote a runner on top (a file with `-- @sqlite`, `-- @postgres` and `-- @both` sections) and the first step is the whole schema written once per dialect. What sqlx removes is the driver, the pool and the row decoding; what remains is the hand-written SQL, the seam and the migrator, which is what the thin layer also has. It adds 45 crates in the final state.

### the thin layer: complete, small, and a mini-ORM of our own

One `Store` over a seam (`Value`, `Row`, `Dialect`, `Db`), `rusqlite` on a small pool on the blocking thread pool, `tokio-postgres` as today, and a runner for one numbered sequence with a SQL file per dialect where they differ. It adds no dependency and 0.9 s to a cold build, and it is the only candidate that ported the whole trait: the conformance suite runs on it and passes, on SQLite (45 scenarios) and PostgreSQL (42), beside the old stores' own runs, and a test checks that it and the old stores make the same schema and render the same report. What it does not do is take the work of the dialects away: the SQL stays hand-written (63 statements) and the seam, the SQLite pool, the placeholder translation and the migrator are ours to keep, about 1,000 lines.

### sea-orm: the builder takes the dialects

`sea-query` renders one description of a query as SQLite or PostgreSQL SQL, with the placeholders, quoting and `LIMIT`/`OFFSET` of each, and `sea-orm-migration`'s DSL does the same for the tables and indexes. The spike used it as a query builder and migrator only (no entities). Three fragments stayed raw in the queries: SQLite's `substr` expression that makes a stored time compare as an instant (#216), the column that keeps `event_outcomes` rows in recording order (`rowid` against `id`), and the UTC day of a stored time. The migrator has two more (SQLite's `PRAGMA user_version` and a probe for the old PostgreSQL table) and the expression index of step 10. The cost is weight: 108 more crates once the old driver is gone, and a cold build of the store crate that takes 24 s instead of 10.

### Why sea-orm

The gates leave three survivors. The rule set before the spike was: fewest raw SQL fragments and per-dialect migration steps first, then fewest new crates and the smallest compile-time increase, then least code. By the first criterion sea-orm wins by a wide margin, and it is the one that is an ORM layer in the sense #598 asked for: the SQL for the two databases is generated from one description instead of kept as text with placeholders translated by hand, so a query the builder generates has the same shape on both. That holds for builder-generated queries only. The three raw fragments are written per dialect, they are the SQL most likely to differ between the two databases, and the builder does not cover them: CI on SQLite never runs the PostgreSQL side of them, and only the manual PostgreSQL run below does. Nor is the evidence even: the sea-orm port is half done (30 of about 60 methods) and the thin layer is the only candidate that passes the whole conformance suite on both databases, so the line estimate for the full sea-orm trait is a guess. By the second it is last, by a lot in crates and in cold build time (24 s against 10 s, a build that CI pays for in every job), and the maintainers have said a dependency is acceptable. The third is a draw that does not matter.

## What would change this

- **Build weight matters more than the builder.** The cold build of `henk-store` 2.4 times as long, 108 more crates in `Cargo.lock` to audit, and a release build of them in the Docker image. If that is too much, choose the thin layer: nothing new, and the work is nearly done on `spike/store-thin`.
- **sea-orm 2.0 is young.** It is a new major version. The spike met no bug in what it used, and it is only the builder and migrator (sqlx is under it), but a release that changes the query builder's API would cost a port.
- **A feature the spike did not use.** If entities and relations would later be wanted, sea-orm has them; the thin layer never will.

## The migrations

The 36 existing migrations are one history of 17 steps (SQLite 001 to 020, PostgreSQL 001 to 016, same changes in the same order, PostgreSQL's first covering SQLite's 001 to 003 and 005, and SQLite's 013 having no counterpart).

| Step | SQLite | PostgreSQL | What | Class |
|---|---|---|---|---|
| 1 | 001, 002, 003, 005 | 001 | the first schema: runs, lanes, findings, events, requests, inbound events, outcomes, their indexes | per-dialect (identity columns, `TEXT` vs `TIMESTAMPTZ`, `INTEGER` vs `BIGINT`) |
| 2 | 004 | 002 | indexes `runs_started`, `runs_status` | shared SQL |
| 3 | 006 | 003 | `inbound_events.requester` | shared SQL |
| 4 | 007 | 004 | `tool_calls` | per-dialect |
| 5 | 008 | 005 | `transcripts` | per-dialect |
| 6 | 009 | 006 | `drafts` | per-dialect |
| 7 | 010 | 007 | `runs.superseded_by` | shared SQL |
| 8 | 011 | 008 | `stages` | per-dialect (times) |
| 9 | 012 | 009 | indexes `findings_created`, `drafts_created` | shared SQL |
| 10 | 013 | none | expression index `runs_started_instant` (#216) | SQLite only |
| 11 | 014 | 010 | `runs.loop_stop`, `loop_rounds` | shared SQL |
| 12 | 015 | 011 | cache token columns | shared if the type is `BIGINT` (SQLite accepts the name) |
| 13 | 016 | 012 | `lanes` becomes `sessions`, adds `role`, renames `lane` to `session` | shared body, per-dialect tail (PostgreSQL also renames an index and drops a default) |
| 14 | 017 | 013 | `session_rounds` | per-dialect |
| 15 | 018 | 014 | `sessions.reasoning_tokens` | shared if `BIGINT` |
| 16 | 019 | 015 | cut tool names over 64 characters | shared if written with `substr` and `length`; a no-op on a fresh database |
| 17 | 020 | 016 | `run_children` | shared SQL |

Six steps share their SQL, three can if written for both, six need their own per database, one has a per-database tail and one is SQLite's alone.

The two final schemas differ by more than types: PostgreSQL has indexes SQLite lacks (`events_run`, `findings_run`, `sessions_run`) and `id` identity columns on `event_outcomes` and `requests`; SQLite has `runs_started_instant`. The sea-orm spike adds the three indexes to SQLite in a step of their own, so the two end alike; the identity columns stay as they are (nothing reads them but the recording order).

### How the spike did it with sea-orm

Four steps: `m001_baseline` (every table of SQLite 019 / PostgreSQL 015, with its indexes, in the DSL), `m002_run_instant` (the expression index, raw, SQLite only), `m003_run_children` (the end of SQLite 020 / PostgreSQL 016), and `m004_run_indexes` (the three PostgreSQL-only indexes, for both). Adoption is one function run before the migrator: it reads `PRAGMA user_version` on SQLite and `henk_schema_migrations` on PostgreSQL, and when the database is at the end of the old history (20, or 16) it records the first three steps as applied in `seaql_migrations` and changes nothing else, so the old bookkeeping stays where it is (the previous Henk looks only at that bookkeeping, so it should still open the database; not tested). PostgreSQL takes an advisory lock around it, as the old migrator did, and a test starts two connections at once. A database at another version of the old history is refused with a message that names the version to reach.

### Decision left for the maintainers: the old history

The spike squashes: the new sequence starts at the end of the old history, and a database *below* SQLite 020 / PostgreSQL 016 is refused with "open it once with a Henk that still has the previous migrator". The alternative is to carry all 17 steps as real steps so any old database upgrades in one go, which is what the thin layer does (a test migrates a SQLite database from the middle of the history). Squashing is smaller and Henk is before 1.0, and the data steps that matter for old rows (the `role` backfill of step 13, the tool-name cut of step 16) only concern databases older than a few releases. I recommend squashing, with the refusal message, and saying so in the release notes of the release that ships it. It is not free to reverse: steps cannot be put in front of the baseline without renumbering, so the choice is better made now. This is the one decision in this record I would like a maintainer to confirm.

### Times and numbers stay as they are

SQLite keeps its times as RFC 3339 text and compares them through the same `substr` expression as today (and the same expression index); PostgreSQL keeps `TIMESTAMPTZ`. No converting migration is needed, and every golden of #612 passes unchanged on all three candidates. Two differences that the goldens recorded stay as they are for now: SQLite returns nanoseconds where PostgreSQL returns microseconds, and SQLite lists inbound events by their time as *text* (a later `10:00:00.4Z` after `10:00:00Z`), which PostgreSQL does not. The port orders inbound events by instant on both, and blesses that one golden in the pull request that moves them, with the reason in its description.

### The numbering from the first new migration on

One counter and one step per change: `m005_...` onwards in the sequence of `sea-orm-migration`, named for what they do. A change that needs different DDL per database says so inside its step. The README next to the migrations will say how to add one.

## Scope facts

Nothing outside `crates/henk-store` matches on a `StoreError` variant. `dashboard/api/mod.rs` converts one into a logged `ApiError` with a fixed message, `?` turns it into `anyhow` elsewhere, and `crates/henk/src/live.rs` repeats `Result<_, StoreError>` in the signatures of its `Announcing` wrapper and the `Paused` test double. Nothing outside the store names `rusqlite`, `tokio_postgres`, `deadpool_postgres` or `rusqlite_migration`. So replacing the driver-typed variants with string-carrying ones breaks no caller. `SqliteStore::open`, `SqliteStore::in_memory`, `PgStore::connect` and `henk_store::describe_url` are the constructors used elsewhere (`app.rs`, `doctor.rs` and tests) and stay as thin wrappers over the new type.

## How PostgreSQL is checked, since CI has none

By hand, against a throwaway container, at the end of the port and before a release:

```
docker run -d --rm --name henk-pg -e POSTGRES_PASSWORD=henk-ci -p 127.0.0.1:55436:5432 postgres:17-alpine
HENK_TEST_DATABASE_URL='postgres://postgres:henk-ci@127.0.0.1:55436/postgres?sslmode=disable' \
  cargo test -p henk-store --all-features --locked -- --ignored
```

The goldens of #612 and the conformance suite's PostgreSQL scenarios are the checks. Until #612 is merged, only the conformance suite is on this tree, and the command above runs just that. The spike ran them on PostgreSQL 17.11 (`postgres:17-alpine`). With one implementation the PostgreSQL scenarios stay as `#[ignore]`d twins.

## What changes in the sub-issues

The split stays: #604 (migrations), #605 (the `Store` type and the runs), #606 and #607 (the rest of the trait), #608 (remove the old code, CI on SQLite).

- #604 follows "How the spike did it with sea-orm" and settles the squash question above; its tests are the ones in the spike (adopt a database the current code made, refuse another version, two connections migrating at once, a fresh database and an adopted one end with the same schema).
- #605 starts from `spike/store-sea-orm` and adds a test that the run listing uses the `runs_started_instant` index (the sqlx spike had one; the plan of that query changes if its expression drifts from the index's).
- #607 orders inbound events by instant and blesses `sqlite/inbound_events.txt` with the reason.
- #608 is as written. It also deletes `spike/*`.

## Risks left

- A SQLite-only CI can pass where PostgreSQL fails (concurrent writers, `RETURNING`, collation, integer width). The hand check above is the cover. The builder's one description per query lowers the risk only for the queries it generates; the three raw fragments are per dialect and have only the hand check.
- The cold build of `henk-store` and the Docker image get heavier (24 s against 10 s on this machine, 108 more crates to audit with `cargo deny`).
- The goldens are tied to the table layout through `seed.sql`; a migration that changes a table changes the seed in the same pull request.

## Evidence

The branches are throwaway and are deleted when #608 merges: `spike/store-sea-orm` (4687fa8), `spike/store-sqlx` (856efd9), `spike/store-thin` (4907319), all based on `test/store-golden-fixtures` (#612). The commands below work only on those branches, not on main until #612 is merged. Each can be checked with `cargo test -p henk-store --locked --test golden` and, with `HENK_TEST_DATABASE_URL` set, `-- --ignored`. The probes for the gates (a scratch crate per candidate with the repo's `deny.toml`) and the timing script are described in the pull request that adds this record.
