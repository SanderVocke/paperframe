use std::{future::Future, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use tokio::sync::watch;
use tracing::{error, info};

use crate::{
    config::Settings,
    selection::{self, SelectionResult},
    state::AppState,
};

const RETRY_DELAY: Duration = Duration::from_secs(5 * 60);

pub async fn run(state: Arc<AppState>, shutdown: watch::Receiver<bool>) {
    run_with_selector(
        state,
        shutdown,
        RETRY_DELAY,
        |settings, cache_dir, now| async move {
            selection::select_and_cache(settings, &cache_dir, now).await
        },
    )
    .await;
}

async fn run_with_selector<F, Fut>(
    state: Arc<AppState>,
    mut shutdown: watch::Receiver<bool>,
    retry_delay: Duration,
    selector: F,
) where
    F: Fn(Settings, PathBuf, DateTime<Utc>) -> Fut,
    Fut: Future<Output = Result<SelectionResult>>,
{
    let mut triggers = state.subscribe_to_selection_triggers();
    loop {
        if *shutdown.borrow() {
            return;
        }

        let snapshot = state.settings.snapshot().await;
        let attempted_at = Utc::now();
        state
            .selection_started(snapshot.generation, attempted_at)
            .await;
        let selection = selector(
            snapshot.settings.clone(),
            state.cache.root().to_path_buf(),
            attempted_at,
        );
        tokio::pin!(selection);
        let result = tokio::select! {
            result = &mut selection => result,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let cancelled = Err(anyhow!("selection cancelled during shutdown"));
                    state.selection_finished(&cancelled, Utc::now()).await;
                    return;
                }
                continue;
            }
        };
        let completed_at = Utc::now();
        state.selection_finished(&result, completed_at).await;
        match &result {
            Ok(result) => {
                info!(image_id = result.image_id, selection_date = %result.selection_date, cache_changed = result.cache_changed, "daily image selection completed")
            }
            Err(error) => error!(%error, "daily image selection failed; retaining existing cache"),
        }

        let next_attempt = if result.is_ok() {
            next_rollover(&snapshot.settings, completed_at).unwrap_or_else(|error| {
                error!(%error, "failed to calculate next rollover; using retry delay");
                completed_at
                    + chrono::Duration::from_std(retry_delay)
                        .unwrap_or_else(|_| chrono::Duration::minutes(5))
            })
        } else {
            completed_at
                + chrono::Duration::from_std(retry_delay)
                    .unwrap_or_else(|_| chrono::Duration::minutes(5))
        };
        state.set_next_selection_attempt(next_attempt).await;
        let wait = (next_attempt - Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            changed = triggers.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

pub fn next_rollover(settings: &Settings, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let timezone = settings.selection.timezone()?;
    let rollover = settings.selection.rollover_time()?;
    let local_now = now.with_timezone(&timezone);
    let mut date = local_now.date_naive();
    if local_now.time() >= rollover {
        date = date.succ_opt().context("rollover date overflow")?;
    }

    for _ in 0..2 {
        let candidate = resolve_local_rollover(timezone, date, rollover)?;
        if candidate > now {
            return Ok(candidate);
        }
        date = date.succ_opt().context("rollover date overflow")?;
    }
    Err(anyhow!("could not find the next daily rollover"))
}

fn resolve_local_rollover(
    timezone: chrono_tz::Tz,
    date: NaiveDate,
    rollover: chrono::NaiveTime,
) -> Result<DateTime<Utc>> {
    let base = NaiveDateTime::new(date, rollover);
    for minutes in 0..=180 {
        let local = base
            .checked_add_signed(chrono::Duration::minutes(minutes))
            .context("local rollover overflow")?;
        match timezone.from_local_datetime(&local) {
            LocalResult::Single(value) => return Ok(value.with_timezone(&Utc)),
            LocalResult::Ambiguous(first, second) => {
                return Ok(std::cmp::min(first, second).with_timezone(&Utc));
            }
            LocalResult::None => {}
        }
    }
    Err(anyhow!(
        "rollover time cannot be resolved in configured timezone"
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use chrono::{NaiveDate, TimeZone};
    use tempfile::tempdir;
    use tokio::time::timeout;

    use crate::{
        cache::CacheStore,
        config::{
            Config, PathMapping, SelectionMode, SelectionSettings, Settings, SettingsManager,
        },
        state::AppState,
    };

    use super::*;

    fn settings(root: &std::path::Path) -> Settings {
        Settings {
            database_path: root.join("digikam.db"),
            query: "unused".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/library"),
                mounted_root: root.join("photos"),
            }],
            selection: SelectionSettings {
                mode: SelectionMode::First,
                random_seed: String::new(),
                timezone: "UTC".into(),
                rollover_time: "00:00".into(),
            },
            max_candidates: 10,
        }
    }

    fn state(root: &std::path::Path) -> Arc<AppState> {
        std::fs::create_dir_all(root.join("photos")).unwrap();
        let cache = CacheStore::new(&root.join("cache")).unwrap();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            settings_path: root.join("settings.toml"),
            cache_dir: root.join("cache"),
        };
        AppState::new(config, SettingsManager::in_memory(settings(root)), cache)
    }

    fn success() -> SelectionResult {
        SelectionResult {
            image_id: 42,
            selection_date: NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
            cache_changed: true,
            digest: "digest".into(),
            content_length: 10,
        }
    }

    async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn runs_at_startup_and_on_trigger_without_overlap_then_shuts_down() {
        let directory = tempdir().unwrap();
        let state = state(directory.path());
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = {
            let calls = calls.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            let state = state.clone();
            tokio::spawn(run_with_selector(
                state,
                shutdown_rx,
                Duration::from_secs(60),
                move |_, _, _| {
                    let calls = calls.clone();
                    let active = active.clone();
                    let maximum = maximum.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(success())
                    }
                },
            ))
        };

        wait_for_count(&calls, 1).await;
        state.trigger_selection();
        state.trigger_selection();
        wait_for_count(&calls, 2).await;
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(maximum.load(Ordering::SeqCst), 1);
        assert!(state.selector_status().await.last_success.is_some());
    }

    #[tokio::test]
    async fn retries_failure_and_retains_ready_cache() {
        let directory = tempdir().unwrap();
        let state = state(directory.path());
        std::fs::write(state.cache.current_path(), b"old cache").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = {
            let calls = calls.clone();
            let state = state.clone();
            tokio::spawn(run_with_selector(
                state,
                shutdown_rx,
                Duration::from_millis(5),
                move |_, _, _| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            Err(anyhow!("injected database failure"))
                        } else {
                            Ok(success())
                        }
                    }
                },
            ))
        };

        wait_for_count(&calls, 2).await;
        assert_eq!(
            std::fs::read(state.cache.current_path()).unwrap(),
            b"old cache"
        );
        assert!(state.cache.is_ready());
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn calculates_rollovers_across_dst_boundaries() {
        let directory = tempdir().unwrap();
        let mut input = settings(directory.path());
        input.selection.timezone = "Europe/Amsterdam".into();
        input.selection.rollover_time = "02:30".into();

        let spring = Utc.with_ymd_and_hms(2026, 3, 28, 22, 0, 0).unwrap();
        let spring_next = next_rollover(&input, spring).unwrap();
        assert_eq!(
            spring_next,
            Utc.with_ymd_and_hms(2026, 3, 29, 1, 0, 0).unwrap()
        );

        let autumn = Utc.with_ymd_and_hms(2026, 10, 24, 22, 0, 0).unwrap();
        let autumn_next = next_rollover(&input, autumn).unwrap();
        assert_eq!(
            autumn_next,
            Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0).unwrap()
        );
    }
}
