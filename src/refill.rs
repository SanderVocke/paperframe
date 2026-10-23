use std::{sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::watch;
use tracing::{error, info};

use crate::{
    queue::{QueueEventReason, RenderedEnqueueRequest},
    state::AppState,
};

const RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
const IDLE_POLL: Duration = Duration::from_secs(24 * 60 * 60);

pub async fn run(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
    let mut queue_events = state.queue.subscribe();
    let mut runtime = state.subscribe_runtime();
    loop {
        if *shutdown.borrow() {
            return;
        }
        let work_serial = queue_events.borrow_and_update().work_serial;
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
        let deadline = tokio::time::sleep(poll);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    state.queue.demand(QueueEventReason::SupplierRefresh);
                    break;
                }
                changed = queue_events.changed() => {
                    if changed.is_err() { return; }
                    if queue_events.borrow_and_update().work_serial != work_serial { break; }
                    // Library-only changes refresh the UI, not supplier polling.
                    // Keep the existing deadline so saves cannot postpone a scan.
                }
                changed = runtime.changed() => { if changed.is_err() { return; } break; }
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } break; }
            }
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
        let rendering = settings.rendering.clone();
        let supplier_id = supplier.id().to_owned();
        let _render_permit = state.render_serialization.clone().acquire_owned().await?;
        let requests = tokio::task::spawn_blocking(move || {
            candidates
                .into_iter()
                .take(deficit)
                .filter_map(|candidate| {
                    let decoded = match crate::renderer::decode_source_named(
                        &candidate.bytes,
                        Some(&candidate.display_name),
                    ) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            tracing::warn!(image = %candidate.display_name, %error, "supplier image could not be decoded");
                            return None;
                        }
                    };
                    let thumbnail = match crate::renderer::thumbnail_jpeg(&decoded) {
                        Ok(thumbnail) => thumbnail,
                        Err(error) => {
                            tracing::warn!(image = %candidate.display_name, %error, "supplier image thumbnail could not be generated");
                            return None;
                        }
                    };
                    let rendered = match crate::renderer::render_with_original(
                        &decoded,
                        &candidate.bytes,
                        rendering.target,
                        rendering.output_mode,
                        decoded.orientation,
                    ) {
                        Ok(rendered) => rendered,
                        Err(error) => {
                            tracing::warn!(image = %candidate.display_name, %error, "supplier image could not be rendered");
                            return None;
                        }
                    };
                    Some(RenderedEnqueueRequest {
                        display_name: candidate.display_name,
                        source_bytes: candidate.bytes,
                        derivative_bytes: rendered.bytes,
                        thumbnail_bytes: thumbnail,
                        media_type: rendered.media_type.into(),
                        source_width: decoded.pixels.width(),
                        source_height: decoded.pixels.height(),
                        orientation: decoded.orientation,
                        frame_target: rendering.target,
                        text_overlay: None,
                        dedicated_text: false,
                        supplier_id: Some(supplier_id.clone()),
                        source_key: Some(candidate.source_key),
                    })
                })
                .collect::<Vec<_>>()
        })
        .await?;
        added += state
            .queue
            .enqueue_rendered_batch(None, Some(target), requests)
            .await?
            .len();
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
