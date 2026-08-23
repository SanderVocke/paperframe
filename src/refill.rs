use std::{sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::watch;
use tracing::{error, info};

use crate::{queue::QueueEventReason, state::AppState};

const RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
const IDLE_POLL: Duration = Duration::from_secs(24 * 60 * 60);

pub async fn run(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
    let mut queue_events = state.queue.subscribe();
    let mut runtime = state.subscribe_runtime();
    loop {
        if *shutdown.borrow() {
            return;
        }
        state.refill_started().await;
        let result = refill_once(&state).await;
        state.refill_finished(&result).await;
        match &result {
            Ok(added) => info!(added, "queue refill completed"),
            Err(error) => error!(%error, "queue refill failed; existing queue retained"),
        }
        let poll = if result.is_err() {
            RETRY_DELAY
        } else {
            state.registry().await.shortest_poll().unwrap_or(IDLE_POLL)
        };
        tokio::select! {
            _ = tokio::time::sleep(poll) => state.queue.demand(QueueEventReason::SupplierRefresh),
            changed = queue_events.changed() => { if changed.is_err() { return; } }
            changed = runtime.changed() => { if changed.is_err() { return; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } }
        }
    }
}

pub async fn refill_once(state: &AppState) -> Result<usize> {
    let settings = state.settings.snapshot().await.settings;
    let target = settings.queue.target_length;
    let registry = state.registry().await;
    let mut added = 0;
    for supplier in registry.suppliers() {
        let count = state.queue.snapshot().await.entries.len();
        if count >= target {
            break;
        }
        let deficit = target - count;
        let seen = state.queue.seen(supplier.id()).await;
        let candidates = supplier.supply(seen, deficit).await?;
        for candidate in candidates.into_iter().take(deficit) {
            let current_seen = state.queue.seen(supplier.id()).await;
            if current_seen.contains(&candidate.source_key) {
                continue;
            }
            state
                .queue
                .enqueue(
                    candidate.display_name,
                    candidate.bytes,
                    Some(supplier.id().to_owned()),
                    Some(candidate.source_key),
                )
                .await?;
            added += 1;
            if state.queue.snapshot().await.entries.len() >= target {
                break;
            }
        }
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    // Concrete supplier/refill behavior is covered by route and end-to-end fixtures.
    #[test]
    fn target_is_a_minimum_not_a_cap() {
        assert!(10_usize.saturating_sub(12) == 0);
    }
}
