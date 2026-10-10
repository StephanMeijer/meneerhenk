//! `PostgreSQL`: a small connection pool, TLS on rustls with the platform's
//! roots, and native timestamps. Behaves exactly like [`crate::SqliteStore`];
//! `tests/conformance.rs` holds both to that.

use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use henk_domain::run::{EventId, RunId};
use rustls_platform_verifier::BuilderVerifierExt as _;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio_postgres::Row;
use tokio_postgres::config::Host;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::store::RunStore;
use crate::types::{
    DayCounts, DayRates, DraftDecision, DraftFilter, DraftGroup, DraftListing, DraftRates,
    DraftRecord, EventFacets, EventFilter, EventRecord, EventWithOutcomes, FindingAction,
    FindingRecord, InboundEvent, LaneEnding, LaneRecord, LaneStatus, MAX_PAYLOAD_BYTES,
    MOST_LANE_REVIEWS, NewRun, OutcomeFilter, OutcomeRecord, OutcomeRow, Page, PruneCounts, RawRun,
    RunFilter, RunRecord, RunStatus, Stage, StageRecord, StageState, StageWrite, StoreError,
    ToolCallFilter, ToolCallListing, ToolCallRecord, ToolUsage, TranscriptRecord,
    TranscriptSummary, VerdictFilter, attach_outcomes, draft_verdict, kind_parse, kind_str,
    lane_dots, lane_endings, merge_days, platform_parse, platform_str, stage_dots, stage_records,
    status_str, to_i64, to_u64,
};

/// Schema migrations, applied in order. Only ever append.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/postgres/001_initial.sql"),
    include_str!("../migrations/postgres/002_dashboard.sql"),
    include_str!("../migrations/postgres/003_event_requester.sql"),
    include_str!("../migrations/postgres/004_tool_calls.sql"),
    include_str!("../migrations/postgres/005_transcripts.sql"),
    include_str!("../migrations/postgres/006_drafts.sql"),
    include_str!("../migrations/postgres/007_superseded_by.sql"),
    include_str!("../migrations/postgres/008_stages.sql"),
    include_str!("../migrations/postgres/009_stats.sql"),
    include_str!("../migrations/postgres/010_loop.sql"),
];

/// Serialises migrations between Henk processes starting together.
const MIGRATION_LOCK: i64 = 0x4865_6e6b; // "Henk"

/// Connections kept open at most. Henk's writes are small and few.
const POOL_SIZE: usize = 8;

/// How long a new connection may take when the URL does not say.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The run store over `PostgreSQL`.
#[derive(Debug)]
pub struct PgStore {
    pool: Pool,
}

impl PgStore {
    /// Connects to the database at `url` and brings its schema up to date.
    /// `sslmode` in the URL decides TLS, as usual for `PostgreSQL`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the URL does not parse, the server cannot
    /// be reached, or migrating fails. The URL itself is never in the error.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let mut config = parse_url(url)?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(CONNECT_TIMEOUT);
        }
        let manager = Manager::from_config(
            config,
            tls()?,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(POOL_SIZE)
            .build()
            .map_err(|error| StoreError::Connect(error.to_string()))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), StoreError> {
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        transaction
            .execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
            .await?;
        transaction
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS henk_schema_migrations (
                    version    INTEGER PRIMARY KEY,
                    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
                 )",
            )
            .await?;
        let applied: i32 = transaction
            .query_one(
                "SELECT COALESCE(MAX(version), 0) FROM henk_schema_migrations",
                &[],
            )
            .await?
            .try_get(0)?;
        let known = i32::try_from(MIGRATIONS.len()).unwrap_or(i32::MAX);
        if applied > known {
            return Err(StoreError::Schema(format!(
                "the database is at schema version {applied}, newer than the {known} this Henk knows"
            )));
        }
        for (version, sql) in (1..).zip(MIGRATIONS) {
            if version <= applied {
                continue;
            }
            transaction.batch_execute(sql).await?;
            transaction
                .execute(
                    "INSERT INTO henk_schema_migrations (version) VALUES ($1)",
                    &[&version],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, StoreError> {
        Ok(self.pool.get().await?)
    }
}

/// Where `url` points, without user or password: for logs and `henk doctor`.
///
/// # Errors
///
/// Returns [`StoreError::Connect`] when the URL does not parse.
pub fn describe_url(url: &str) -> Result<String, StoreError> {
    let config = parse_url(url)?;
    let ports = config.get_ports();
    let hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .enumerate()
        .map(|(i, host)| {
            let host = match host {
                Host::Tcp(name) => name.clone(),
                Host::Unix(path) => path.display().to_string(),
            };
            match ports.get(i).or_else(|| ports.first()) {
                Some(port) => format!("{host}:{port}"),
                None => host,
            }
        })
        .collect();
    Ok(format!(
        "{}/{}",
        hosts.join(","),
        config.get_dbname().unwrap_or("")
    ))
}

fn parse_url(url: &str) -> Result<tokio_postgres::Config, StoreError> {
    // The parse error can quote the URL, and the URL holds the password.
    url.parse()
        .map_err(|_| StoreError::Connect("the database URL does not parse".to_owned()))
}

fn tls() -> Result<MakeRustlsConnect, StoreError> {
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| StoreError::Connect(error.to_string()))?
    .with_platform_verifier()
    .map_err(|error| StoreError::Connect(error.to_string()))?
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// A stored turn count, refused when it is not one.
fn turns(value: i64) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::Corrupt {
        column: "transcripts.turns",
        value: value.to_string(),
    })
}

fn text(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// A caller-supplied RFC 3339 time; empty means now.
fn parse_time(column: &'static str, value: &str) -> Result<OffsetDateTime, StoreError> {
    if value.is_empty() {
        return Ok(now());
    }
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| StoreError::Corrupt {
        column,
        value: value.to_owned(),
    })
}

