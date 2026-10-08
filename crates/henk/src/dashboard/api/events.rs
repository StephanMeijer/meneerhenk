//! Inbound events: webhooks and requests, with what each listener did.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use henk_domain::run::EventId;
use henk_store::{EventFilter, EventKey};
use serde::Deserialize;

use super::types::{EventDetail, EventFacets, EventItem, EventSummary, ListenerOutcome, Page};
use super::{ApiError, ApiQuery, ApiResult, cursor, limit, read_cursor};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;

/// What `GET /events` takes. Every filter is optional.
#[derive(Debug, Default, Deserialize)]
pub struct EventQuery {
    source: Option<String>,
    kind: Option<String>,
    repo: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

fn wanted(value: Option<&String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// `GET /events`: inbound events newest first, by filter, a page at a time.
pub async fn list(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<EventQuery>,
) -> ApiResult<Page<EventItem>> {
    let before = query
        .cursor
        .as_deref()
        .map(read_cursor)
        .transpose()?
        .map(|(received_at, id)| EventKey { received_at, id });
    let filter = EventFilter {
        source: wanted(query.source.as_ref()),
        kind: wanted(query.kind.as_ref()),
        repo: wanted(query.repo.as_ref()),
        before,
    };
    let limit = limit(query.limit);
    let listed = dashboard
        .app
        .store
        .list_inbound_events(&filter, henk_store::Page::new(limit, 0))
        .await?;
    let next = (u32::try_from(listed.len()).ok() == Some(limit))
        .then(|| {
            listed
                .last()
                .map(|e| cursor(&e.event.received_at, e.event.id.as_str()))
        })
        .flatten();
    Ok(Json(Page {
        items: listed.iter().map(EventItem::from).collect(),
        next,
    }))
}

/// `GET /events/facets`: the sources and kinds recorded, for the filters.
pub async fn facets(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
) -> ApiResult<EventFacets> {
    let facets = dashboard.app.store.event_facets().await?;
    Ok(Json(EventFacets {
        sources: facets.sources,
        kinds: facets.kinds,
    }))
}

/// `GET /events/{id}`: one event with its payload and outcomes.
pub async fn detail(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path(id): Path<String>,
) -> ApiResult<EventDetail> {
    let id = EventId::parse(id).map_err(|_| ApiError::bad_request("That is not an event id."))?;
    let store = &dashboard.app.store;
    let event = store
        .inbound_event(&id)
        .await?
        .ok_or_else(|| ApiError::not_found("No such event."))?;
    let outcomes = store.outcomes(&id).await?;
    Ok(Json(EventDetail {
        event: EventSummary::from(&event),
        payload: event.payload,
        outcomes: outcomes.iter().map(ListenerOutcome::from).collect(),
    }))
}
