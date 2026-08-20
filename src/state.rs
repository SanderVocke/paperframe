use std::{net::IpAddr, sync::Arc};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, watch};

use crate::{
    cache::CacheStore,
    config::{Config, SettingsManager},
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

pub struct AppState {
    pub config: Config,
    pub settings: Arc<SettingsManager>,
    pub cache: CacheStore,
    last_image_request: RwLock<Option<ImageRequest>>,
    selector_status: RwLock<SelectorStatus>,
    selection_trigger: watch::Sender<u64>,
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
            selection_trigger,
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
            selection_trigger,
        })
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
