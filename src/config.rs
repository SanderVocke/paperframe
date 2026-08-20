use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use chrono::NaiveTime;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bind: SocketAddr,
    pub settings_path: PathBuf,
    pub cache_dir: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read bootstrap configuration from {}",
                path.display()
            )
        })?;
        let config: Self = toml::from_str(&contents).with_context(|| {
            format!(
                "failed to parse bootstrap configuration from {}",
                path.display()
            )
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.settings_path.as_os_str().is_empty() {
            bail!("settings_path must not be empty");
        }
        if self.cache_dir.as_os_str().is_empty() {
            bail!("cache_dir must not be empty");
        }
        Ok(())
    }

    pub fn current_image_path(&self) -> PathBuf {
        self.cache_dir.join("current.jpg")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub database_path: PathBuf,
    pub query: String,
    pub path_mappings: Vec<PathMapping>,
    pub selection: SelectionSettings,
    pub max_candidates: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathMapping {
    pub database_root: PathBuf,
    pub mounted_root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SelectionSettings {
    pub mode: SelectionMode,
    pub random_seed: String,
    pub timezone: String,
    pub rollover_time: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    First,
    DailyRandom,
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        cleanup_temporary_files(path)?;
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read settings from {}", path.display()))?;
        let settings: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse settings from {}", path.display()))?;
        settings.validate_basic()?;
        Ok(settings)
    }

    pub fn validate_basic(&self) -> Result<()> {
        if self.database_path.as_os_str().is_empty() {
            bail!("database_path must not be empty");
        }
        if self.query.trim().is_empty() {
            bail!("query must not be empty");
        }
        if self.path_mappings.is_empty() {
            bail!("at least one path mapping is required");
        }
        let mut roots = HashSet::new();
        for mapping in &self.path_mappings {
            if !mapping.database_root.is_absolute() {
                bail!(
                    "database_root must be absolute: {}",
                    mapping.database_root.display()
                );
            }
            if !mapping.mounted_root.is_absolute() {
                bail!(
                    "mounted_root must be absolute: {}",
                    mapping.mounted_root.display()
                );
            }
            if !roots.insert(mapping.database_root.clone()) {
                bail!("database_root values must be unique");
            }
        }
        if self.max_candidates == 0 {
            bail!("max_candidates must be greater than zero");
        }
        if self.max_candidates > 1_000_000 {
            bail!("max_candidates must not exceed 1000000");
        }
        if self.selection.mode == SelectionMode::DailyRandom
            && self.selection.random_seed.trim().is_empty()
        {
            bail!("random_seed must not be empty in daily_random mode");
        }
        self.selection.timezone()?;
        self.selection.rollover_time()?;
        Ok(())
    }
}

impl SelectionSettings {
    pub fn timezone(&self) -> Result<Tz> {
        self.timezone
            .parse()
            .with_context(|| format!("invalid IANA timezone: {}", self.timezone))
    }

    pub fn rollover_time(&self) -> Result<NaiveTime> {
        NaiveTime::parse_from_str(&self.rollover_time, "%H:%M").with_context(|| {
            format!(
                "invalid rollover_time (expected HH:MM): {}",
                self.rollover_time
            )
        })
    }
}

#[derive(Clone, Debug)]
pub struct SettingsSnapshot {
    pub generation: u64,
    pub settings: Settings,
}

pub struct SettingsManager {
    path: PathBuf,
    snapshot: RwLock<SettingsSnapshot>,
    update_lock: Mutex<()>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplaceSettingsError {
    Stale { current_generation: u64 },
    Invalid(String),
    Persistence(String),
}

impl SettingsManager {
    pub fn load(path: PathBuf) -> Result<Arc<Self>> {
        let settings = Settings::load(&path)?;
        Ok(Arc::new(Self {
            path,
            snapshot: RwLock::new(SettingsSnapshot {
                generation: 1,
                settings,
            }),
            update_lock: Mutex::new(()),
        }))
    }

    #[cfg(test)]
    pub fn in_memory(settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            path: PathBuf::from("unused-settings.toml"),
            snapshot: RwLock::new(SettingsSnapshot {
                generation: 1,
                settings,
            }),
            update_lock: Mutex::new(()),
        })
    }

    pub async fn snapshot(&self) -> SettingsSnapshot {
        self.snapshot.read().await.clone()
    }

    pub async fn replace(
        &self,
        expected_generation: u64,
        settings: Settings,
    ) -> std::result::Result<SettingsSnapshot, ReplaceSettingsError> {
        settings
            .validate_basic()
            .map_err(|error| ReplaceSettingsError::Invalid(error.to_string()))?;
        let _guard = self.update_lock.lock().await;
        let current_generation = self.snapshot.read().await.generation;
        if current_generation != expected_generation {
            return Err(ReplaceSettingsError::Stale { current_generation });
        }

        let path = self.path.clone();
        let to_persist = settings.clone();
        tokio::task::spawn_blocking(move || persist_settings(&path, &to_persist))
            .await
            .map_err(|error| ReplaceSettingsError::Persistence(error.to_string()))?
            .map_err(|error| ReplaceSettingsError::Persistence(error.to_string()))?;

        let next = SettingsSnapshot {
            generation: current_generation.saturating_add(1),
            settings,
        };
        *self.snapshot.write().await = next.clone();
        Ok(next)
    }
}

fn persist_settings(path: &Path, settings: &Settings) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("settings path must have a UTF-8 file name")?;
    let temporary_name = format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let temporary_path = parent.join(temporary_name);
    let serialized = toml::to_string_pretty(settings).context("failed to serialize settings")?;

    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .with_context(|| {
                format!(
                    "failed to create temporary settings file in {}",
                    parent.display()
                )
            })?;
        file.write_all(serialized.as_bytes())
            .context("failed to write temporary settings file")?;
        file.sync_all()
            .context("failed to sync temporary settings file")?;
        fs::rename(&temporary_path, path).context("failed to atomically replace settings file")?;
        sync_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn cleanup_temporary_files(settings_path: &Path) -> Result<()> {
    let parent = settings_path.parent().unwrap_or_else(|| Path::new("."));
    let Some(file_name) = settings_path.file_name().and_then(|name| name.to_str()) else {
        bail!("settings path must have a UTF-8 file name");
    };
    let prefix = format!(".{file_name}.");
    let entries = fs::read_dir(parent)
        .with_context(|| format!("failed to inspect settings directory {}", parent.display()))?;
    for entry in entries {
        let entry = entry.context("failed to inspect settings temporary file")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            fs::remove_file(entry.path())
                .context("failed to remove abandoned settings temporary file")?;
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = fs::File::open(path)
        .with_context(|| format!("failed to open directory {} for sync", path.display()))?;
    directory
        .sync_all()
        .context("failed to sync settings directory")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn example_settings(database_path: PathBuf, mounted_root: PathBuf) -> Settings {
        Settings {
            database_path,
            query: "SELECT 1 AS image_id, '/photos' AS root_path, '/' AS album_path, 'one.jpg' AS file_name".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/original/photos"),
                mounted_root,
            }],
            selection: SelectionSettings {
                mode: SelectionMode::DailyRandom,
                random_seed: "living-room".into(),
                timezone: "Europe/Amsterdam".into(),
                rollover_time: "03:00".into(),
            },
            max_candidates: 5000,
        }
    }

    #[test]
    fn loads_valid_bootstrap_configuration() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "bind = \"127.0.0.1:8080\"\nsettings_path = \"settings.toml\"\ncache_dir = \"cache\"\n",
        )
        .unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.settings_path, PathBuf::from("settings.toml"));
        assert_eq!(
            config.current_image_path(),
            PathBuf::from("cache/current.jpg")
        );
    }

    #[test]
    fn rejects_invalid_bootstrap_configuration() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "bind = \"127.0.0.1:8080\"\nsettings_path = \"\"\ncache_dir = \"cache\"\n",
        )
        .unwrap();

        assert!(
            Config::load(&path)
                .unwrap_err()
                .to_string()
                .contains("settings_path")
        );
    }

    #[test]
    fn validates_settings_fields() {
        let directory = tempdir().unwrap();
        let mut settings = example_settings(
            directory.path().join("digikam.db"),
            directory.path().join("photos"),
        );
        settings.max_candidates = 0;
        assert!(
            settings
                .validate_basic()
                .unwrap_err()
                .to_string()
                .contains("max_candidates")
        );
        settings.max_candidates = 10;
        settings.selection.timezone = "Not/AZone".into();
        assert!(
            settings
                .validate_basic()
                .unwrap_err()
                .to_string()
                .contains("timezone")
        );
    }

    #[tokio::test]
    async fn persists_replacement_and_reloads_after_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let original = example_settings(
            directory.path().join("digikam.db"),
            directory.path().join("photos"),
        );
        persist_settings(&path, &original).unwrap();
        let manager = SettingsManager::load(path.clone()).unwrap();
        let mut replacement = original.clone();
        replacement.query = "SELECT 2 AS image_id, '/photos' AS root_path, '/' AS album_path, 'two.jpg' AS file_name".into();

        let snapshot = manager.replace(1, replacement.clone()).await.unwrap();

        assert_eq!(snapshot.generation, 2);
        assert_eq!(Settings::load(&path).unwrap(), replacement);
        assert_eq!(
            SettingsManager::load(path)
                .unwrap()
                .snapshot()
                .await
                .settings,
            replacement
        );
    }

    #[tokio::test]
    async fn rejects_stale_generation_without_changing_disk() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let original = example_settings(
            directory.path().join("digikam.db"),
            directory.path().join("photos"),
        );
        persist_settings(&path, &original).unwrap();
        let manager = SettingsManager::load(path.clone()).unwrap();
        let before = fs::read(&path).unwrap();

        let error = manager.replace(99, original).await.unwrap_err();

        assert_eq!(
            error,
            ReplaceSettingsError::Stale {
                current_generation: 1
            }
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn cleans_abandoned_temporary_settings_files() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let settings = example_settings(
            directory.path().join("digikam.db"),
            directory.path().join("photos"),
        );
        persist_settings(&path, &settings).unwrap();
        let abandoned = directory.path().join(".settings.toml.123.1.tmp");
        fs::write(&abandoned, "partial").unwrap();

        Settings::load(&path).unwrap();

        assert!(!abandoned.exists());
    }

    #[tokio::test]
    async fn persistence_failure_keeps_live_settings() {
        let directory = tempdir().unwrap();
        let missing_parent = directory.path().join("gone");
        fs::create_dir(&missing_parent).unwrap();
        let path = missing_parent.join("settings.toml");
        let original = example_settings(
            directory.path().join("digikam.db"),
            directory.path().join("photos"),
        );
        persist_settings(&path, &original).unwrap();
        let manager = SettingsManager::load(path).unwrap();
        fs::remove_dir_all(missing_parent).unwrap();
        let mut replacement = original.clone();
        replacement.max_candidates = 12;

        assert!(matches!(
            manager.replace(1, replacement).await,
            Err(ReplaceSettingsError::Persistence(_))
        ));
        assert_eq!(manager.snapshot().await.settings, original);
    }
}
