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
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
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

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(flatten)]
    pub mode: ModeSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", content = "mode_settings", rename_all = "snake_case")]
pub enum ModeSettings {
    DigikamSqlite(DigikamSettings),
    FolderSequence(FolderSettings),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DigikamSettings {
    pub database_path: PathBuf,
    pub query: String,
    pub path_mappings: Vec<PathMapping>,
    pub selection: SelectionSettings,
    pub max_candidates: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FolderSettings {
    pub directory: PathBuf,
    pub max_upload_bytes: u64,
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

#[derive(Deserialize)]
struct CurrentSettings {
    #[serde(flatten)]
    mode: ModeSettings,
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = toml::Value::deserialize(deserializer)?;
        let table = value
            .as_table()
            .ok_or_else(|| D::Error::custom("settings must be a TOML table"))?;
        if table.contains_key("mode") {
            if let Some(unknown) = table
                .keys()
                .find(|key| key.as_str() != "mode" && key.as_str() != "mode_settings")
            {
                return Err(D::Error::custom(format!(
                    "unknown top-level field in tagged settings: {unknown}"
                )));
            }
            let current: CurrentSettings = value.try_into().map_err(D::Error::custom)?;
            Ok(Self { mode: current.mode })
        } else {
            let legacy: DigikamSettings = value.try_into().map_err(D::Error::custom)?;
            Ok(Self {
                mode: ModeSettings::DigikamSqlite(legacy),
            })
        }
    }
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
        match &self.mode {
            ModeSettings::DigikamSqlite(settings) => settings.validate_basic(),
            ModeSettings::FolderSequence(settings) => settings.validate_basic(),
        }
    }

    pub fn digikam(&self) -> Option<&DigikamSettings> {
        match &self.mode {
            ModeSettings::DigikamSqlite(settings) => Some(settings),
            ModeSettings::FolderSequence(_) => None,
        }
    }

    pub fn folder(&self) -> Option<&FolderSettings> {
        match &self.mode {
            ModeSettings::FolderSequence(settings) => Some(settings),
            ModeSettings::DigikamSqlite(_) => None,
        }
    }
}

impl DigikamSettings {
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

impl FolderSettings {
    pub fn validate_basic(&self) -> Result<()> {
        if self.directory.as_os_str().is_empty() {
            bail!("folder directory must not be empty");
        }
        if !self.directory.is_absolute() {
            bail!("folder directory must be absolute");
        }
        if self.max_upload_bytes == 0 || self.max_upload_bytes > 100 * 1024 * 1024 {
            bail!("max_upload_bytes must be between 1 and 104857600");
        }
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
    let temporary_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
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
    for entry in fs::read_dir(parent)
        .with_context(|| format!("failed to inspect settings directory {}", parent.display()))?
    {
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
    fs::File::open(path)
        .with_context(|| format!("failed to open directory {} for sync", path.display()))?
        .sync_all()
        .context("failed to sync settings directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn digikam() -> DigikamSettings {
        DigikamSettings {
            database_path: "/catalog/digikam.db".into(),
            query: "SELECT 1".into(),
            path_mappings: vec![PathMapping {
                database_root: "/library".into(),
                mounted_root: "/photos".into(),
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

    #[test]
    fn parses_legacy_and_round_trips_canonical_modes() {
        let legacy = toml::to_string(&digikam()).unwrap();
        let parsed: Settings = toml::from_str(&legacy).unwrap();
        assert!(parsed.digikam().is_some());

        let folder = Settings {
            mode: ModeSettings::FolderSequence(FolderSettings {
                directory: "/srv/frame-images".into(),
                max_upload_bytes: 1024,
            }),
        };
        let encoded = toml::to_string_pretty(&folder).unwrap();
        assert!(encoded.contains("mode = \"folder_sequence\""));
        assert_eq!(toml::from_str::<Settings>(&encoded).unwrap(), folder);

        let mixed = format!("{encoded}\ndatabase_path = '/unexpected.db'\n");
        assert!(toml::from_str::<Settings>(&mixed).is_err());
    }

    #[tokio::test]
    async fn persists_replacement_and_rejects_stale_generation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let original = Settings {
            mode: ModeSettings::DigikamSqlite(digikam()),
        };
        persist_settings(&path, &original).unwrap();
        let manager = SettingsManager::load(path.clone()).unwrap();
        let replacement = Settings {
            mode: ModeSettings::FolderSequence(FolderSettings {
                directory: directory.path().to_path_buf(),
                max_upload_bytes: 2048,
            }),
        };
        manager.replace(1, replacement.clone()).await.unwrap();
        assert_eq!(Settings::load(&path).unwrap(), replacement);
        assert!(matches!(
            manager.replace(1, original).await,
            Err(ReplaceSettingsError::Stale {
                current_generation: 2
            })
        ));
    }

    #[test]
    fn validates_mode_specific_fields() {
        let mut folder = FolderSettings {
            directory: "relative".into(),
            max_upload_bytes: 0,
        };
        assert!(folder.validate_basic().is_err());
        folder.directory = "/images".into();
        folder.max_upload_bytes = 1;
        folder.validate_basic().unwrap();

        let mut digi = digikam();
        digi.max_candidates = 0;
        assert!(digi.validate_basic().is_err());
    }
}
