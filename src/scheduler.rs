use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use tokio::sync::watch;
use tracing::error;

use crate::{config::AdvancementSettings, state::AppState};

pub async fn run(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
    let mut runtime = state.subscribe_runtime();
    loop {
        if *shutdown.borrow() {
            return;
        }
        let settings = state.settings.snapshot().await.settings;
        let AdvancementSettings::Scheduled { timezone, times } = settings.advancement else {
            tokio::select! {
                changed = runtime.changed() => { if changed.is_err() { return; } }
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } }
            }
            continue;
        };
        let now = Utc::now();
        let next = match next_scheduled(&timezone, &times, now) {
            Ok(next) => next,
            Err(error) => {
                error!(%error, "failed to calculate next advancement");
                now + chrono::Duration::minutes(5)
            }
        };
        let wait = (next - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {
                if let Err(error) = state.queue.advance().await { error!(%error, "scheduled queue advancement failed"); }
            }
            changed = runtime.changed() => { if changed.is_err() { return; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } }
        }
    }
}

pub fn next_scheduled(
    timezone: &str,
    times: &[String],
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let timezone: chrono_tz::Tz = timezone
        .parse()
        .with_context(|| format!("invalid timezone: {timezone}"))?;
    let parsed = times
        .iter()
        .map(|time| {
            NaiveTime::parse_from_str(time, "%H:%M")
                .with_context(|| format!("invalid time: {time}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let local_date = now.with_timezone(&timezone).date_naive();
    let mut candidates = Vec::new();
    for day in 0..=1 {
        let date = local_date
            .checked_add_signed(chrono::Duration::days(day))
            .context("schedule date overflow")?;
        for time in &parsed {
            let candidate = resolve_local(timezone, date, *time)?;
            if candidate > now {
                candidates.push(candidate);
            }
        }
    }
    candidates
        .into_iter()
        .min()
        .ok_or_else(|| anyhow!("could not find next scheduled time"))
}

fn resolve_local(
    timezone: chrono_tz::Tz,
    date: NaiveDate,
    time: NaiveTime,
) -> Result<DateTime<Utc>> {
    let base = NaiveDateTime::new(date, time);
    for minutes in 0..=180 {
        let local = base
            .checked_add_signed(chrono::Duration::minutes(minutes))
            .context("local schedule overflow")?;
        match timezone.from_local_datetime(&local) {
            LocalResult::Single(value) => return Ok(value.with_timezone(&Utc)),
            LocalResult::Ambiguous(first, second) => {
                return Ok(std::cmp::min(first, second).with_timezone(&Utc));
            }
            LocalResult::None => {}
        }
    }
    Err(anyhow!("scheduled local time cannot be resolved"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_next_of_multiple_times_without_replay() {
        let now = DateTime::parse_from_rfc3339("2026-08-20T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = next_scheduled("UTC", &["08:00".into(), "13:00".into()], now).unwrap();
        assert_eq!(
            next,
            DateTime::parse_from_rfc3339("2026-08-20T13:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn resolves_dst_gap() {
        let now = DateTime::parse_from_rfc3339("2026-03-28T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = next_scheduled("Europe/Amsterdam", &["02:30".into()], now).unwrap();
        assert_eq!(
            next,
            DateTime::parse_from_rfc3339("2026-03-29T01:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }
}
