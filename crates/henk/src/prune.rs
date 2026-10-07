//! Old recordings (#69). While serving, Henk deletes inbound events and
//! their outcomes older than `server.keep_events_days`. Runs are kept: their
//! links are posted on the platforms (§8.6).

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::App;

/// How often a serving process prunes. Recordings are kept for days, so an
/// hour late is nothing.
const PRUNE_EVERY: Duration = Duration::from_hours(1);

/// Prunes now and every [`PRUNE_EVERY`] until `cancel`.
pub fn spawn_pruner(app: Arc<App>, cancel: CancellationToken) -> JoinHandle<()> {
    let keep = Duration::from_hours(24 * u64::from(app.settings.server.keep_events_days));
    tokio::spawn(prune_every(app, cancel, PRUNE_EVERY, keep))
}

/// The pruner's loop: every `every`, deletes events received longer than
/// `keep` ago. The first pass is immediate.
pub(crate) async fn prune_every(
    app: Arc<App>,
    cancel: CancellationToken,
    every: Duration,
    keep: Duration,
) {
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticks.tick() => prune_before(&app, time::OffsetDateTime::now_utc() - keep).await,
        }
    }
}

/// One pass. A failure is logged and the next pass tries again.
async fn prune_before(app: &App, cutoff: time::OffsetDateTime) {
    match app.store.prune_events(cutoff).await {
        Ok(counts) if counts.events > 0 || counts.outcomes > 0 || counts.transcripts > 0 => {
            info!(
                events = counts.events,
                outcomes = counts.outcomes,
                transcripts = counts.transcripts,
                "pruned old events"
            );
        }
        Ok(_) => {}
        Err(error) => warn!(%error, "could not prune old events"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use henk_domain::run::EventId;
    use henk_store::InboundEvent;

    use super::*;
    use crate::config::Config;

    const MINIMAL: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
"#;

    fn event(id: &str, received_at: &str) -> InboundEvent {
        InboundEvent {
            id: EventId::parse(id).unwrap(),
            received_at: received_at.to_owned(),
            source: "api".to_owned(),
            kind: "plan_requested".to_owned(),
            repo: Some("docspec/app".to_owned()),
            target: Some(1),
            payload: None,
            requester: None,
        }
    }

    #[tokio::test]
    async fn old_events_are_pruned_while_serving_and_new_ones_kept() {
        let settings = Config::parse(MINIMAL).unwrap().into_settings().unwrap();
        let app = Arc::new(
            App::build(settings, Some(std::path::Path::new(":memory:")))
                .await
                .unwrap(),
        );
        let (old, new) = (
            EventId::parse("e-old").unwrap(),
            EventId::parse("e-new").unwrap(),
        );
        app.store
            .record_event(&event("e-old", "2026-01-01T00:00:00Z"))
            .await
            .unwrap();
        app.store
            .record_event(&event("e-new", &crate::hooks::now_rfc3339()))
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let pruner = tokio::spawn(prune_every(
            Arc::clone(&app),
            cancel.clone(),
            Duration::from_millis(10),
            Duration::from_hours(24),
        ));
        let mut gone = false;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            gone = app.store.inbound_event(&old).await.unwrap().is_none();
            if gone {
                break;
            }
        }
        cancel.cancel();
        pruner.await.unwrap();

        assert!(gone, "the old event is pruned");
        assert!(
            app.store.inbound_event(&new).await.unwrap().is_some(),
            "a recent event is kept"
        );
    }
}
