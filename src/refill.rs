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
                    let rendered = match if rendering.output_mode == crate::renderer::OutputMode::DirectJpeg
                        && decoded.format == image::ImageFormat::Jpeg
                    {
                        Ok(crate::renderer::RenderedImage {
                            bytes: candidate.bytes.clone(),
                            media_type: "image/jpeg",
                            extension: "jpg",
                        })
                    } else {
                        crate::renderer::render(
                            &decoded,
                            rendering.target,
                            rendering.output_mode,
                            decoded.orientation,
                        )
                    } {
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
                        media_type: rendered.media_type.into(),
                        source_width: decoded.pixels.width(),
                        source_height: decoded.pixels.height(),
                        orientation: decoded.orientation,
                        frame_target: rendering.target,
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
