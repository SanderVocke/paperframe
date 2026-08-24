use std::{net::IpAddr, sync::Arc};

use anyhow::{Context, Result, bail};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, RwLock, Semaphore, watch};

use crate::{
    config::{Config, Settings, SettingsManager, SupplierKind},
    queue::QueueStore,
    supplier::SupplierRegistry,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRequest {
    pub requested_at: DateTime<Utc>,
    pub client_ip: IpAddr,
    pub response_status: StatusCode,
}

#[derive(Clone, Debug, Default)]
pub struct RefillStatus {
    pub running: bool,
    pub last_success: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_added: usize,
}

pub struct AppState {
    pub config: Config,
    pub settings: Arc<SettingsManager>,
    pub queue: Arc<QueueStore>,
    registry: RwLock<Arc<SupplierRegistry>>,
    refill_status: RwLock<RefillStatus>,
    last_image_request: RwLock<Option<ImageRequest>>,
    runtime_trigger: watch::Sender<u64>,
    pub frame_serialization: Arc<Mutex<()>>,
    pub render_serialization: Arc<Semaphore>,
    pub rendering_transition: RwLock<()>,
    csrf_token: String,
}

impl AppState {
    pub async fn initialize(config: Config, settings: Arc<SettingsManager>) -> Result<Arc<Self>> {
        let snapshot = settings.snapshot().await;
        snapshot.settings.validate()?;
        std::fs::create_dir_all(&config.queue_dir).with_context(|| {
            format!("failed to create queue root {}", config.queue_dir.display())
        })?;
        validate_settings_separation(&config, &snapshot.settings)?;
        let migrate_current = matches!(
            snapshot.settings.supplier().kind,
            SupplierKind::DigikamSqlite(_)
        );
        let queue = QueueStore::new(&config.queue_dir, migrate_current).await?;
        let expected_media = match snapshot.settings.rendering.output_mode {
            crate::renderer::OutputMode::ServerDitherBmp => "image/bmp",
            crate::renderer::OutputMode::DirectJpeg => "image/jpeg",
        };
        let queue_snapshot = queue.snapshot().await;
        let needs_rerender = queue_snapshot.entries.iter().any(|entry| {
            entry.media_type != expected_media
                || (snapshot.settings.rendering.output_mode
                    == crate::renderer::OutputMode::ServerDitherBmp
                    && (entry.frame_target != snapshot.settings.rendering.target
                        || entry.renderer_profile != crate::renderer::RENDER_PROFILE_VERSION))
        });
        if needs_rerender {
            queue
                .rerender_all(
                    snapshot.settings.rendering.target,
                    snapshot.settings.rendering.output_mode,
                )
                .await
                .context("failed to render existing queue for configured frame output")?;
        }
        let registry =
            SupplierRegistry::prepare(&snapshot.settings.suppliers, queue.root(), false).await?;
        let (runtime_trigger, _) = watch::channel(0);
        Ok(Arc::new(Self {
            config,
            settings,
            queue,
            registry: RwLock::new(registry),
            refill_status: RwLock::new(RefillStatus::default()),
            last_image_request: RwLock::new(None),
            runtime_trigger,
            frame_serialization: Arc::new(Mutex::new(())),
            render_serialization: Arc::new(Semaphore::new(1)),
            rendering_transition: RwLock::new(()),
            csrf_token: random_token(),
        }))
    }

    pub async fn prepare_registry(&self, settings: &Settings) -> Result<Arc<SupplierRegistry>> {
        settings.validate()?;
        validate_settings_separation(&self.config, settings)?;
        SupplierRegistry::prepare(&settings.suppliers, self.queue.root(), true).await
    }

    pub async fn activate_registry(&self, registry: Arc<SupplierRegistry>) {
        *self.registry.write().await = registry;
        self.trigger_runtime();
    }

    pub async fn registry(&self) -> Arc<SupplierRegistry> {
        self.registry.read().await.clone()
    }

    pub fn csrf_token(&self) -> &str {
        &self.csrf_token
    }
    pub fn trigger_runtime(&self) {
        self.runtime_trigger
            .send_modify(|value| *value = value.saturating_add(1));
    }
    pub fn subscribe_runtime(&self) -> watch::Receiver<u64> {
        self.runtime_trigger.subscribe()
    }

    pub async fn last_image_request(&self) -> Option<ImageRequest> {
        self.last_image_request.read().await.clone()
    }
    pub async fn record_image_request(&self, request: ImageRequest) {
        *self.last_image_request.write().await = Some(request);
    }
    pub async fn refill_status(&self) -> RefillStatus {
        self.refill_status.read().await.clone()
    }
    pub async fn refill_started(&self) {
        self.refill_status.write().await.running = true;
    }
    pub async fn refill_finished(&self, result: &Result<usize>) {
        let mut status = self.refill_status.write().await;
        status.running = false;
        match result {
            Ok(added) => {
                status.last_success = Some(Utc::now());
                status.last_added = *added;
                status.last_error = None;
            }
            Err(error) => status.last_error = Some(error.to_string()),
        }
    }
}

fn validate_settings_separation(config: &Config, settings: &Settings) -> Result<()> {
    let queue = std::fs::canonicalize(&config.queue_dir).with_context(|| {
        format!(
            "failed to canonicalize queue root {}",
            config.queue_dir.display()
        )
    })?;
    let settings_path = std::fs::canonicalize(&config.settings_path).with_context(|| {
        format!(
            "failed to canonicalize settings file {}",
            config.settings_path.display()
        )
    })?;
    if settings_path.starts_with(&queue) {
        bail!("settings file must not be inside queue root");
    }
    match &settings.supplier().kind {
        SupplierKind::ManualFolder(manual) => check_source_separation(&queue, &manual.directory)?,
        SupplierKind::DigikamSqlite(digikam) => {
            for mapping in &digikam.path_mappings {
                check_source_separation(&queue, &mapping.mounted_root)?;
            }
        }
    }
    Ok(())
}

fn check_source_separation(queue: &std::path::Path, configured: &std::path::Path) -> Result<()> {
    let source = std::fs::canonicalize(configured).unwrap_or_else(|_| configured.to_path_buf());
    if source.starts_with(queue) || queue.starts_with(&source) {
        bail!("supplier source and queue root must be separate");
    }
    Ok(())
}

fn random_token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
