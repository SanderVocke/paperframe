use std::{net::IpAddr, sync::Arc};

use anyhow::Result;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, watch};

use crate::{
    cache::CacheStore,
    config::{Config, ModeSettings, SettingsManager},
    folder::FolderMode,
    selection::SelectionResult,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRequest {
    pub requested_at: DateTime<Utc>,
    pub client_ip: IpAddr,
    pub response_status: StatusCode,
}

#[derive(Clone, Debug)]
pub struct SelectionSuccess {
    pub completed_at: DateTime<Utc>,
    pub result: SelectionResult,
}

#[derive(Clone, Debug, Default)]
pub struct SelectorStatus {
    pub running: bool,
    pub settings_generation: Option<u64>,
    pub last_attempt: Option<DateTime<Utc>>,
    pub last_success: Option<SelectionSuccess>,
    pub last_error: Option<String>,
    pub next_attempt: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub enum ActiveMode {
    Digikam,
    Folder(Arc<FolderMode>),
}

pub struct AppState {
    pub config: Config,
    pub settings: Arc<SettingsManager>,
    pub cache: CacheStore,
    last_image_request: RwLock<Option<ImageRequest>>,
    selector_status: RwLock<SelectorStatus>,
    active_mode: RwLock<ActiveMode>,
    selection_trigger: watch::Sender<u64>,
    csrf_token: String,
}

impl AppState {
    pub fn new(config: Config, settings: Arc<SettingsManager>, cache: CacheStore) -> Arc<Self> {
        let (selection_trigger, _) = watch::channel(0);
        Arc::new(Self {
            config,
            settings,
            cache,
            last_image_request: RwLock::new(None),
            selector_status: RwLock::new(SelectorStatus::default()),
            active_mode: RwLock::new(ActiveMode::Digikam),
            selection_trigger,
            csrf_token: random_token(),
        })
    }

    #[cfg(test)]
    pub fn with_last_image_request(
        config: Config,
        settings: Arc<SettingsManager>,
        cache: CacheStore,
        request: ImageRequest,
    ) -> Arc<Self> {
        let (selection_trigger, _) = watch::channel(0);
        Arc::new(Self {
            config,
            settings,
            cache,
            last_image_request: RwLock::new(Some(request)),
            selector_status: RwLock::new(SelectorStatus::default()),
            active_mode: RwLock::new(ActiveMode::Digikam),
            selection_trigger,
            csrf_token: random_token(),
        })
    }

    pub async fn initialize(
        config: Config,
        settings: Arc<SettingsManager>,
        cache: CacheStore,
    ) -> Result<Arc<Self>> {
        let state = Self::new(config, settings, cache);
        let snapshot = state.settings.snapshot().await;
        let mode = state.prepare_mode(&snapshot.settings.mode).await?;
        state.activate_mode(mode).await;
        Ok(state)
    }

    pub async fn prepare_mode(&self, settings: &ModeSettings) -> Result<ActiveMode> {
        match settings {
            ModeSettings::DigikamSqlite(settings) => {
                let settings = settings.clone();
                let cache_root = self.cache.root().to_path_buf();
                tokio::task::spawn_blocking(move || {
                    crate::source::SourceStore::new(&settings, &cache_root).map(|_| ())
                })
                .await
                .map_err(|error| anyhow::anyhow!("source validation worker stopped: {error}"))??;
                Ok(ActiveMode::Digikam)
            }
            ModeSettings::FolderSequence(settings) => Ok(ActiveMode::Folder(
                FolderMode::new(settings, self.cache.root(), &self.config.settings_path).await?,
            )),
        }
    }

    pub async fn activate_mode(&self, mode: ActiveMode) {
        *self.active_mode.write().await = mode;
    }

    pub async fn active_mode(&self) -> ActiveMode {
        self.active_mode.read().await.clone()
    }

    pub async fn is_ready(&self) -> bool {
        match self.active_mode().await {
            ActiveMode::Digikam => self.cache.is_ready(),
            ActiveMode::Folder(folder) => folder.is_ready().await,
        }
    }

    pub fn csrf_token(&self) -> &str {
        &self.csrf_token
    }

    pub async fn last_image_request(&self) -> Option<ImageRequest> {
        self.last_image_request.read().await.clone()
    }

    pub async fn record_image_request(&self, request: ImageRequest) {
        *self.last_image_request.write().await = Some(request);
    }

    pub async fn selector_status(&self) -> SelectorStatus {
        self.selector_status.read().await.clone()
    }

    pub async fn selection_started(&self, generation: u64, attempted_at: DateTime<Utc>) {
        let mut status = self.selector_status.write().await;
        status.running = true;
        status.settings_generation = Some(generation);
        status.last_attempt = Some(attempted_at);
        status.next_attempt = None;
    }

    pub async fn selection_finished(
        &self,
        result: &anyhow::Result<SelectionResult>,
        completed_at: DateTime<Utc>,
    ) {
        let mut status = self.selector_status.write().await;
        status.running = false;
        match result {
            Ok(result) => {
                status.last_success = Some(SelectionSuccess {
                    completed_at,
                    result: result.clone(),
                });
                status.last_error = None;
            }
            Err(error) => status.last_error = Some(error.to_string()),
        }
    }

    pub async fn set_next_selection_attempt(&self, next_attempt: DateTime<Utc>) {
        self.selector_status.write().await.next_attempt = Some(next_attempt);
    }

    pub fn subscribe_to_selection_triggers(&self) -> watch::Receiver<u64> {
        self.selection_trigger.subscribe()
    }

    pub fn trigger_selection(&self) {
        self.selection_trigger.send_modify(|generation| {
            *generation = generation.saturating_add(1);
        });
    }
}

fn random_token() -> String {
    let bytes = rand::random::<[u8; 32]>();
    let mut token = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    token
}
