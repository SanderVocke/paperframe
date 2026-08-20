use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::{
    cache::{CacheStore, Publication},
    catalog::{self, Candidate},
    config::{SelectionMode, Settings},
    source::SourceStore,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionResult {
    pub image_id: i64,
    pub selection_date: NaiveDate,
    pub cache_changed: bool,
    pub digest: String,
    pub content_length: u64,
}

pub async fn select_and_cache(
    settings: Settings,
    cache_dir: &Path,
    now: DateTime<Utc>,
) -> Result<SelectionResult> {
    settings.validate_basic()?;
    let candidates = catalog::candidates(settings.clone()).await?;
    let cache_dir = cache_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let date = selection_date(&settings, now)?;
        select_from_candidates(&settings, &cache_dir, date, candidates)
    })
    .await
    .context("selection worker stopped")?
}

pub fn selection_date(settings: &Settings, now: DateTime<Utc>) -> Result<NaiveDate> {
    let timezone = settings.selection.timezone()?;
    let rollover = settings.selection.rollover_time()?;
    let local = now.with_timezone(&timezone);
    let mut date = local.date_naive();
    if local.time() < rollover {
        date = date.pred_opt().context("selection date underflow")?;
    }
    Ok(date)
}

fn select_from_candidates(
    settings: &Settings,
    cache_dir: &Path,
    date: NaiveDate,
    candidates: Vec<Candidate>,
) -> Result<SelectionResult> {
    if candidates.is_empty() {
        bail!("DigiKam query returned no candidates");
    }
    let ranked = rank_candidates(settings, date, candidates);
    let sources = SourceStore::new(settings, cache_dir)?;
    let cache = CacheStore::new(cache_dir)?;
    let mut last_error = None;

    for candidate in ranked {
        match publish_candidate(&sources, &cache, &candidate) {
            Ok(publication) => return Ok(result(candidate.image_id, date, publication)),
            Err(error) => {
                warn!(image_id = candidate.image_id, %error, "candidate was skipped");
                last_error = Some(error);
            }
        }
    }

    let error = last_error.context("all candidates were rejected")?;
    Err(error).context("all DigiKam candidates were rejected")
}

fn publish_candidate(
    sources: &SourceStore,
    cache: &CacheStore,
    candidate: &Candidate,
) -> Result<Publication> {
    let mut source = sources.open(candidate)?;
    cache.publish(&mut source)
}

fn result(image_id: i64, date: NaiveDate, publication: Publication) -> SelectionResult {
    SelectionResult {
        image_id,
        selection_date: date,
        cache_changed: publication.changed,
        digest: publication.digest,
        content_length: publication.content_length,
    }
}

fn rank_candidates(
    settings: &Settings,
    date: NaiveDate,
    mut candidates: Vec<Candidate>,
) -> Vec<Candidate> {
    if settings.selection.mode == SelectionMode::DailyRandom {
        let seed = Arc::<str>::from(settings.selection.random_seed.as_str());
        candidates.sort_by_cached_key(|candidate| daily_rank(&seed, date, candidate.image_id));
    }
    candidates
}

