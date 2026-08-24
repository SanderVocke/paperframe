use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, SocketAddr},
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

use crate::renderer::{FrameTarget, OutputMode};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
const DEFAULT_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bind: SocketAddr,
    pub settings_path: PathBuf,
    #[serde(alias = "cache_dir")]
    pub queue_dir: PathBuf,
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
        if config.settings_path.as_os_str().is_empty() {
            bail!("settings_path must not be empty");
        }
        if config.queue_dir.as_os_str().is_empty() {
            bail!("queue_dir must not be empty");
        }
        Ok(config)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Settings {
    pub queue: QueueSettings,
    pub rendering: RenderingSettings,
    pub advancement: AdvancementSettings,
    pub suppliers: Vec<SupplierSettings>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderingSettings {
    #[serde(default)]
    pub target: FrameTarget,
    #[serde(default)]
    pub output_mode: OutputMode,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueueSettings {
    #[serde(default = "default_target")]
    pub target_length: usize,
    #[serde(default = "default_upload_bytes")]
    pub max_upload_bytes: u64,
}

fn default_target() -> usize {
    10
}
fn default_upload_bytes() -> u64 {
    DEFAULT_UPLOAD_BYTES
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", content = "settings", rename_all = "snake_case")]
pub enum AdvancementSettings {
    EveryFetch {
        #[serde(default)]
        frame_client_ips: Vec<IpAddr>,
    },
    Scheduled {
        timezone: String,
        times: Vec<String>,
    },
    Manual,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SupplierSettings {
    pub id: String,
    #[serde(flatten)]
    pub kind: SupplierKind,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", content = "settings", rename_all = "snake_case")]
pub enum SupplierKind {
    DigikamSqlite(DigikamSettings),
    ManualFolder(ManualFolderSettings),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DigikamSettings {
    pub database_path: PathBuf,
    pub query: String,
    pub path_mappings: Vec<PathMapping>,
    pub max_candidates: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManualFolderSettings {
    pub directory: PathBuf,
    #[serde(default = "default_poll_seconds")]
    pub poll_interval_seconds: u64,
}

fn default_poll_seconds() -> u64 {
    60
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathMapping {
    pub database_root: PathBuf,
    pub mounted_root: PathBuf,
}

// Compatibility shapes from the two previously released settings formats.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OldDigikamSettings {
    database_path: PathBuf,
    query: String,
    path_mappings: Vec<PathMapping>,
    selection: OldSelectionSettings,
    max_candidates: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OldFolderSettings {
    directory: PathBuf,
    max_upload_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OldSelectionSettings {
    mode: OldSelectionMode,
    random_seed: String,
    timezone: String,
    rollover_time: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OldSelectionMode {
    First,
    DailyRandom,
}

#[derive(Deserialize)]
#[serde(tag = "mode", content = "mode_settings", rename_all = "snake_case")]
enum OldModeSettings {
    DigikamSqlite(OldDigikamSettings),
    FolderSequence(OldFolderSettings),
}

#[derive(Deserialize)]
struct OldTagged {
    #[serde(flatten)]
    mode: OldModeSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalSettings {
    queue: QueueSettings,
    #[serde(default)]
    rendering: RenderingSettings,
    advancement: AdvancementSettings,
    suppliers: Vec<SupplierSettings>,
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = toml::Value::deserialize(deserializer)?;
        let table = value
            .as_table()
            .ok_or_else(|| D::Error::custom("settings must be a table"))?;
        if table.contains_key("queue") || table.contains_key("suppliers") {
            let current: CanonicalSettings = value.try_into().map_err(D::Error::custom)?;
            return Ok(Self {
                queue: current.queue,
                rendering: current.rendering,
                advancement: current.advancement,
                suppliers: current.suppliers,
            });
        }
        if table.contains_key("mode") {
            let old: OldTagged = value.try_into().map_err(D::Error::custom)?;
            return Ok(migrate_old_mode(old.mode));
        }
        let old: OldDigikamSettings = value.try_into().map_err(D::Error::custom)?;
        Ok(migrate_digikam(old))
    }
}

fn migrate_old_mode(mode: OldModeSettings) -> Settings {
    match mode {
        OldModeSettings::DigikamSqlite(settings) => migrate_digikam(settings),
        OldModeSettings::FolderSequence(settings) => Settings {
            queue: QueueSettings {
                target_length: 10,
                max_upload_bytes: settings.max_upload_bytes,
            },
            rendering: RenderingSettings::default(),
            advancement: AdvancementSettings::EveryFetch {
                frame_client_ips: Vec::new(),
            },
            suppliers: vec![SupplierSettings {
                id: "primary".into(),
                kind: SupplierKind::ManualFolder(ManualFolderSettings {
                    directory: settings.directory,
                    poll_interval_seconds: 60,
                }),
            }],
        },
    }
}

fn migrate_digikam(old: OldDigikamSettings) -> Settings {
    let _ = (old.selection.mode, old.selection.random_seed);
    Settings {
        queue: QueueSettings {
            target_length: 10,
            max_upload_bytes: DEFAULT_UPLOAD_BYTES,
        },
        rendering: RenderingSettings::default(),
        advancement: AdvancementSettings::Scheduled {
            timezone: old.selection.timezone,
            times: vec![old.selection.rollover_time],
        },
        suppliers: vec![SupplierSettings {
            id: "primary".into(),
            kind: SupplierKind::DigikamSqlite(DigikamSettings {
                database_path: old.database_path,
                query: old.query,
                path_mappings: old.path_mappings,
                max_candidates: old.max_candidates,
            }),
        }],
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        cleanup_temporary_files(path)?;
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read settings from {}", path.display()))?;
        let settings: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse settings from {}", path.display()))?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> Result<()> {
        if self.queue.target_length == 0 || self.queue.target_length > 100_000 {
            bail!("target_length must be between 1 and 100000");
        }
        if self.queue.max_upload_bytes == 0 || self.queue.max_upload_bytes > 100 * 1024 * 1024 {
            bail!("max_upload_bytes must be between 1 and 104857600");
        }
        if self.suppliers.len() != 1 {
            bail!("exactly one supplier is currently supported");
        }
        let mut ids = HashSet::new();
        for supplier in &self.suppliers {
            if supplier.id.trim().is_empty() || !ids.insert(&supplier.id) {
                bail!("supplier IDs must be non-empty and unique");
            }
            supplier.validate()?;
        }
        if let AdvancementSettings::Scheduled { timezone, times } = &self.advancement {
            let _: Tz = timezone
                .parse()
                .with_context(|| format!("invalid IANA timezone: {timezone}"))?;
            if times.is_empty() {
                bail!("scheduled advancement requires at least one time");
            }
            let mut unique = HashSet::new();
            for time in times {
                NaiveTime::parse_from_str(time, "%H:%M")
                    .with_context(|| format!("invalid scheduled time (expected HH:MM): {time}"))?;
                if !unique.insert(time) {
                    bail!("scheduled times must be unique");
                }
            }
        }
        Ok(())
    }

    pub fn supplier(&self) -> &SupplierSettings {
        &self.suppliers[0]
    }
}

impl SupplierSettings {
    fn validate(&self) -> Result<()> {
        match &self.kind {
            SupplierKind::DigikamSqlite(settings) => settings.validate(),
            SupplierKind::ManualFolder(settings) => {
                if !settings.directory.is_absolute() && !settings.directory.has_root() {
                    bail!("manual folder directory must be absolute");
                }
                if settings.poll_interval_seconds == 0 || settings.poll_interval_seconds > 86400 {
                    bail!("poll_interval_seconds must be between 1 and 86400");
                }
                Ok(())
            }
        }
    }
}

impl DigikamSettings {
    pub fn validate(&self) -> Result<()> {
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
            if (!mapping.database_root.is_absolute() && !mapping.database_root.has_root())
                || (!mapping.mounted_root.is_absolute() && !mapping.mounted_root.has_root())
            {
                bail!("path mapping roots must be absolute");
            }
            if !roots.insert(mapping.database_root.clone()) {
                bail!("database_root values must be unique");
            }
        }
        if self.max_candidates == 0 || self.max_candidates > 1_000_000 {
            bail!("max_candidates must be between 1 and 1000000");
        }
        Ok(())
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
        expected: u64,
        settings: Settings,
    ) -> std::result::Result<SettingsSnapshot, ReplaceSettingsError> {
        settings
            .validate()
            .map_err(|error| ReplaceSettingsError::Invalid(error.to_string()))?;
        let _guard = self.update_lock.lock().await;
        let generation = self.snapshot.read().await.generation;
        if generation != expected {
            return Err(ReplaceSettingsError::Stale {
                current_generation: generation,
            });
        }
        let path = self.path.clone();
        let persisted = settings.clone();
        tokio::task::spawn_blocking(move || persist_settings(&path, &persisted))
            .await
            .map_err(|error| ReplaceSettingsError::Persistence(error.to_string()))?
            .map_err(|error| ReplaceSettingsError::Persistence(error.to_string()))?;
        let next = SettingsSnapshot {
            generation: generation.saturating_add(1),
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
        .context("settings path must have a UTF-8 filename")?;
    let temporary = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let serialized = toml::to_string_pretty(settings).context("failed to serialize settings")?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("failed to create settings temporary")?;
        file.write_all(serialized.as_bytes())
            .context("failed to write settings temporary")?;
        file.sync_all()
            .context("failed to sync settings temporary")?;
        fs::rename(&temporary, path).context("failed to atomically replace settings")?;
        let _ = sync_directory(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn cleanup_temporary_files(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("settings filename must be UTF-8")?;
    let prefix = format!(".{name}.");
    for entry in fs::read_dir(parent).context("failed to inspect settings directory")? {
        let entry = entry?;
        let entry_name = entry.file_name();
        let entry_name = entry_name.to_string_lossy();
        if entry_name.starts_with(&prefix) && entry_name.ends_with(".tmp") {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::File::open(path)?
            .sync_all()
            .context("failed to sync directory")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_modes_and_round_trips_canonical() {
        let old_folder =
            "mode='folder_sequence'\n[mode_settings]\ndirectory='/images'\nmax_upload_bytes=1000\n";
        let folder: Settings = toml::from_str(old_folder).unwrap();
        assert!(matches!(
            folder.advancement,
            AdvancementSettings::EveryFetch { .. }
        ));
        assert!(matches!(
            folder.supplier().kind,
            SupplierKind::ManualFolder(_)
        ));
        let encoded = toml::to_string_pretty(&folder).unwrap();
        assert_eq!(toml::from_str::<Settings>(&encoded).unwrap(), folder);

        let legacy = "database_path='/db'\nquery='SELECT 1'\nmax_candidates=10\n[[path_mappings]]\ndatabase_root='/a'\nmounted_root='/b'\n[selection]\nmode='first'\nrandom_seed=''\ntimezone='UTC'\nrollover_time='03:00'\n";
        let digi: Settings = toml::from_str(legacy).unwrap();
        assert!(matches!(
            digi.advancement,
            AdvancementSettings::Scheduled { .. }
        ));
        assert!(matches!(
            digi.supplier().kind,
            SupplierKind::DigikamSqlite(_)
        ));

        let tagged = "mode='digikam_sqlite'\n[mode_settings]\ndatabase_path='/db'\nquery='SELECT 1'\nmax_candidates=10\n[[mode_settings.path_mappings]]\ndatabase_root='/a'\nmounted_root='/b'\n[mode_settings.selection]\nmode='daily_random'\nrandom_seed='old'\ntimezone='UTC'\nrollover_time='03:00'\n";
        let tagged_digi: Settings = toml::from_str(tagged).unwrap();
        assert_eq!(tagged_digi.queue.target_length, 10);
        assert!(matches!(
            tagged_digi.supplier().kind,
            SupplierKind::DigikamSqlite(_)
        ));
    }

    #[test]
    fn validates_target_supplier_and_schedule() {
        let mut settings: Settings = toml::from_str("queue={target_length=10,max_upload_bytes=100}\nadvancement={type='manual'}\n[[suppliers]]\nid='one'\ntype='manual_folder'\n[suppliers.settings]\ndirectory='/images'\npoll_interval_seconds=60\n").unwrap();
        settings.validate().unwrap();
        assert_eq!(settings.rendering, RenderingSettings::default());
        settings.queue.target_length = 0;
        assert!(settings.validate().is_err());
        settings.queue.target_length = 10;
        settings.suppliers.push(settings.suppliers[0].clone());
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rendering_settings_round_trip_and_legacy_defaults_are_stable() {
        let settings: Settings = toml::from_str("queue={target_length=10,max_upload_bytes=100}\nrendering={target='paper_l',output_mode='server_dither_bmp'}\nadvancement={type='manual'}\n[[suppliers]]\nid='one'\ntype='manual_folder'\n[suppliers.settings]\ndirectory='/images'\npoll_interval_seconds=60\n").unwrap();
        assert_eq!(settings.rendering.target, FrameTarget::PaperL);
        assert_eq!(settings.rendering.output_mode, OutputMode::ServerDitherBmp);
        let encoded = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&encoded).unwrap(), settings);
    }

    #[test]
    fn documented_examples_parse_and_validate() {
        for example in [
            include_str!("../settings.example.toml"),
            include_str!("../settings.folder.example.toml"),
        ] {
            let settings: Settings = toml::from_str(example).unwrap();
            settings.validate().unwrap();
        }
    }

    #[test]
    fn bootstrap_accepts_legacy_cache_dir() {
        let config: Config =
            toml::from_str("bind='127.0.0.1:1'\nsettings_path='s'\ncache_dir='q'\n").unwrap();
        assert_eq!(config.queue_dir, PathBuf::from("q"));
    }
}
