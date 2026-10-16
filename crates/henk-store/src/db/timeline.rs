//! What a run leaves behind as it goes: the transcripts of its sessions, the
//! events of its timeline, and who asked for it.

use henk_domain::run::RunId;
use sea_orm::sea_query::{Expr, ExprTrait, Order, Query};

use super::{Store, at, count, get};
use crate::types::{EventRecord, StoreError, TranscriptRecord, TranscriptSummary, to_u32};

impl Store {
    /// Keeps a transcript whole, however large; a session that is recorded
    /// again has its later transcript read as the latest.
    pub(crate) async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError> {
        let recorded = self
            .dialect()
            .await?
            .time_text("transcripts.recorded_at", &transcript.at)?;
        let mut insert = Query::insert();
        insert
            .into_table("transcripts")
            .columns([
                "run_id",
                "session",
                "model",
                "stop",
                "turns",
                "bytes",
                "body",
                "recorded_at",
            ])
            .values([
                run.as_str().into(),
                transcript.session.as_str().into(),
                transcript.model.as_str().into(),
                transcript.stop.as_str().into(),
                i64::from(transcript.turns).into(),
                i64::try_from(transcript.bytes).unwrap_or(i64::MAX).into(),
                transcript.body.as_str().into(),
                recorded.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "transcripts",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    /// The transcripts of a run, without their bodies, in the order they
    /// were recorded.
    pub(crate) async fn transcripts(
        &self,
        run: &RunId,
    ) -> Result<Vec<TranscriptSummary>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["recorded_at", "session", "model", "stop", "turns", "bytes"])
            .from("transcripts")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(TranscriptSummary {
                    at: at(row, 0)?,
                    session: get(row, 1)?,
                    model: get(row, 2)?,
                    stop: get(row, 3)?,
                    turns: to_u32("transcripts.turns", get(row, 4)?)?,
                    bytes: count(row, 5)?,
                })
            })
            .collect()
    }

    /// The latest transcript recorded for `session`, whole.
    pub(crate) async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "recorded_at",
                "session",
                "model",
                "stop",
                "turns",
                "bytes",
                "body",
            ])
            .from("transcripts")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .and_where(Expr::col("session").eq(session))
            .order_by("id", Order::Desc)
            .limit(1);
        self.first(&query)
            .await?
            .map(|row| {
                Ok(TranscriptRecord {
                    at: at(&row, 0)?,
                    session: get(&row, 1)?,
                    model: get(&row, 2)?,
                    stop: get(&row, 3)?,
                    turns: to_u32("transcripts.turns", get(&row, 4)?)?,
                    bytes: count(&row, 5)?,
                    body: get(&row, 6)?,
                })
            })
            .transpose()
    }

    /// Adds a line to the run's timeline, stamped now.
    pub(crate) async fn event(
        &self,
        run: &RunId,
        level: &str,
        message: &str,
    ) -> Result<(), StoreError> {
        let at = self.dialect().await?.now();
        let mut insert = Query::insert();
        insert
            .into_table("events")
            .columns(["run_id", "at", "level", "message"])
            .values([run.as_str().into(), at.into(), level.into(), message.into()])
            .map_err(|error| StoreError::Corrupt {
                column: "events",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["at", "level", "message"])
            .from("events")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(EventRecord {
                    at: at(row, 0)?,
                    level: get(row, 1)?,
                    message: get(row, 2)?,
                })
            })
            .collect()
    }

    /// Records that another request joined the run, from `source`.
    pub(crate) async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        let at = self.dialect().await?.now();
        let mut insert = Query::insert();
        insert
            .into_table("requests")
            .columns(["run_id", "at", "source"])
            .values([run.as_str().into(), at.into(), source.into()])
            .map_err(|error| StoreError::Corrupt {
                column: "requests",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }
}