/// The columns [`raw_run`] reads, in order.
/// The `WHERE` of a draft count or listing over `drafts d JOIN runs r`,
/// over parameters 1 to 5: model, lane, repo, since and until.
const DRAFT_FILTER: &str =
    "($1::text IS NULL OR d.model = $1) AND ($2::text IS NULL OR d.lane = $2)
     AND ($3::text IS NULL OR r.repo = $3)
     AND ($4::timestamptz IS NULL OR d.created_at >= $4)
     AND ($5::timestamptz IS NULL OR d.created_at < $5)
     AND d.kind <> 'loop_finding'";

/// One count per verdict, and the waiting ones, in [`DraftRates`] order.
const VERDICT_SUMS: &str = "COUNT(*) FILTER (WHERE d.verdict = 'confirmed'),
     COUNT(*) FILTER (WHERE d.verdict = 'rejected'),
     COUNT(*) FILTER (WHERE d.verdict = 'same_as'),
     COUNT(*) FILTER (WHERE d.verdict = 'unchecked'),
     COUNT(*) FILTER (WHERE d.verdict = 'not_checked'),
     COUNT(*) FILTER (WHERE d.verdict = 'cancelled'),
     COUNT(*) FILTER (WHERE d.verdict = 'failed'),
     COUNT(*) FILTER (WHERE d.verdict IS NULL)";

/// The time bounds of a draft or tool call count or listing.
fn window(
    since: Option<&String>,
    until: Option<&String>,
) -> Result<(Option<OffsetDateTime>, Option<OffsetDateTime>), StoreError> {
    let time = |column, value: Option<&String>| value.map(|v| parse_time(column, v)).transpose();
    Ok((time("since", since)?, time("until", until)?))
}

/// The `WHERE` of a tool call tally or listing over `tool_calls c`, over
/// parameters 1 to 5: tool, model, kind of session, since and until. The
/// kind is the session name's, as [`crate::session_kind`] reads it.
const CALL_FILTER: &str = "($1::text IS NULL OR c.tool = $1) AND ($2::text IS NULL OR c.model = $2)
     AND ($3::text IS NULL OR ($3 = 'check' AND c.session LIKE 'check-%')
          OR ($3 = c.session AND c.session IN ('planner', 'address'))
          OR ($3 = 'lane' AND c.session NOT LIKE 'check-%' AND c.session NOT IN ('planner', 'address')))
     AND ($4::timestamptz IS NULL OR c.at >= $4)
     AND ($5::timestamptz IS NULL OR c.at < $5)";

/// The `WHERE` of a run listing, over parameters 1 to 9: kind, status,
/// platform, repo, target, since, until, and the keyset (time, id).
const RUN_FILTER: &str = "($1::text IS NULL OR kind = $1) AND ($2::text IS NULL OR status = $2)
     AND ($3::text IS NULL OR platform = $3) AND ($4::text IS NULL OR repo = $4)
     AND ($5::bigint IS NULL OR target = $5)
     AND ($6::timestamptz IS NULL OR started_at >= $6)
     AND ($7::timestamptz IS NULL OR started_at < $7)
     AND ($8::timestamptz IS NULL OR started_at < $8 OR (started_at = $8 AND id < $9::text))";

/// A run filter as query parameters.
struct RunParams<'a> {
    kind: Option<&'static str>,
    status: Option<&'static str>,
    platform: Option<&'static str>,
    target: Option<i64>,
    since: Option<OffsetDateTime>,
    until: Option<OffsetDateTime>,
    before_at: Option<OffsetDateTime>,
    before_id: Option<&'a str>,
}

impl<'a> RunParams<'a> {
    fn of(filter: &'a RunFilter) -> Result<Self, StoreError> {
        let time =
            |column, value: Option<&String>| value.map(|v| parse_time(column, v)).transpose();
        Ok(Self {
            kind: filter.kind.map(kind_str),
            status: filter.status.map(status_str),
            platform: filter.platform.map(platform_str),
            target: filter.target.map(|t| i64::try_from(t).unwrap_or(i64::MAX)),
            since: time("since", filter.since.as_ref())?,
            until: time("until", filter.until.as_ref())?,
            before_at: time("started_at", filter.before.as_ref().map(|k| &k.started_at))?,
            before_id: filter.before.as_ref().map(|k| k.id.as_str()),
        })
    }
}

const RUN_COLUMNS: &str = "id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, finished_at, link, summary, error, heartbeat_at, check_id, superseded_by, loop_stop, loop_rounds";

fn raw_run(row: &Row) -> Result<RawRun, StoreError> {
    let at = |i: usize| -> Result<String, StoreError> { Ok(text(row.try_get(i)?)) };
    let maybe_at = |i: usize| -> Result<Option<String>, StoreError> {
        Ok(row.try_get::<_, Option<OffsetDateTime>>(i)?.map(text))
    };
    Ok(RawRun {
        id: row.try_get(0)?,
        kind: row.try_get(1)?,
        platform: row.try_get(2)?,
        repo: row.try_get(3)?,
        target: row.try_get(4)?,
        commit: row.try_get(5)?,
        requester: row.try_get(6)?,
        trigger: row.try_get(7)?,
        status: row.try_get(8)?,
        started_at: at(9)?,
        finished_at: maybe_at(10)?,
        link: row.try_get(11)?,
        summary: row.try_get(12)?,
        error: row.try_get(13)?,
        heartbeat_at: maybe_at(14)?,
        check_id: row.try_get(15)?,
        superseded_by: row.try_get(16)?,
        loop_stop: row.try_get(17)?,
        loop_rounds: row.try_get::<_, Option<i32>>(18)?.map(i64::from),
    })
}

