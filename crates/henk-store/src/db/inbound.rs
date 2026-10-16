//! What Henk was told, and what each listener did about it.

use henk_domain::run::{EventId, RunId};
use sea_orm::sea_query::{Condition, Expr, ExprTrait, Order, Query, SelectStatement};
use sea_orm::{ConnectionTrait, QueryResult, TransactionTrait};
use time::OffsetDateTime;

use super::dialect::Dialect;
use super::{Store, at, get};
use crate::types::{
    EventFacets, EventFilter, EventWithOutcomes, InboundEvent, MAX_PAYLOAD_BYTES, OutcomeRecord,
    OutcomeRow, Page, PruneCounts, StoreError, attach_outcomes, event_id, to_i64, to_u64,
};

impl Store {
    /// Records an event Henk received. A payload over [`MAX_PAYLOAD_BYTES`] is
    /// left out; the event still is recorded.
    pub(crate) async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
        let received = self
            .dialect()
            .await?
            .time_text("inbound_events.received_at", &event.received_at)?;
        let payload = event
            .payload
            .as_deref()
            .filter(|payload| payload.len() <= MAX_PAYLOAD_BYTES);
        let target = event
            .target
            .map(|target| to_i64("inbound_events.target", target))
            .transpose()?;
        let mut insert = Query::insert();
        insert
            .into_table("inbound_events")
            .columns([
                "id",
                "received_at",
                "source",
                "kind",
                "repo",
                "target",
                "payload",
                "requester",
            ])
            .values([
                event.id.as_str().into(),
                received.into(),
                event.source.as_str().into(),
                event.kind.as_str().into(),
                event.repo.clone().into(),
                target.into(),
                payload.map(str::to_owned).into(),
                event.requester.clone().into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "inbound_events",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    /// Records what a listener did with an event.
    pub(crate) async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
        let at = self
            .dialect()
            .await?
            .time_text("event_outcomes.at", &outcome.at)?;
        let mut insert = Query::insert();
        insert
            .into_table("event_outcomes")
            .columns(["event_id", "listener", "outcome", "detail", "run_id", "at"])
            .values([
                outcome.event_id.as_str().into(),
                outcome.listener.as_str().into(),
                outcome.outcome.as_str().into(),
                outcome.detail.as_str().into(),
                outcome.run_id.clone().into(),
                at.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "event_outcomes",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn inbound_event(
        &self,
        id: &EventId,
    ) -> Result<Option<InboundEvent>, StoreError> {
        let mut query = inbound_select();
        query.and_where(Expr::col("id").eq(id.as_str()));
        self.first(&query)
            .await?
            .map(|row| inbound_event(&row))
            .transpose()
    }

    /// The sources and kinds events have come with, for the filters of the
    /// events page.
    pub(crate) async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        let mut facets = EventFacets::default();
        for (column, values) in [("source", &mut facets.sources), ("kind", &mut facets.kinds)] {
            let mut query = Query::select();
            query
                .distinct()
                .column(column)
                .from("inbound_events")
                .order_by(column, Order::Asc);
            *values = self
                .all(&query)
                .await?
                .iter()
                .map(|row| get(row, 0))
                .collect::<Result<_, _>>()?;
        }
        Ok(facets)
    }

    /// What the listeners did with an event, in the order they said.
    pub(crate) async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["listener", "outcome", "detail", "run_id"])
            .column("at")
            .from("event_outcomes")
            .and_where(Expr::col("event_id").eq(id.as_str()))
            .order_by_expr(self.dialect().await?.recorded_order(), Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(OutcomeRecord {
                    event_id: id.clone(),
                    listener: get(row, 0)?,
                    outcome: get(row, 1)?,
                    detail: get(row, 2)?,
                    run_id: get(row, 3)?,
                    at: at(row, 4)?,
                })
            })
            .collect()
    }

    /// The events a listener answered with `run`, oldest first as instants.
    pub(crate) async fn inbound_events_for_run(
        &self,
        run: &RunId,
    ) -> Result<Vec<InboundEvent>, StoreError> {
        let dialect = self.dialect().await?;
        let mut outcomes = Query::select();
        outcomes
            .column("event_id")
            .from("event_outcomes")
            .and_where(Expr::col("run_id").eq(run.as_str()));
        let mut query = inbound_select();
        query
            .and_where(Expr::col("id").in_subquery(outcomes))
            .order_by_expr(dialect.instant("received_at"), Order::Asc)
            .order_by("id", Order::Asc);
        self.all(&query).await?.iter().map(inbound_event).collect()
    }