fn daily_rank(seed: &str, date: NaiveDate, image_id: i64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update([0]);
    hasher.update(date.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(image_id.to_be_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::SystemTime};

    use chrono::TimeZone;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::config::{PathMapping, SelectionSettings};

    use super::*;

    const JPEG_A: &[u8] = b"\xff\xd8\xfffirst source\xff\xd9";
    const JPEG_B: &[u8] = b"\xff\xd8\xffsecond source\xff\xd9";

    fn settings(root: &Path) -> Settings {
        Settings {
            database_path: root.join("digikam.db"),
            query: "unused".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/original/library"),
                mounted_root: root.join("photos"),
            }],
            selection: SelectionSettings {
                mode: SelectionMode::First,
                random_seed: "frame-a".into(),
                timezone: "Europe/Amsterdam".into(),
                rollover_time: "03:00".into(),
            },
            max_candidates: 100,
        }
    }

    fn candidate(id: i64, name: &str) -> Candidate {
        Candidate {
            image_id: id,
            root_path: PathBuf::from("/original/library"),
            album_path: PathBuf::from("/album"),
            file_name: PathBuf::from(name),
        }
    }

    fn prepare_tree(root: &Path) -> PathBuf {
        let photos = root.join("photos/album");
        let cache = root.join("cache");
        fs::create_dir_all(&photos).unwrap();
        fs::create_dir(&cache).unwrap();
        fs::write(photos.join("a.jpg"), JPEG_A).unwrap();
        fs::write(photos.join("b.jpg"), JPEG_B).unwrap();
        cache
    }

    fn source_digest(path: &Path) -> Vec<u8> {
        Sha256::digest(fs::read(path).unwrap()).to_vec()
    }

    #[test]
    fn first_mode_uses_query_order_and_falls_back_from_invalid_jpeg() {
        let directory = tempdir().unwrap();
        let cache = prepare_tree(directory.path());
        fs::write(directory.path().join("photos/album/bad.jpg"), b"not jpeg").unwrap();
        let input = settings(directory.path());

        let selected = select_from_candidates(
            &input,
            &cache,
            NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
            vec![
                candidate(1, "bad.jpg"),
                candidate(2, "b.jpg"),
                candidate(3, "a.jpg"),
            ],
        )
        .unwrap();

        assert_eq!(selected.image_id, 2);
        assert_eq!(fs::read(cache.join("current.jpg")).unwrap(), JPEG_B);
    }

    #[test]
    fn daily_random_is_deterministic_and_independent_of_query_order() {
        let directory = tempdir().unwrap();
        let mut input = settings(directory.path());
        input.selection.mode = SelectionMode::DailyRandom;
        let date = NaiveDate::from_ymd_opt(2026, 8, 20).unwrap();
        let candidates = vec![
            candidate(1, "a.jpg"),
            candidate(2, "b.jpg"),
            candidate(3, "c.jpg"),
        ];
        let mut reversed = candidates.clone();
        reversed.reverse();

        let first = rank_candidates(&input, date, candidates);
        let second = rank_candidates(&input, date, reversed);

        assert_eq!(
            first.iter().map(|item| item.image_id).collect::<Vec<_>>(),
            second.iter().map(|item| item.image_id).collect::<Vec<_>>()
        );
        assert_ne!(
            daily_rank("frame-a", date, 1),
            daily_rank("frame-a", date.succ_opt().unwrap(), 1)
        );
        assert_ne!(
            daily_rank("frame-a", date, 1),
            daily_rank("frame-b", date, 1)
        );
    }

    #[test]
    fn selection_date_honors_timezone_rollover_and_dst() {
        let directory = tempdir().unwrap();
        let input = settings(directory.path());

        let before = Utc.with_ymd_and_hms(2026, 3, 29, 0, 30, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 3, 29, 1, 30, 0).unwrap();

        assert_eq!(
            selection_date(&input, before).unwrap(),
            NaiveDate::from_ymd_opt(2026, 3, 28).unwrap()
        );
        assert_eq!(
            selection_date(&input, after).unwrap(),
            NaiveDate::from_ymd_opt(2026, 3, 29).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn selection_never_changes_or_links_source_image() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempdir().unwrap();
        let cache = prepare_tree(directory.path());
        let source_path = directory.path().join("photos/album/a.jpg");
        let before_digest = source_digest(&source_path);
        let before_modified: SystemTime = fs::metadata(&source_path).unwrap().modified().unwrap();
        let mut permissions = fs::metadata(&source_path).unwrap().permissions();
        permissions.set_mode(0o444);
        fs::set_permissions(&source_path, permissions).unwrap();
        let input = settings(directory.path());

        select_from_candidates(
            &input,
            &cache,
            NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
            vec![candidate(1, "a.jpg")],
        )
        .unwrap();

        let source_metadata = fs::metadata(&source_path).unwrap();
        let cache_metadata = fs::metadata(cache.join("current.jpg")).unwrap();
        assert_eq!(source_digest(&source_path), before_digest);
        assert_eq!(source_metadata.modified().unwrap(), before_modified);
        assert_ne!(source_metadata.ino(), cache_metadata.ino());
        assert!(
            !fs::symlink_metadata(cache.join("current.jpg"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