fn inbound_event(row: &Row) -> Result<InboundEvent, StoreError> {
    let id: String = row.try_get(0)?;
    let id = EventId::parse(id.clone()).map_err(|_| StoreError::Corrupt {
        column: "inbound_events.id",
        value: id,
    })?;
    let target: Option<i64> = row.try_get(5)?;
    Ok(InboundEvent {
        id,
        received_at: text(row.try_get(1)?),
        source: row.try_get(2)?,
        kind: row.try_get(3)?,
        repo: row.try_get(4)?,
        target: target
            .map(|t| to_u64("inbound_events.target", t))
            .transpose()?,
        payload: row.try_get(6)?,
        requester: row.try_get(7)?,
    })
}

#[async_trait]
impl RunStore for PgStore {
    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        let started = now();
        self.client()
            .await?
            .execute(
                "INSERT INTO runs (id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, link, heartbeat_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $10)",
                &[
                    &run.id.as_str(),
                    &kind_str(run.kind),
                    &platform_str(run.platform),
                    &run.repo,
                    &to_i64("runs.target", run.target)?,
                    &run.commit,
                    &run.requester,
                    &run.trigger,
                    &RunStatus::Running.as_str(),
                    &started,
                    &run.link,
                ],
            )
            .await?;
        Ok(())
    }

    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "UPDATE runs SET status = $2, finished_at = $3, summary = $4, error = $5 WHERE id = $1",
                &[&id.as_str(), &status.as_str(), &now(), &summary, &error],
            )
            .await?;
        Ok(())
    }

    async fn supersede_run(&self, id: &RunId, by: &RunId, reason: &str) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "UPDATE runs SET status = $2, finished_at = $3, error = $4, superseded_by = $5 WHERE id = $1",
                &[
                    &id.as_str(),
                    &RunStatus::Superseded.as_str(),
                    &now(),
                    &reason,
                    &by.as_str(),
                ],
            )
            .await?;
        Ok(())
    }

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        self.client()
            .await?
            .query_opt(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = $1"),
                &[&id.as_str()],
            )
            .await?
            .map(|row| raw_run(&row)?.into_record())
            .transpose()
    }

    async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "UPDATE runs SET heartbeat_at = $2 WHERE id = $1",
                &[&id.as_str(), &now()],
            )
            .await?;
        Ok(())
    }

    async fn end_loop(&self, id: &RunId, stop: &str, rounds: u32) -> Result<(), StoreError> {
        let rounds = i32::try_from(rounds).unwrap_or(i32::MAX);
        self.client()
            .await?
            .execute(
                "UPDATE runs SET loop_stop = $2, loop_rounds = $3 WHERE id = $1",
                &[&id.as_str(), &stop, &rounds],
            )
            .await?;
        Ok(())
    }

    async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "UPDATE runs SET check_id = $2 WHERE id = $1",
                &[&id.as_str(), &check_id],
            )
            .await?;
        Ok(())
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        self.client()
            .await?
            .query(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM runs
                     WHERE status = $1 AND (heartbeat_at IS NULL OR heartbeat_at < $2)
                     ORDER BY started_at, id"
                ),
                &[&RunStatus::Running.as_str(), &stale_before],
            )
            .await?
            .iter()
            .map(|row| raw_run(row)?.into_record())
            .collect()
    }

    async fn drop_running_lanes(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "UPDATE lanes SET status = $2, finished_at = $3, error = $4
                 WHERE run_id = $1 AND status = $5",
                &[
                    &run.as_str(),
                    &LaneStatus::DidNotFinish.as_str(),
                    &now(),
                    &reason,
                    &LaneStatus::Running.as_str(),
                ],
            )
            .await?;
        Ok(())
    }

    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
        let client = self.client().await?;
        let day = |column: &str| format!("to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD')");
        let runs = client
            .query(
                &format!(
                    "SELECT {}, COUNT(*), COUNT(*) FILTER (WHERE status = 'finished'),
                        COUNT(*) FILTER (WHERE status = 'failed')
                     FROM runs WHERE started_at >= $1 GROUP BY 1",
                    day("started_at")
                ),
                &[&since],
            )
            .await?
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let findings = client
            .query(
                &format!(
                    "SELECT {}, COUNT(*) FROM findings
                     WHERE action IN ('posted', 'unverified') AND created_at >= $1 GROUP BY 1",
                    day("created_at")
                ),
                &[&since],
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let drafts = client
            .query(
                &format!(
                    "SELECT {}, COUNT(*) FROM drafts
                     WHERE created_at >= $1 AND kind <> 'loop_finding' GROUP BY 1",
                    day("created_at")
                ),
                &[&since],
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(merge_days(runs, findings, drafts))
    }

    async fn lanes_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, LaneStatus)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<&str> = runs.iter().map(RunId::as_str).collect();
        let rows = self
            .client()
            .await?
            .query(
                "SELECT run_id, name, status FROM lanes WHERE run_id = ANY($1) ORDER BY run_id, name",
                &[&ids],
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?, row.try_get(2)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        lane_dots(rows)
    }

    async fn lane_endings(
        &self,
        since: Option<OffsetDateTime>,
        reviews: u32,
    ) -> Result<Vec<LaneEnding>, StoreError> {
        let reviews = i64::from(reviews.min(MOST_LANE_REVIEWS));
        let rows = self
            .client()
            .await?
            .query(
                "SELECT r.id, r.started_at, l.name, l.model, l.status, l.error
                 FROM (SELECT id, started_at FROM runs
                       WHERE kind = 'review' AND status NOT IN ('running', 'superseded', 'cancelled')
                         AND ($1::timestamptz IS NULL OR started_at >= $1)
                       ORDER BY started_at DESC, id DESC LIMIT $2) r
                 JOIN lanes l ON l.run_id = r.id
                 ORDER BY r.started_at DESC, r.id DESC, l.name",
                &[&since, &reviews],
            )
            .await?
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    text(row.try_get(1)?),
                    row.try_get(2)?,
                    row.try_get(3)?,
                    row.try_get(4)?,
                    row.try_get(5)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        lane_endings(rows)
    }

    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<&str> = runs.iter().map(RunId::as_str).collect();
        let rows = self
            .client()
            .await?
            .query(
                "SELECT run_id, stage, state FROM stages WHERE run_id = ANY($1)",
                &[&ids],
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?, row.try_get(2)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_dots(rows)
    }

    async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError> {
        let started = write.started_at.unwrap_or_else(now);
        let ended = write
            .state
            .ended()
            .then(|| write.ended_at.unwrap_or_else(now));
        self.client()
            .await?
            .execute(
                "INSERT INTO stages (run_id, stage, state, started_at, ended_at, detail)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (run_id, stage) DO UPDATE SET
                     state = excluded.state, ended_at = excluded.ended_at, detail = excluded.detail",
                &[
                    &run.as_str(),
                    &write.stage.as_str(),
                    &write.state.as_str(),
                    &started,
                    &ended,
                    &write.detail,
                ],
            )
            .await?;
        Ok(())
    }

    async fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "UPDATE stages SET state = $2, ended_at = $3, detail = $4
                 WHERE run_id = $1 AND state = $5",
                &[
                    &run.as_str(),
                    &StageState::Failed.as_str(),
                    &now(),
                    &reason,
                    &StageState::Running.as_str(),
                ],
            )
            .await?;
        Ok(())
    }

    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT stage, state, started_at, ended_at, detail FROM stages WHERE run_id = $1",
                &[&run.as_str()],
            )
            .await?;
        let rows = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get::<_, String>(0)?,
                    row.try_get::<_, String>(1)?,
                    text(row.try_get(2)?),
                    row.try_get::<_, Option<OffsetDateTime>>(3)?.map(text),
                    row.try_get::<_, String>(4)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_records(rows)
    }

    async fn start_lane(&self, run: &RunId, name: &str, model: &str) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO lanes (run_id, name, model, status, started_at) VALUES ($1, $2, $3, $4, $5)",
                &[
                    &run.as_str(),
                    &name,
                    &model,
                    &LaneStatus::Running.as_str(),
                    &now(),
                ],
            )
            .await?;
        Ok(())
    }

    async fn finish_lane(
        &self,
        run: &RunId,
        name: &str,
        status: LaneStatus,
        turns: u64,
        input_tokens: u64,
        output_tokens: u64,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        // Counts beyond i64 cannot happen; saturate like the SQLite store.
        let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        self.client()
            .await?
            .execute(
                "UPDATE lanes SET status = $3, turns = $4, input_tokens = $5, output_tokens = $6, finished_at = $7, error = $8
                 WHERE run_id = $1 AND name = $2",
                &[
                    &run.as_str(),
                    &name,
                    &status.as_str(),
                    &count(turns),
                    &count(input_tokens),
                    &count(output_tokens),
                    &now(),
                    &error,
                ],
            )
            .await?;
        Ok(())
    }

    async fn lanes(&self, run: &RunId) -> Result<Vec<LaneRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT name, model, status, turns, input_tokens, output_tokens, error, started_at, finished_at FROM lanes WHERE run_id = $1 ORDER BY name",
                &[&run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let status: String = row.try_get(2)?;
                let status = LaneStatus::parse(&status).ok_or(StoreError::Corrupt {
                    column: "lanes.status",
                    value: status,
                })?;
                Ok(LaneRecord {
                    name: row.try_get(0)?,
                    model: row.try_get(1)?,
                    status,
                    turns: to_u64("lanes.turns", row.try_get(3)?)?,
                    input_tokens: to_u64("lanes.input_tokens", row.try_get(4)?)?,
                    output_tokens: to_u64("lanes.output_tokens", row.try_get(5)?)?,
                    error: row.try_get(6)?,
                    started_at: text(row.try_get(7)?),
                    finished_at: row.try_get::<_, Option<OffsetDateTime>>(8)?.map(text),
                })
            })
            .collect()
    }

    async fn record_finding(
        &self,
        run: &RunId,
        lane: &str,
        path: &str,
        line_number: u32,
        comment_id: &str,
        action: FindingAction,
    ) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO findings (run_id, lane, path, line, comment_id, action, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &run.as_str(),
                    &lane,
                    &path,
                    &i64::from(line_number),
                    &comment_id,
                    &action.as_str(),
                    &now(),
                ],
            )
            .await?;
        Ok(())
    }

    async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError> {
        let at = parse_time("drafts.created_at", &draft.at)?;
        self.client()
            .await?
            .execute(
                "INSERT INTO drafts (run_id, draft, lane, model, kind, path, line, target, body, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (run_id, draft) DO UPDATE SET kind = excluded.kind, target = excluded.target, body = excluded.body",
                &[
                    &run.as_str(),
                    &draft.draft,
                    &draft.lane,
                    &draft.model,
                    &draft.kind,
                    &draft.path,
                    &i64::from(draft.line),
                    &draft.target,
                    &draft.body,
                    &at,
                ],
            )
            .await?;
        Ok(())
    }

    async fn decide_draft(
        &self,
        run: &RunId,
        draft: &str,
        decision: &DraftDecision,
    ) -> Result<(), StoreError> {
        let at = parse_time("drafts.decided_at", &decision.at)?;
        self.client()
            .await?
            .execute(
                "UPDATE drafts SET verdict = $3, checker = $4, reason = $5, same_as = $6, comment_id = $7, decided_at = $8 WHERE run_id = $1 AND draft = $2",
                &[
                    &run.as_str(),
                    &draft,
                    &decision.verdict.as_str(),
                    &decision.checker,
                    &decision.reason,
                    &decision.same_as,
                    &decision.comment_id,
                    &at,
                ],
            )
            .await?;
        Ok(())
    }

    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT created_at, draft, lane, model, kind, path, line, target, body, verdict, checker, reason, same_as, comment_id, decided_at FROM drafts WHERE run_id = $1 ORDER BY id",
                &[&run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let line: i64 = row.try_get(6)?;
                let verdict: Option<String> = row.try_get(9)?;
                let decision = match verdict {
                    Some(verdict) => Some(DraftDecision {
                        at: row
                            .try_get::<_, Option<OffsetDateTime>>(14)?
                            .map(text)
                            .unwrap_or_default(),
                        verdict: draft_verdict(&verdict)?,
                        checker: row.try_get(10)?,
                        reason: row.try_get(11)?,
                        same_as: row.try_get(12)?,
                        comment_id: row.try_get(13)?,
                    }),
                    None => None,
                };
                Ok(DraftRecord {
                    at: text(row.try_get(0)?),
                    draft: row.try_get(1)?,
                    lane: row.try_get(2)?,
                    model: row.try_get(3)?,
                    kind: row.try_get(4)?,
                    path: row.try_get(5)?,
                    line: u32::try_from(line).map_err(|_| StoreError::Corrupt {
                        column: "drafts.line",
                        value: line.to_string(),
                    })?,
                    target: row.try_get(7)?,
                    body: row.try_get(8)?,
                    decision,
                })
            })
            .collect()
    }

    async fn draft_rates(
        &self,
        group: DraftGroup,
        filter: &DraftFilter,
    ) -> Result<Vec<DraftRates>, StoreError> {
        let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
        let (key, columns, by) = match group {
            DraftGroup::Model => ("d.model", "NULL::text, NULL::bigint, NULL::text", "d.model"),
            DraftGroup::Lane => ("d.lane", "NULL::text, NULL::bigint, NULL::text", "d.lane"),
            DraftGroup::Repo => ("r.repo", "NULL::text, NULL::bigint, NULL::text", "r.repo"),
            DraftGroup::Target => (
                "r.repo || ' #' || r.target::text",
                "r.repo, r.target, r.platform",
                "r.repo, r.target, r.platform",
            ),
        };
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT {key}, {columns}, COUNT(*), {VERDICT_SUMS}
                     FROM drafts d JOIN runs r ON r.id = d.run_id
                     WHERE {DRAFT_FILTER}
                     GROUP BY {by} ORDER BY COUNT(*) DESC, 1"
                ),
                &[&filter.model, &filter.lane, &filter.repo, &since, &until],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let count = |i: usize| -> Result<u64, StoreError> {
                    let n: i64 = row.try_get(i)?;
                    Ok(u64::try_from(n).unwrap_or_default())
                };
                let target: Option<i64> = row.try_get(2)?;
                Ok(DraftRates {
                    key: row.try_get(0)?,
                    repo: row.try_get(1)?,
                    target: target.and_then(|t| u64::try_from(t).ok()),
                    platform: row
                        .try_get::<_, Option<String>>(3)?
                        .as_deref()
                        .and_then(platform_parse),

                    drafts: count(4)?,
                    confirmed: count(5)?,
                    rejected: count(6)?,
                    same_as: count(7)?,
                    unchecked: count(8)?,
                    not_checked: count(9)?,
                    cancelled: count(10)?,
                    failed: count(11)?,
                    waiting: count(12)?,
                })
            })
            .collect()
    }

    async fn daily_draft_rates(
        &self,
        group: DraftGroup,
        filter: &DraftFilter,
    ) -> Result<Vec<DayRates>, StoreError> {
        let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
        let key = match group {
            DraftGroup::Model => "d.model",
            DraftGroup::Lane => "d.lane",
            DraftGroup::Repo => "r.repo",
            DraftGroup::Target => "r.repo || ' #' || r.target::text",
        };
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT {key}, to_char(d.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'),
                            COUNT(*) FILTER (WHERE d.verdict IN ('confirmed', 'rejected', 'same_as')),
                            COUNT(*) FILTER (WHERE d.verdict = 'rejected')
                     FROM drafts d JOIN runs r ON r.id = d.run_id
                     WHERE {DRAFT_FILTER}
                     GROUP BY 1, 2 ORDER BY 1, 2"
                ),
                &[&filter.model, &filter.lane, &filter.repo, &since, &until],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let count = |i: usize| -> Result<u64, StoreError> {
                    let n: i64 = row.try_get(i)?;
                    Ok(u64::try_from(n).unwrap_or_default())
                };
                Ok(DayRates {
                    key: row.try_get(0)?,
                    day: row.try_get(1)?,
                    judged: count(2)?,
                    rejected: count(3)?,
                })
            })
            .collect()
    }

    async fn count_drafts(&self, filter: &DraftFilter) -> Result<u64, StoreError> {
        let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
        let verdict = VerdictFilter::param(filter.verdict);
        let count: i64 = self
            .client()
            .await?
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM drafts d JOIN runs r ON r.id = d.run_id
                     WHERE {DRAFT_FILTER}
                       AND ($6::text IS NULL OR ($6 = 'waiting' AND d.verdict IS NULL) OR d.verdict = $6)"
                ),
                &[&filter.model, &filter.lane, &filter.repo, &since, &until, &verdict],
            )
            .await?
            .try_get(0)?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    async fn list_drafts(
        &self,
        filter: &DraftFilter,
        page: Page,
    ) -> Result<Vec<DraftListing>, StoreError> {
        let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
        let verdict = VerdictFilter::param(filter.verdict);
        let before_at = filter
            .before
            .as_ref()
            .map(|k| parse_time("created_at", &k.created_at))
            .transpose()?;
        let before_id = filter.before.as_ref().map(|k| k.id);
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT d.id, d.run_id, r.repo, r.target, d.created_at, d.draft, d.lane, d.model,
                            d.kind, d.path, d.line, d.target, d.body, d.verdict, d.checker, d.reason,
                            d.same_as, d.comment_id, d.decided_at, r.platform
                     FROM drafts d JOIN runs r ON r.id = d.run_id
                     WHERE {DRAFT_FILTER}
                       AND ($6::text IS NULL OR ($6 = 'waiting' AND d.verdict IS NULL) OR d.verdict = $6)
                       AND ($7::timestamptz IS NULL OR d.created_at < $7
                            OR (d.created_at = $7 AND d.id < $8::bigint))
                     ORDER BY d.created_at DESC, d.id DESC LIMIT $9 OFFSET $10"
                ),
                &[
                    &filter.model,
                    &filter.lane,
                    &filter.repo,
                    &since,
                    &until,
                    &verdict,
                    &before_at,
                    &before_id,
                    &i64::from(page.limit()),
                    &i64::from(page.offset()),
                ],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let line: i64 = row.try_get(10)?;
                let verdict: Option<String> = row.try_get(13)?;
                let decision = match verdict {
                    Some(verdict) => Some(DraftDecision {
                        at: row
                            .try_get::<_, Option<OffsetDateTime>>(18)?
                            .map(text)
                            .unwrap_or_default(),
                        verdict: draft_verdict(&verdict)?,
                        checker: row.try_get(14)?,
                        reason: row.try_get(15)?,
                        same_as: row.try_get(16)?,
                        comment_id: row.try_get(17)?,
                    }),
                    None => None,
                };
                let target: i64 = row.try_get(3)?;
                let platform: String = row.try_get(19)?;
                Ok(DraftListing {
                    id: row.try_get(0)?,
                    run_id: row.try_get(1)?,
                    repo: row.try_get(2)?,
                    target: to_u64("runs.target", target)?,
                    platform: platform_parse(&platform).ok_or_else(|| StoreError::Corrupt {
                        column: "runs.platform",
                        value: platform.clone(),
                    })?,
                    draft: DraftRecord {
                        at: text(row.try_get(4)?),
                        draft: row.try_get(5)?,
                        lane: row.try_get(6)?,
                        model: row.try_get(7)?,
                        kind: row.try_get(8)?,
                        path: row.try_get(9)?,
                        line: u32::try_from(line).map_err(|_| StoreError::Corrupt {
                            column: "drafts.line",
                            value: line.to_string(),
                        })?,
                        target: row.try_get(11)?,
                        body: row.try_get(12)?,
                        decision,
                    },
                })
            })
            .collect()
    }
    async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError> {
        let at = if transcript.at.is_empty() {
            now()
        } else {
            parse_time("transcripts.recorded_at", &transcript.at)?
        };
        self.client()
            .await?
            .execute(
                "INSERT INTO transcripts (run_id, session, model, stop, turns, bytes, body, recorded_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &run.as_str(),
                    &transcript.session,
                    &transcript.model,
                    &transcript.stop,
                    &i64::from(transcript.turns),
                    &i64::try_from(transcript.bytes).unwrap_or(i64::MAX),
                    &transcript.body,
                    &at,
                ],
            )
            .await?;
        Ok(())
    }

    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT recorded_at, session, model, stop, turns, bytes FROM transcripts WHERE run_id = $1 ORDER BY id",
                &[&run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(TranscriptSummary {
                    at: text(row.try_get(0)?),
                    session: row.try_get(1)?,
                    model: row.try_get(2)?,
                    stop: row.try_get(3)?,
                    turns: turns(row.try_get(4)?)?,
                    bytes: u64::try_from(row.try_get::<_, i64>(5)?).unwrap_or_default(),
                })
            })
            .collect()
    }

    async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT recorded_at, session, model, stop, turns, bytes, body FROM transcripts WHERE run_id = $1 AND session = $2 ORDER BY id DESC LIMIT 1",
                &[&run.as_str(), &session],
            )
            .await?;
        rows.first()
            .map(|row| {
                Ok(TranscriptRecord {
                    at: text(row.try_get(0)?),
                    session: row.try_get(1)?,
                    model: row.try_get(2)?,
                    stop: row.try_get(3)?,
                    turns: turns(row.try_get(4)?)?,
                    bytes: u64::try_from(row.try_get::<_, i64>(5)?).unwrap_or_default(),
                    body: row.try_get(6)?,
                })
            })
            .transpose()
    }

    async fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError> {
        let at = if call.at.is_empty() {
            now()
        } else {
            parse_time("tool_calls.at", &call.at)?
        };
        let number = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        self.client()
            .await?
            .execute(
                "INSERT INTO tool_calls (run_id, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms, at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &run.as_str(),
                    &call.session,
                    &call.model,
                    &i64::from(call.turn),
                    &call.tool,
                    &call.origin,
                    &call.outcome,
                    &call.arguments,
                    &number(call.arguments_len),
                    &number(call.result_chars),
                    &number(call.elapsed_ms),
                    &at,
                ],
            )
            .await?;
        Ok(())
    }

    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT at, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms FROM tool_calls WHERE run_id = $1 ORDER BY id",
                &[&run.as_str()],
            )
            .await?;
        let number = |n: i64| u64::try_from(n).unwrap_or_default();
        rows.iter()
            .map(|row| {
                let turn: i64 = row.try_get(3)?;
                Ok(ToolCallRecord {
                    at: text(row.try_get(0)?),
                    session: row.try_get(1)?,
                    model: row.try_get(2)?,
                    turn: u32::try_from(turn).map_err(|_| StoreError::Corrupt {
                        column: "tool_calls.turn",
                        value: turn.to_string(),
                    })?,
                    tool: row.try_get(4)?,
                    origin: row.try_get(5)?,
                    outcome: row.try_get(6)?,
                    arguments: row.try_get(7)?,
                    arguments_len: number(row.try_get(8)?),
                    result_chars: number(row.try_get(9)?),
                    elapsed_ms: number(row.try_get(10)?),
                })
            })
            .collect()
    }

    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
        let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT c.model, c.session, c.tool, c.outcome, COUNT(*),
                            COALESCE(SUM(c.elapsed_ms), 0)::BIGINT
                     FROM tool_calls c WHERE {CALL_FILTER}
                     GROUP BY c.model, c.session, c.tool, c.outcome"
                ),
                &[
                    &filter.tool,
                    &filter.model,
                    &filter.session_kind,
                    &since,
                    &until,
                ],
            )
            .await?;
        let number = |n: i64| u64::try_from(n).unwrap_or_default();
        let rows = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                    number(row.try_get(4)?),
                    number(row.try_get(5)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(ToolUsage::across_runs(rows))
    }

    async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError> {
        let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
        let outcome = OutcomeFilter::param(filter.outcome.as_ref());
        let before_at = filter
            .before
            .as_ref()
            .map(|k| parse_time("at", &k.at))
            .transpose()?;
        let before_id = filter.before.as_ref().map(|k| k.id);
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT c.id, c.run_id, r.repo, r.target, r.platform, c.at, c.session, c.model,
                            c.turn, c.tool, c.origin, c.outcome, c.arguments, c.arguments_len,
                            c.result_chars, c.elapsed_ms,
                            EXISTS (SELECT 1 FROM transcripts t
                                    WHERE t.run_id = c.run_id AND t.session = c.session),
                            r.kind
                     FROM tool_calls c JOIN runs r ON r.id = c.run_id
                     WHERE {CALL_FILTER}
                       AND ($6::text IS NULL OR ($6 = 'problems' AND c.outcome <> 'ok') OR c.outcome = $6)
                       AND ($7::timestamptz IS NULL OR c.at < $7 OR (c.at = $7 AND c.id < $8::bigint))
                     ORDER BY c.at DESC, c.id DESC LIMIT $9 OFFSET $10"
                ),
                &[
                    &filter.tool,
                    &filter.model,
                    &filter.session_kind,
                    &since,
                    &until,
                    &outcome,
                    &before_at,
                    &before_id,
                    &i64::from(page.limit()),
                    &i64::from(page.offset()),
                ],
            )
            .await?;
        let number = |n: i64| u64::try_from(n).unwrap_or_default();
        rows.iter()
            .map(|row| {
                let target: i64 = row.try_get(3)?;
                let platform: String = row.try_get(4)?;
                let turn: i64 = row.try_get(8)?;
                let kind: String = row.try_get(17)?;
                Ok(ToolCallListing {
                    id: row.try_get(0)?,
                    run_id: row.try_get(1)?,
                    repo: row.try_get(2)?,
                    target: to_u64("runs.target", target)?,
                    platform: platform_parse(&platform).ok_or_else(|| StoreError::Corrupt {
                        column: "runs.platform",
                        value: platform.clone(),
                    })?,
                    kind: kind_parse(&kind).ok_or_else(|| StoreError::Corrupt {
                        column: "runs.kind",
                        value: kind.clone(),
                    })?,
                    call: ToolCallRecord {
                        at: text(row.try_get(5)?),
                        session: row.try_get(6)?,
                        model: row.try_get(7)?,
                        turn: u32::try_from(turn).map_err(|_| StoreError::Corrupt {
                            column: "tool_calls.turn",
                            value: turn.to_string(),
                        })?,
                        tool: row.try_get(9)?,
                        origin: row.try_get(10)?,
                        outcome: row.try_get(11)?,
                        arguments: row.try_get(12)?,
                        arguments_len: number(row.try_get(13)?),
                        result_chars: number(row.try_get(14)?),
                        elapsed_ms: number(row.try_get(15)?),
                    },
                    transcript_kept: row.try_get(16)?,
                })
            })
            .collect()
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT created_at, lane, path, line, comment_id, action FROM findings WHERE run_id = $1 ORDER BY id",
                &[&run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let line: i64 = row.try_get(3)?;
                Ok(FindingRecord {
                    at: text(row.try_get(0)?),
                    lane: row.try_get(1)?,
                    path: row.try_get(2)?,
                    line: u32::try_from(line).map_err(|_| StoreError::Corrupt {
                        column: "findings.line",
                        value: line.to_string(),
                    })?,
                    comment_id: row.try_get(4)?,
                    action: row.try_get(5)?,
                })
            })
            .collect()
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO events (run_id, at, level, message) VALUES ($1, $2, $3, $4)",
                &[&run.as_str(), &now(), &level, &message],
            )
            .await?;
        Ok(())
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT at, level, message FROM events WHERE run_id = $1 ORDER BY id",
                &[&run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(EventRecord {
                    at: text(row.try_get(0)?),
                    level: row.try_get(1)?,
                    message: row.try_get(2)?,
                })
            })
            .collect()
    }

    async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO requests (run_id, at, source) VALUES ($1, $2, $3)",
                &[&run.as_str(), &now(), &source],
            )
            .await?;
        Ok(())
    }

    async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
        let payload = event
            .payload
            .as_deref()
            .filter(|p| p.len() <= MAX_PAYLOAD_BYTES);
        let target = event
            .target
            .map(|t| to_i64("inbound_events.target", t))
            .transpose()?;
        self.client()
            .await?
            .execute(
                "INSERT INTO inbound_events (id, received_at, source, kind, repo, target, payload, requester) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &event.id.as_str(),
                    &parse_time("inbound_events.received_at", &event.received_at)?,
                    &event.source,
                    &event.kind,
                    &event.repo,
                    &target,
                    &payload,
                    &event.requester,
                ],
            )
            .await?;
        Ok(())
    }

    async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO event_outcomes (event_id, listener, outcome, detail, run_id, at) VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &outcome.event_id.as_str(),
                    &outcome.listener,
                    &outcome.outcome,
                    &outcome.detail,
                    &outcome.run_id,
                    &parse_time("event_outcomes.at", &outcome.at)?,
                ],
            )
            .await?;
        Ok(())
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        self.client()
            .await?
            .query_opt(
                "SELECT id, received_at, source, kind, repo, target, payload, requester FROM inbound_events WHERE id = $1",
                &[&id.as_str()],
            )
            .await?
            .map(|row| inbound_event(&row))
            .transpose()
    }

    async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        let client = self.client().await?;
        let mut facets = EventFacets::default();
        for (column, values) in [("source", &mut facets.sources), ("kind", &mut facets.kinds)] {
            *values = client
                .query(
                    &format!("SELECT DISTINCT {column} FROM inbound_events ORDER BY 1"),
                    &[],
                )
                .await?
                .iter()
                .map(|row| row.try_get(0))
                .collect::<Result<Vec<String>, _>>()?;
        }
        Ok(facets)
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT listener, outcome, detail, run_id, at FROM event_outcomes WHERE event_id = $1 ORDER BY id",
                &[&id.as_str()],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(OutcomeRecord {
                    event_id: id.clone(),
                    listener: row.try_get(0)?,
                    outcome: row.try_get(1)?,
                    detail: row.try_get(2)?,
                    run_id: row.try_get(3)?,
                    at: text(row.try_get(4)?),
                })
            })
            .collect()
    }

    async fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await?;
        let outcomes = transaction
            .execute(
                "DELETE FROM event_outcomes WHERE event_id IN
                 (SELECT id FROM inbound_events WHERE received_at < $1)",
                &[&older_than],
            )
            .await?;
        let events = transaction
            .execute(
                "DELETE FROM inbound_events WHERE received_at < $1",
                &[&older_than],
            )
            .await?;
        let transcripts = transaction
            .execute(
                "DELETE FROM transcripts WHERE recorded_at < $1",
                &[&older_than],
            )
            .await?;
        transaction.commit().await?;
        Ok(PruneCounts {
            events,
            outcomes,
            transcripts,
        })
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        // One query, unlike SQLite's lookup per id: here each lookup is a round trip.
        let rows = self
            .client()
            .await?
            .query(
                "SELECT e.id, e.received_at, e.source, e.kind, e.repo, e.target, e.payload, e.requester
                 FROM inbound_events e
                 WHERE e.id IN (SELECT o.event_id FROM event_outcomes o WHERE o.run_id = $1)
                 ORDER BY e.received_at, e.id",
                &[&run.as_str()],
            )
            .await?;
        rows.iter().map(inbound_event).collect()
    }

    async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let p = RunParams::of(filter)?;
        self.client()
            .await?
            .query(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM runs WHERE {RUN_FILTER}
                     ORDER BY started_at DESC, id DESC LIMIT $10 OFFSET $11"
                ),
                &[
                    &p.kind,
                    &p.status,
                    &p.platform,
                    &filter.repo,
                    &p.target,
                    &p.since,
                    &p.until,
                    &p.before_at,
                    &p.before_id,
                    &i64::from(page.limit()),
                    &i64::from(page.offset()),
                ],
            )
            .await?
            .iter()
            .map(|row| raw_run(row)?.into_record())
            .collect()
    }

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        let p = RunParams::of(filter)?;
        let count: i64 = self
            .client()
            .await?
            .query_one(
                &format!("SELECT COUNT(*) FROM runs WHERE {RUN_FILTER}"),
                &[
                    &p.kind,
                    &p.status,
                    &p.platform,
                    &filter.repo,
                    &p.target,
                    &p.since,
                    &p.until,
                    &p.before_at,
                    &p.before_id,
                ],
            )
            .await?
            .try_get(0)?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        let before_at = filter
            .before
            .as_ref()
            .map(|k| parse_time("received_at", &k.received_at))
            .transpose()?;
        let before_id = filter.before.as_ref().map(|k| k.id.as_str());
        let client = self.client().await?;
        let events = client
            .query(
                "SELECT id, received_at, source, kind, repo, target, payload, requester FROM inbound_events
                 WHERE ($1::text IS NULL OR source = $1) AND ($2::text IS NULL OR kind = $2)
                   AND ($3::text IS NULL OR repo = $3)
                   AND ($4::timestamptz IS NULL OR received_at < $4
                        OR (received_at = $4 AND id < $5::text))
                 ORDER BY received_at DESC, id DESC LIMIT $6 OFFSET $7",
                &[
                    &filter.source,
                    &filter.kind,
                    &filter.repo,
                    &before_at,
                    &before_id,
                    &i64::from(page.limit()),
                    &i64::from(page.offset()),
                ],
            )
            .await?
            .iter()
            .map(inbound_event)
            .collect::<Result<Vec<_>, _>>()?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        // One query for every outcome of the page, not one per event.
        let ids: Vec<String> = events.iter().map(|e| e.id.as_str().to_owned()).collect();
        let rows = client
            .query(
                "SELECT event_id, listener, outcome, detail, run_id, at FROM event_outcomes
                 WHERE event_id = ANY($1) ORDER BY id",
                &[&ids],
            )
            .await?;
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in &rows {
            outcomes.push(OutcomeRow {
                event_id: row.try_get(0)?,
                listener: row.try_get(1)?,
                outcome: row.try_get(2)?,
                detail: row.try_get(3)?,
                run_id: row.try_get(4)?,
                at: text(row.try_get(5)?),
            });
        }
        Ok(attach_outcomes(events, &outcomes))
    }
}