    /// Events newest first as instants, with what the listeners did, for the
    /// events page: filtered, paged by offset or by the key of the last one
    /// seen.
    pub(crate) async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        let dialect = self.dialect().await?;
        let query = list_inbound_events_query(dialect, filter, page)?;
        let events = self
            .all(&query)
            .await?
            .iter()
            .map(inbound_event)
            .collect::<Result<Vec<_>, _>>()?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        // One query for every outcome of the page, not one per event.
        let mut query = Query::select();
        query
            .columns(["event_id", "listener", "outcome", "detail", "run_id"])
            .column("at")
            .from("event_outcomes")
            .and_where(Expr::col("event_id").is_in(events.iter().map(|event| event.id.as_str())))
            .order_by_expr(dialect.recorded_order(), Order::Asc);
        let outcomes = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(OutcomeRow {
                    event_id: get(row, 0)?,
                    listener: get(row, 1)?,
                    outcome: get(row, 2)?,
                    detail: get(row, 3)?,
                    run_id: get(row, 4)?,
                    at: at(row, 5)?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(attach_outcomes(events, &outcomes))
    }

    /// Deletes the events received before `older_than`, what was done about
    /// them, and the transcripts recorded before it: all or none.
    pub(crate) async fn prune_events(
        &self,
        older_than: OffsetDateTime,
    ) -> Result<PruneCounts, StoreError> {
        let cutoff = self.dialect().await?.time(older_than);
        let old_events = Query::select()
            .column("id")
            .from("inbound_events")
            .and_where(Expr::col("received_at").lt(cutoff.clone()))
            .take();
        let outcomes = Query::delete()
            .from_table("event_outcomes")
            .and_where(Expr::col("event_id").in_subquery(old_events))
            .take();
        let events = Query::delete()
            .from_table("inbound_events")
            .and_where(Expr::col("received_at").lt(cutoff.clone()))
            .take();
        let transcripts = Query::delete()
            .from_table("transcripts")
            .and_where(Expr::col("recorded_at").lt(cutoff))
            .take();
        let transaction = self.connection().await?.begin().await?;
        let counts = PruneCounts {
            outcomes: transaction.execute(&outcomes).await?.rows_affected(),
            events: transaction.execute(&events).await?.rows_affected(),
            transcripts: transaction.execute(&transcripts).await?.rows_affected(),
        };
        transaction.commit().await?;
        Ok(counts)
    }
}

/// The columns [`inbound_event`] reads.
fn inbound_select() -> SelectStatement {
    let mut query = Query::select();
    query
        .columns([
            "id",
            "received_at",
            "source",
            "kind",
            "repo",
            "target",
            "payload",
            "requester",
        ])
        .from("inbound_events");
    query
}

fn inbound_event(row: &QueryResult) -> Result<InboundEvent, StoreError> {
    let target: Option<i64> = get(row, 5)?;
    Ok(InboundEvent {
        id: event_id(get(row, 0)?)?,
        received_at: at(row, 1)?,
        source: get(row, 2)?,
        kind: get(row, 3)?,
        repo: get(row, 4)?,
        target: target
            .map(|target| to_u64("inbound_events.target", target))
            .transpose()?,
        payload: get(row, 6)?,
        requester: get(row, 7)?,
    })
}

/// The page of events a listing asks for: newest first by when they were
/// received as an instant, not as text (#216), then by id.
pub(super) fn list_inbound_events_query(
    dialect: Dialect,
    filter: &EventFilter,
    page: Page,
) -> Result<SelectStatement, StoreError> {
    let mut all = Condition::all();
    if let Some(source) = &filter.source {
        all = all.add(Expr::col("source").eq(source.as_str()));
    }
    if let Some(kind) = &filter.kind {
        all = all.add(Expr::col("kind").eq(kind.as_str()));
    }
    if let Some(repo) = &filter.repo {
        all = all.add(Expr::col("repo").eq(repo.as_str()));
    }
    if let Some(key) = &filter.before {
        let at = dialect.instant_value("before", &key.received_at)?;
        all = all.add(
            Condition::any()
                .add(dialect.instant("received_at").lt(at.clone()))
                .add(
                    Condition::all()
                        .add(dialect.instant("received_at").eq(at))
                        .add(Expr::col("id").lt(key.id.as_str())),
                ),
        );
    }
    let mut query = inbound_select();
    query
        .cond_where(all)
        .order_by_expr(dialect.instant("received_at"), Order::Desc)
        .order_by("id", Order::Desc)
        .limit(page.limit().into())
        .offset(page.offset().into());
    Ok(query)
}
