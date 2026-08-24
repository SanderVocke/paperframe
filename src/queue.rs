use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock, watch};

use crate::renderer::{FrameTarget, ImageOrientation, OrientationSource, RENDER_PROFILE_VERSION};

const STATE_VERSION: u32 = 2;
const STATE_FILE: &str = "queue-state.toml";
const OBJECTS_DIR: &str = "objects";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: String,
    /// Served derivative object; retained name keeps version-1 descriptor compatibility.
    pub object_name: String,
    pub display_name: String,
    pub digest: String,
    pub content_length: u64,
    #[serde(default = "default_media_type")]
    pub media_type: String,
    #[serde(default)]
    pub source_object_name: Option<String>,
    #[serde(default)]
    pub source_digest: Option<String>,
    #[serde(default)]
    pub source_content_length: Option<u64>,
    #[serde(default)]
    pub source_width: u32,
    #[serde(default)]
    pub source_height: u32,
    #[serde(default = "default_orientation")]
    pub orientation: ImageOrientation,
    #[serde(default = "default_orientation_source")]
    pub orientation_source: OrientationSource,
    #[serde(default)]
    pub frame_target: FrameTarget,
    #[serde(default)]
    pub renderer_profile: u32,
    pub supplier_id: Option<String>,
    pub source_key: Option<String>,
}

fn default_media_type() -> String {
    "image/jpeg".into()
}
fn default_orientation() -> ImageOrientation {
    ImageOrientation::Landscape
}
fn default_orientation_source() -> OrientationSource {
    OrientationSource::Inferred
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Descriptor {
    version: u32,
    generation: u64,
    entries: Vec<QueueEntry>,
    #[serde(default)]
    seen: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone, Debug)]
pub struct QueueSnapshot {
    pub generation: u64,
    pub entries: Vec<QueueEntry>,
}

#[derive(Clone, Debug)]
pub struct EnqueueRequest {
    pub display_name: String,
    pub bytes: Vec<u8>,
    pub supplier_id: Option<String>,
    pub source_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RenderedEnqueueRequest {
    pub display_name: String,
    pub source_bytes: Vec<u8>,
    pub derivative_bytes: Vec<u8>,
    pub media_type: String,
    pub source_width: u32,
    pub source_height: u32,
    pub orientation: ImageOrientation,
    pub frame_target: FrameTarget,
    pub supplier_id: Option<String>,
    pub source_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PreparedImage {
    pub entry: QueueEntry,
    pub bytes: Vec<u8>,
    pub modified: SystemTime,
}

#[derive(Clone, Debug)]
pub struct PreparedSource {
    pub entry: QueueEntry,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueEventReason {
    Startup,
    Enqueue,
    Reorder,
    Remove,
    Advance,
    BlockedAdvance,
    SupplierRefresh,
    SeenReset,
    Settings,
}

#[derive(Clone, Debug)]
pub struct QueueEvent {
    pub serial: u64,
    pub revision: u64,
    pub reason: QueueEventReason,
}

#[cfg(test)]
struct RerenderTestHook {
    started: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

pub struct QueueStore {
    root: PathBuf,
    objects: PathBuf,
    state_path: PathBuf,
    descriptor: RwLock<Descriptor>,
    mutation: Mutex<()>,
    events: watch::Sender<QueueEvent>,
    event_serial: AtomicU64,
    #[cfg(test)]
    rerender_test_hook: std::sync::Mutex<Option<RerenderTestHook>>,
}

impl QueueStore {
    pub async fn new(path: &Path, migrate_current: bool) -> Result<Arc<Self>> {
        let configured = path.to_path_buf();
        let (root, objects, descriptor) =
            tokio::task::spawn_blocking(move || initialize(&configured))
                .await
                .context("queue initialization worker stopped")??;
        let (events, _) = watch::channel(QueueEvent {
            serial: 0,
            revision: descriptor.generation,
            reason: QueueEventReason::Startup,
        });
        let store = Arc::new(Self {
            state_path: root.join(STATE_FILE),
            root,
            objects,
            descriptor: RwLock::new(descriptor),
            mutation: Mutex::new(()),
            events,
            event_serial: AtomicU64::new(0),
            #[cfg(test)]
            rerender_test_hook: std::sync::Mutex::new(None),
        });
        if migrate_current && store.snapshot().await.entries.is_empty() {
            let legacy = store.root.join("current.jpg");
            if legacy.is_file() {
                let bytes = tokio::fs::read(&legacy)
                    .await
                    .context("failed to read legacy current.jpg")?;
                let migrated = store
                    .enqueue(
                        "current.jpg".into(),
                        bytes,
                        Some("migration".into()),
                        Some("legacy-current".into()),
                    )
                    .await?;
                store.preserve_legacy_source(&migrated.id).await?;
                tokio::fs::remove_file(legacy)
                    .await
                    .context("failed to remove migrated current.jpg")?;
            }
        }
        store.notify(QueueEventReason::Startup, store.snapshot().await.generation);
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn snapshot(&self) -> QueueSnapshot {
        let state = self.descriptor.read().await;
        QueueSnapshot {
            generation: state.generation,
            entries: state.entries.clone(),
        }
    }

    pub async fn seen(&self, supplier_id: &str) -> BTreeSet<String> {
        self.descriptor
            .read()
            .await
            .seen
            .get(supplier_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn subscribe(&self) -> watch::Receiver<QueueEvent> {
        self.events.subscribe()
    }

    pub fn demand(&self, reason: QueueEventReason) {
        let revision = self
            .descriptor
            .try_read()
            .map(|state| state.generation)
            .unwrap_or(0);
        self.notify(reason, revision);
    }

    pub async fn enqueue(
        &self,
        display_name: String,
        bytes: Vec<u8>,
        supplier_id: Option<String>,
        source_key: Option<String>,
    ) -> Result<QueueEntry> {
        let mut added = self
            .enqueue_batch(
                None,
                None,
                vec![EnqueueRequest {
                    display_name,
                    bytes,
                    supplier_id,
                    source_key,
                }],
            )
            .await?;
        added.pop().context("enqueue was unexpectedly skipped")
    }

    pub async fn enqueue_batch(
        &self,
        expected_generation: Option<u64>,
        maximum_length: Option<usize>,
        mut requests: Vec<EnqueueRequest>,
    ) -> Result<Vec<QueueEntry>> {
        for request in &requests {
            validate_jpeg(&request.bytes)?;
        }
        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        if let Some(expected) = expected_generation {
            require_generation(&current, expected)?;
        }
        if let Some(maximum) = maximum_length {
            requests.truncate(maximum.saturating_sub(current.entries.len()));
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let entries = requests
            .iter()
            .map(|request| {
                let id = random_id();
                QueueEntry {
                    object_name: format!("{id}.jpg"),
                    id,
                    display_name: request.display_name.clone(),
                    digest: digest(&request.bytes),
                    content_length: request.bytes.len() as u64,
                    media_type: default_media_type(),
                    source_object_name: None,
                    source_digest: None,
                    source_content_length: None,
                    source_width: 0,
                    source_height: 0,
                    orientation: ImageOrientation::Landscape,
                    orientation_source: OrientationSource::Inferred,
                    frame_target: FrameTarget::Paper7,
                    renderer_profile: 0,
                    supplier_id: request.supplier_id.clone(),
                    source_key: request.source_key.clone(),
                }
            })
            .collect::<Vec<_>>();
        let mut next = current;
        next.generation = next.generation.saturating_add(1);
        for entry in &entries {
            next.entries.push(entry.clone());
            if let (Some(supplier), Some(source)) = (&entry.supplier_id, &entry.source_key) {
                next.seen
                    .entry(supplier.clone())
                    .or_default()
                    .insert(source.clone());
            }
        }
        let root = self.root.clone();
        let objects = self.objects.clone();
        let state_path = self.state_path.clone();
        let disk_entries = entries.clone();
        let payloads = requests.into_iter().map(|request| request.bytes).collect();
        let next_for_disk = next.clone();
        tokio::task::spawn_blocking(move || {
            publish_enqueue_batch(
                &root,
                &objects,
                &state_path,
                &disk_entries,
                payloads,
                &next_for_disk,
            )
        })
        .await
        .context("queue enqueue worker stopped")??;
        *self.descriptor.write().await = next.clone();
        self.notify(QueueEventReason::Enqueue, next.generation);
        Ok(entries)
    }

    pub async fn enqueue_rendered_batch(
        &self,
        expected_generation: Option<u64>,
        maximum_length: Option<usize>,
        mut requests: Vec<RenderedEnqueueRequest>,
    ) -> Result<Vec<QueueEntry>> {
        for request in &requests {
            crate::renderer::decode_source_named(
                &request.source_bytes,
                Some(&request.display_name),
            )?;
            validate_derivative(
                &request.derivative_bytes,
                &request.media_type,
                request.frame_target,
            )?;
        }
        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        if let Some(expected) = expected_generation {
            require_generation(&current, expected)?;
        }
        if let Some(maximum) = maximum_length {
            requests.truncate(maximum.saturating_sub(current.entries.len()));
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let entries = requests
            .iter()
            .map(|request| {
                let id = random_id();
                let extension = if request.media_type == "image/bmp" {
                    "bmp"
                } else {
                    "jpg"
                };
                QueueEntry {
                    object_name: format!("{id}.{extension}"),
                    source_object_name: Some(format!("{id}.source")),
                    id,
                    display_name: request.display_name.clone(),
                    digest: digest(&request.derivative_bytes),
                    content_length: request.derivative_bytes.len() as u64,
                    media_type: request.media_type.clone(),
                    source_digest: Some(digest(&request.source_bytes)),
                    source_content_length: Some(request.source_bytes.len() as u64),
                    source_width: request.source_width,
                    source_height: request.source_height,
                    orientation: request.orientation,
                    orientation_source: OrientationSource::Inferred,
                    frame_target: request.frame_target,
                    renderer_profile: RENDER_PROFILE_VERSION,
                    supplier_id: request.supplier_id.clone(),
                    source_key: request.source_key.clone(),
                }
            })
            .collect::<Vec<_>>();
        let mut next = current;
        next.version = STATE_VERSION;
        next.generation = next.generation.saturating_add(1);
        for entry in &entries {
            next.entries.push(entry.clone());
            if let (Some(supplier), Some(source)) = (&entry.supplier_id, &entry.source_key) {
                next.seen
                    .entry(supplier.clone())
                    .or_default()
                    .insert(source.clone());
            }
        }
        let root = self.root.clone();
        let objects = self.objects.clone();
        let state_path = self.state_path.clone();
        let disk_entries = entries.clone();
        let payloads = requests
            .into_iter()
            .map(|request| (request.source_bytes, request.derivative_bytes))
            .collect();
        let next_for_disk = next.clone();
        tokio::task::spawn_blocking(move || {
            publish_rendered_batch(
                &root,
                &objects,
                &state_path,
                &disk_entries,
                payloads,
                &next_for_disk,
            )
        })
        .await
        .context("rendered queue enqueue worker stopped")??;
        *self.descriptor.write().await = next.clone();
        self.notify(QueueEventReason::Enqueue, next.generation);
        Ok(entries)
    }

    async fn preserve_legacy_source(&self, id: &str) -> Result<()> {
        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        let index = current
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .context("queue entry not found")?;
        if current.entries[index].source_object_name.is_some() {
            return Ok(());
        }
        let mut next = current;
        let source_name = format!("{}.source", next.entries[index].id);
        let display_name = next.entries[index].display_name.clone();
        let from = self.objects.join(&next.entries[index].object_name);
        let to = self.objects.join(&source_name);
        let state_path = self.state_path.clone();
        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            fs::copy(from, &to).context("failed to preserve legacy current source")?;
            fs::read(to).context("failed to read preserved legacy source")
        })
        .await
        .context("legacy source worker stopped")??;
        let decoded = crate::renderer::decode_source_named(&bytes, Some(&display_name))?;
        let entry = &mut next.entries[index];
        entry.source_object_name = Some(source_name);
        entry.source_digest = Some(digest(&bytes));
        entry.source_content_length = Some(bytes.len() as u64);
        entry.source_width = decoded.pixels.width();
        entry.source_height = decoded.pixels.height();
        entry.orientation = decoded.orientation;
        let disk = next.clone();
        if let Err(error) =
            tokio::task::spawn_blocking(move || persist_descriptor(&state_path, &disk))
                .await
                .context("legacy source descriptor worker stopped")?
        {
            if let Some(name) = &next.entries[index].source_object_name {
                let _ = fs::remove_file(self.objects.join(name));
            }
            return Err(error);
        }
        *self.descriptor.write().await = next;
        Ok(())
    }

    pub async fn reorder(&self, expected: u64, ids: Vec<String>) -> Result<QueueSnapshot> {
        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        require_generation(&current, expected)?;
        if ids.len() != current.entries.len() {
            bail!("reorder must contain every queue entry exactly once");
        }
        let mut submitted = ids.clone();
        submitted.sort();
        submitted.dedup();
        let mut actual = current
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        actual.sort();
        if submitted != actual {
            bail!("reorder contains duplicate, missing, or unknown queue IDs");
        }
        let by_id = current
            .entries
            .iter()
            .cloned()
            .map(|entry| (entry.id.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let mut next = current;
        next.generation = next.generation.saturating_add(1);
        next.entries = ids.into_iter().map(|id| by_id[&id].clone()).collect();
        self.commit_descriptor(next.clone()).await?;
        self.notify(QueueEventReason::Reorder, next.generation);
        Ok(snapshot_of(&next))
    }

    pub async fn remove(&self, expected: u64, id: &str) -> Result<QueueSnapshot> {
        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        require_generation(&current, expected)?;
        let index = current
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .context("queue entry not found")?;
        let mut next = current;
        let removed = next.entries.remove(index);
        next.generation = next.generation.saturating_add(1);
        self.commit_descriptor(next.clone()).await?;
        let object = self.objects.join(&removed.object_name);
        let source = removed
            .source_object_name
            .map(|name| self.objects.join(name));
        let _ = tokio::task::spawn_blocking(move || {
            let _ = fs::remove_file(object);
            if let Some(source) = source {
                let _ = fs::remove_file(source);
            }
        })
        .await;
        self.notify(QueueEventReason::Remove, next.generation);
        Ok(snapshot_of(&next))
    }

    pub async fn advance(&self) -> Result<bool> {
        self.advance_internal(None, None).await
    }

    pub async fn advance_at_generation(&self, expected: u64) -> Result<bool> {
        self.advance_internal(Some(expected), None).await
    }

    pub async fn advance_if_current(&self, id: &str) -> Result<bool> {
        self.advance_internal(None, Some(id)).await
    }

    async fn advance_internal(&self, expected: Option<u64>, id: Option<&str>) -> Result<bool> {
        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        if let Some(expected) = expected {
            require_generation(&current, expected)?;
        }
        match id {
            Some(id) if current.entries.first().map(|entry| entry.id.as_str()) != Some(id) => {
                return Ok(false);
            }
            _ => {}
        }
        if current.entries.len() <= 1 {
            self.notify(QueueEventReason::BlockedAdvance, current.generation);
            return Ok(false);
        }
        let mut next = current;
        let removed = next.entries.remove(0);
        next.generation = next.generation.saturating_add(1);
        self.commit_descriptor(next.clone()).await?;
        let object = self.objects.join(&removed.object_name);
        let source = removed
            .source_object_name
            .map(|name| self.objects.join(name));
        let _ = tokio::task::spawn_blocking(move || {
            let _ = fs::remove_file(object);
            if let Some(source) = source {
                let _ = fs::remove_file(source);
            }
        })
        .await;
        self.notify(QueueEventReason::Advance, next.generation);
        Ok(true)
    }

    pub async fn rerender_all(
        &self,
        target: FrameTarget,
        output_mode: crate::renderer::OutputMode,
    ) -> Result<QueueSnapshot> {
        // Render from an immutable descriptor snapshot without holding the queue mutation lock.
        // Publication below uses the generation as an optimistic concurrency check.
        let base = self.descriptor.read().await.clone();
        if base.entries.is_empty() {
            return Ok(snapshot_of(&base));
        }
        let base_generation = base.generation;
        #[cfg(test)]
        {
            let hook = {
                let mut guard = self.rerender_test_hook.lock().unwrap();
                guard.take()
            };
            if let Some(hook) = hook {
                hook.started.send(()).unwrap();
                hook.resume.await.context("rerender test hook canceled")?;
            }
        }
        let objects = self.objects.clone();
        let entries = base.entries;
        let rendered = tokio::task::spawn_blocking(move || -> Result<Vec<_>> {
            entries
                .into_iter()
                .map(|entry| {
                    let source_name = entry
                        .source_object_name
                        .clone()
                        .unwrap_or_else(|| entry.object_name.clone());
                    let source = fs::read(objects.join(source_name))
                        .context("failed to read queue source for rerender")?;
                    let decoded =
                        crate::renderer::decode_source_named(&source, Some(&entry.display_name))?;
                    let output = crate::renderer::render_with_original(
                        &decoded,
                        &source,
                        target,
                        output_mode,
                        entry.orientation,
                    )?;
                    Ok((entry.id, output))
                })
                .collect()
        })
        .await
        .context("queue rerender worker stopped")??;

        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        require_generation(&current, base_generation)?;
        if current.entries.len() != rendered.len()
            || current
                .entries
                .iter()
                .zip(&rendered)
                .any(|(entry, (id, _))| entry.id != *id)
        {
            bail!("queue changed while images were being rendered");
        }
        let mut next = current;
        next.version = STATE_VERSION;
        next.generation = next.generation.saturating_add(1);
        let mut replacements = Vec::new();
        for (index, (_, output)) in rendered.into_iter().enumerate() {
            let old_name = next.entries[index].object_name.clone();
            let name = format!(
                "{}.render-{}.{}",
                next.entries[index].id, next.generation, output.extension
            );
            next.entries[index].object_name = name.clone();
            next.entries[index].digest = digest(&output.bytes);
            next.entries[index].content_length = output.bytes.len() as u64;
            next.entries[index].media_type = output.media_type.into();
            next.entries[index].frame_target = target;
            next.entries[index].renderer_profile = RENDER_PROFILE_VERSION;
            replacements.push((old_name, name, output.bytes));
        }
        let objects = self.objects.clone();
        let state_path = self.state_path.clone();
        let disk = next.clone();
        tokio::task::spawn_blocking(move || {
            publish_rerender(&objects, &state_path, replacements, &disk)
        })
        .await
        .context("queue rerender publication worker stopped")??;
        *self.descriptor.write().await = next.clone();
        self.notify(QueueEventReason::Settings, next.generation);
        Ok(snapshot_of(&next))
    }

    pub async fn set_orientation(
        &self,
        expected: u64,
        id: &str,
        orientation: ImageOrientation,
        target: FrameTarget,
        output_mode: crate::renderer::OutputMode,
    ) -> Result<QueueSnapshot> {
        // Decode and render outside the mutation lock, then reject publication if the queue moved.
        let base = self.descriptor.read().await.clone();
        require_generation(&base, expected)?;
        let entry = base
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .context("queue entry not found")?;
        let source_name = entry
            .source_object_name
            .clone()
            .unwrap_or_else(|| entry.object_name.clone());
        let source_path = self.objects.join(source_name);
        let id = entry.id.clone();
        let display_name = entry.display_name;
        let rendered = tokio::task::spawn_blocking(move || -> Result<_> {
            let source = fs::read(source_path).context("failed to read queue source")?;
            let decoded = crate::renderer::decode_source_named(&source, Some(&display_name))?;
            crate::renderer::render_with_original(
                &decoded,
                &source,
                target,
                output_mode,
                orientation,
            )
        })
        .await
        .context("orientation render worker stopped")??;

        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        require_generation(&current, expected)?;
        let index = current
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .context("queue entry not found")?;
        let mut next = current;
        next.version = STATE_VERSION;
        next.generation = next.generation.saturating_add(1);
        let extension = rendered.extension;
        let new_name = format!("{id}.{extension}");
        let old_name = next.entries[index].object_name.clone();
        next.entries[index].object_name = new_name.clone();
        next.entries[index].digest = digest(&rendered.bytes);
        next.entries[index].content_length = rendered.bytes.len() as u64;
        next.entries[index].media_type = rendered.media_type.into();
        next.entries[index].orientation = orientation;
        next.entries[index].orientation_source = OrientationSource::Override;
        next.entries[index].frame_target = target;
        next.entries[index].renderer_profile = RENDER_PROFILE_VERSION;
        let objects = self.objects.clone();
        let state_path = self.state_path.clone();
        let disk = next.clone();
        tokio::task::spawn_blocking(move || {
            replace_derivative(
                &objects,
                &state_path,
                &old_name,
                &new_name,
                &rendered.bytes,
                &disk,
            )
        })
        .await
        .context("orientation publication worker stopped")??;
        *self.descriptor.write().await = next.clone();
        self.notify(QueueEventReason::Settings, next.generation);
        Ok(snapshot_of(&next))
    }

    pub async fn reset_seen(&self, expected: u64, supplier_id: &str) -> Result<QueueSnapshot> {
        let _guard = self.mutation.lock().await;
        let current = self.descriptor.read().await.clone();
        require_generation(&current, expected)?;
        let mut next = current;
        next.seen.remove(supplier_id);
        next.generation = next.generation.saturating_add(1);
        self.commit_descriptor(next.clone()).await?;
        self.notify(QueueEventReason::SeenReset, next.generation);
        Ok(snapshot_of(&next))
    }

    pub async fn current(&self) -> Result<Option<PreparedImage>> {
        let entry = self.descriptor.read().await.entries.first().cloned();
        match entry {
            Some(entry) => self.prepare(&entry.id).await.map(Some),
            None => Ok(None),
        }
    }

    pub async fn prepare(&self, id: &str) -> Result<PreparedImage> {
        let entry = self
            .descriptor
            .read()
            .await
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .context("queue entry not found")?;
        let path = self.objects.join(&entry.object_name);
        let expected_digest = entry.digest.clone();
        let media_type = entry.media_type.clone();
        let frame_target = entry.frame_target;
        let (bytes, modified) = tokio::task::spawn_blocking(move || -> Result<_> {
            let metadata = fs::symlink_metadata(&path).context("failed to inspect queue object")?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("queue object is not a regular file");
            }
            let bytes = fs::read(&path).context("failed to read queue object")?;
            validate_derivative(&bytes, &media_type, frame_target)?;
            if digest(&bytes) != expected_digest {
                bail!("queue object digest does not match descriptor");
            }
            Ok((
                bytes,
                metadata
                    .modified()
                    .context("queue object has no modification time")?,
            ))
        })
        .await
        .context("queue read worker stopped")??;
        Ok(PreparedImage {
            entry,
            bytes,
            modified,
        })
    }

    pub async fn prepare_source(&self, id: &str) -> Result<PreparedSource> {
        let entry = self
            .descriptor
            .read()
            .await
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .context("queue entry not found")?;
        let name = entry
            .source_object_name
            .clone()
            .unwrap_or_else(|| entry.object_name.clone());
        let expected = entry
            .source_digest
            .clone()
            .unwrap_or_else(|| entry.digest.clone());
        let path = self.objects.join(name);
        let display_name = entry.display_name.clone();
        let bytes = tokio::task::spawn_blocking(move || -> Result<_> {
            let metadata = fs::symlink_metadata(&path).context("failed to inspect queue source")?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("queue source is not a regular file");
            }
            let bytes = fs::read(path).context("failed to read queue source")?;
            crate::renderer::decode_source_named(&bytes, Some(&display_name))?;
            if digest(&bytes) != expected {
                bail!("queue source digest does not match descriptor");
            }
            Ok(bytes)
        })
        .await
        .context("queue source worker stopped")??;
        Ok(PreparedSource { entry, bytes })
    }

    async fn commit_descriptor(&self, next: Descriptor) -> Result<()> {
        let path = self.state_path.clone();
        let disk = next.clone();
        tokio::task::spawn_blocking(move || persist_descriptor(&path, &disk))
            .await
            .context("queue state worker stopped")??;
        *self.descriptor.write().await = next;
        Ok(())
    }

    fn notify(&self, reason: QueueEventReason, revision: u64) {
        let serial = self
            .event_serial
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.events.send_replace(QueueEvent {
            serial,
            revision,
            reason,
        });
    }
}

fn initialize(configured: &Path) -> Result<(PathBuf, PathBuf, Descriptor)> {
    fs::create_dir_all(configured)
        .with_context(|| format!("failed to create queue root {}", configured.display()))?;
    let root = fs::canonicalize(configured).context("failed to canonicalize queue root")?;
    let objects = root.join(OBJECTS_DIR);
    fs::create_dir_all(&objects).context("failed to create queue objects directory")?;
    for entry in fs::read_dir(&root).context("failed to inspect queue root")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".queue-state.") && name.ends_with(".tmp") {
            fs::remove_file(entry.path())?;
        }
    }
    let state_path = root.join(STATE_FILE);
    let mut descriptor = if state_path.exists() {
        let value: Descriptor = toml::from_str(
            &fs::read_to_string(&state_path).context("failed to read queue descriptor")?,
        )
        .context("failed to parse queue descriptor")?;
        if value.version == 0 || value.version > STATE_VERSION {
            bail!("unsupported queue descriptor version {}", value.version);
        }
        value
    } else {
        let value = Descriptor {
            version: STATE_VERSION,
            generation: 1,
            entries: Vec::new(),
            seen: BTreeMap::new(),
        };
        persist_descriptor(&state_path, &value)?;
        value
    };
    for entry in &descriptor.entries {
        validate_object_name(&entry.object_name)?;
        if let Some(source) = &entry.source_object_name {
            validate_object_name(source)?;
        }
    }
    if descriptor.version < STATE_VERSION {
        migrate_v1_descriptor(&objects, &mut descriptor)?;
        persist_descriptor(&state_path, &descriptor)?;
    }
    let expected = descriptor
        .entries
        .iter()
        .flat_map(|entry| {
            std::iter::once(entry.object_name.clone()).chain(entry.source_object_name.clone())
        })
        .collect::<HashSet<_>>();
    for entry in &descriptor.entries {
        let path = objects.join(&entry.object_name);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("queue object is missing: {}", entry.object_name))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("queue object is unsafe: {}", entry.object_name);
        }
        if let Some(source) = &entry.source_object_name {
            let source_path = objects.join(source);
            let metadata = fs::symlink_metadata(&source_path)
                .with_context(|| format!("queue source is missing: {source}"))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("queue source is unsafe: {source}");
            }
        }
    }
    for entry in fs::read_dir(&objects).context("failed to inspect queue objects")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".object.") && name.ends_with(".tmp") || !expected.contains(&name) {
            fs::remove_file(entry.path()).context("failed to remove orphan queue object")?;
        }
    }
    Ok((root, objects, descriptor))
}

fn validate_object_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(component)), None)
            if !component.is_empty() && component == std::ffi::OsStr::new(name) =>
        {
            Ok(())
        }
        _ => bail!("queue object name is unsafe: {name}"),
    }
}

fn migrate_v1_descriptor(objects: &Path, descriptor: &mut Descriptor) -> Result<()> {
    let mut created = Vec::new();
    let result = (|| -> Result<()> {
        for entry in &mut descriptor.entries {
            let derivative = objects.join(&entry.object_name);
            let source_name = format!("{}.source", entry.id);
            let source = objects.join(&source_name);
            fs::copy(&derivative, &source).context("failed to preserve legacy queue source")?;
            created.push(source);
            let bytes = fs::read(&derivative)?;
            let decoded = crate::renderer::decode_source_named(&bytes, Some(&entry.display_name))
                .context("legacy queue image is not a decodable raster source")?;
            entry.source_object_name = Some(source_name);
            entry.source_digest = Some(digest(&bytes));
            entry.source_content_length = Some(bytes.len() as u64);
            entry.source_width = decoded.pixels.width();
            entry.source_height = decoded.pixels.height();
            entry.orientation = decoded.orientation;
            entry.orientation_source = OrientationSource::Inferred;
            entry.media_type = "image/jpeg".into();
        }
        sync_directory(objects)
    })();
    if result.is_err() {
        for path in created {
            let _ = fs::remove_file(path);
        }
        return result;
    }
    descriptor.version = STATE_VERSION;
    Ok(())
}

fn publish_enqueue_batch(
    root: &Path,
    objects: &Path,
    state_path: &Path,
    entries: &[QueueEntry],
    payloads: Vec<Vec<u8>>,
    descriptor: &Descriptor,
) -> Result<()> {
    let mut temporaries = Vec::new();
    let mut published = Vec::new();
    let result = (|| -> Result<()> {
        for (entry, bytes) in entries.iter().zip(payloads) {
            let temporary = objects.join(format!(
                ".object.{}.{}.tmp",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .context("failed to create queue object temporary")?;
            file.write_all(&bytes)
                .context("failed to write queue object")?;
            file.sync_all().context("failed to sync queue object")?;
            temporaries.push((temporary, objects.join(&entry.object_name)));
        }
        for (temporary, destination) in &temporaries {
            fs::rename(temporary, destination).context("failed to publish queue object")?;
            published.push(destination.clone());
        }
        sync_directory(objects)?;
        persist_descriptor(state_path, descriptor)?;
        let _ = sync_directory(root);
        Ok(())
    })();
    if result.is_err() {
        for (temporary, _) in &temporaries {
            let _ = fs::remove_file(temporary);
        }
        for destination in &published {
            let _ = fs::remove_file(destination);
        }
        let _ = sync_directory(objects);
    }
    result
}

fn publish_rendered_batch(
    root: &Path,
    objects: &Path,
    state_path: &Path,
    entries: &[QueueEntry],
    payloads: Vec<(Vec<u8>, Vec<u8>)>,
    descriptor: &Descriptor,
) -> Result<()> {
    let mut temporaries = Vec::new();
    let mut published = Vec::new();
    let result = (|| -> Result<()> {
        for (entry, (source, derivative)) in entries.iter().zip(payloads) {
            let source_name = entry
                .source_object_name
                .as_ref()
                .context("missing source object name")?;
            for (suffix, destination, bytes) in [
                ("source", objects.join(source_name), source),
                ("derivative", objects.join(&entry.object_name), derivative),
            ] {
                let temporary = objects.join(format!(
                    ".object.{}.{}.{}.tmp",
                    std::process::id(),
                    TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
                    suffix
                ));
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .context("failed to create rendered queue temporary")?;
                file.write_all(&bytes)
                    .context("failed to write rendered queue object")?;
                file.sync_all()
                    .context("failed to sync rendered queue object")?;
                temporaries.push((temporary, destination));
            }
        }
        for (temporary, destination) in &temporaries {
            fs::rename(temporary, destination)
                .context("failed to publish rendered queue object")?;
            published.push(destination.clone());
        }
        sync_directory(objects)?;
        persist_descriptor(state_path, descriptor)?;
        let _ = sync_directory(root);
        Ok(())
    })();
    if result.is_err() {
        for (temporary, _) in &temporaries {
            let _ = fs::remove_file(temporary);
        }
        for destination in &published {
            let _ = fs::remove_file(destination);
        }
        let _ = sync_directory(objects);
    }
    result
}

fn publish_rerender(
    objects: &Path,
    state_path: &Path,
    replacements: Vec<(String, String, Vec<u8>)>,
    descriptor: &Descriptor,
) -> Result<()> {
    let mut temporaries = Vec::new();
    let mut published = Vec::new();
    let result = (|| -> Result<()> {
        for (_, new_name, bytes) in &replacements {
            let temporary = objects.join(format!(
                ".object.{}.{}.tmp",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            temporaries.push((temporary, objects.join(new_name)));
        }
        for (temporary, destination) in &temporaries {
            fs::rename(temporary, destination)?;
            published.push(destination.clone());
        }
        sync_directory(objects)?;
        persist_descriptor(state_path, descriptor)?;
        for (old_name, _, _) in &replacements {
            let _ = fs::remove_file(objects.join(old_name));
        }
        let _ = sync_directory(objects);
        Ok(())
    })();
    if result.is_err() {
        for (temporary, _) in &temporaries {
            let _ = fs::remove_file(temporary);
        }
        for destination in &published {
            let _ = fs::remove_file(destination);
        }
        let _ = sync_directory(objects);
    }
    result
}

fn replace_derivative(
    objects: &Path,
    state_path: &Path,
    old_name: &str,
    new_name: &str,
    bytes: &[u8],
    descriptor: &Descriptor,
) -> Result<()> {
    let temporary = objects.join(format!(
        ".object.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let backup = objects.join(format!(
        ".object.{}.{}.backup.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let old_path = objects.join(old_name);
    let new_path = objects.join(new_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let result = (|| -> Result<()> {
        fs::rename(&old_path, &backup).context("failed to stage old derivative")?;
        if new_path != old_path && new_path.exists() {
            bail!("replacement derivative destination already exists");
        }
        fs::rename(&temporary, &new_path).context("failed to publish replacement derivative")?;
        sync_directory(objects)?;
        persist_descriptor(state_path, descriptor)?;
        let _ = fs::remove_file(&backup);
        let _ = sync_directory(objects);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        if new_path.exists() {
            let _ = fs::remove_file(&new_path);
        }
        if backup.exists() {
            let _ = fs::rename(&backup, &old_path);
        }
        let _ = sync_directory(objects);
    }
    result
}

fn persist_descriptor(path: &Path, descriptor: &Descriptor) -> Result<()> {
    let parent = path.parent().unwrap();
    let temporary = parent.join(format!(
        ".queue-state.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let serialized =
        toml::to_string_pretty(descriptor).context("failed to serialize queue descriptor")?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("failed to create queue descriptor temporary")?;
        file.write_all(serialized.as_bytes())
            .context("failed to write queue descriptor")?;
        file.sync_all().context("failed to sync queue descriptor")?;
        fs::rename(&temporary, path).context("failed to publish queue descriptor")?;
        let _ = sync_directory(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn validate_jpeg(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 5 || !bytes.starts_with(&[0xff, 0xd8, 0xff]) || !bytes.ends_with(&[0xff, 0xd9])
    {
        bail!("image is not a JPEG");
    }
    Ok(())
}
fn validate_derivative(bytes: &[u8], media_type: &str, target: FrameTarget) -> Result<()> {
    match media_type {
        "image/jpeg" => validate_jpeg(bytes),
        "image/bmp" => crate::renderer::validate_bmp4(bytes, target),
        _ => bail!("unsupported derivative media type"),
    }
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn random_id() -> String {
    rand::random::<[u8; 16]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn require_generation(state: &Descriptor, expected: u64) -> Result<()> {
    if state.generation != expected {
        bail!(
            "queue changed concurrently; current generation is {}",
            state.generation
        );
    }
    Ok(())
}
fn snapshot_of(state: &Descriptor) -> QueueSnapshot {
    QueueSnapshot {
        generation: state.generation,
        entries: state.entries.clone(),
    }
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
            .context("failed to sync queue directory")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    const A: &[u8] = include_bytes!("../tests/fixtures/images/a.jpg");
    const B: &[u8] = include_bytes!("../tests/fixtures/images/b.jpg");

    #[tokio::test]
    async fn rendered_enqueue_preserves_source_serves_bmp_and_rerenders_orientation() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let decoded = crate::renderer::decode_source(A).unwrap();
        let rendered = crate::renderer::render(
            &decoded,
            FrameTarget::Paper7,
            crate::renderer::OutputMode::ServerDitherBmp,
            decoded.orientation,
        )
        .unwrap();
        let entries = queue
            .enqueue_rendered_batch(
                None,
                None,
                vec![RenderedEnqueueRequest {
                    display_name: "a.jpg".into(),
                    source_bytes: A.to_vec(),
                    derivative_bytes: rendered.bytes,
                    media_type: rendered.media_type.into(),
                    source_width: decoded.pixels.width(),
                    source_height: decoded.pixels.height(),
                    orientation: decoded.orientation,
                    frame_target: FrameTarget::Paper7,
                    supplier_id: Some("s".into()),
                    source_key: Some("a".into()),
                }],
            )
            .await
            .unwrap();
        let current = queue.current().await.unwrap().unwrap();
        assert_eq!(current.entry.media_type, "image/bmp");
        crate::renderer::validate_bmp4(&current.bytes, FrameTarget::Paper7).unwrap();
        assert_eq!(queue.prepare_source(&entries[0].id).await.unwrap().bytes, A);
        let generation = queue.snapshot().await.generation;
        queue
            .set_orientation(
                generation,
                &entries[0].id,
                ImageOrientation::Portrait,
                FrameTarget::Paper7,
                crate::renderer::OutputMode::ServerDitherBmp,
            )
            .await
            .unwrap();
        let changed = queue.snapshot().await.entries.remove(0);
        assert_eq!(changed.orientation, ImageOrientation::Portrait);
        assert_eq!(changed.orientation_source, OrientationSource::Override);
        assert_eq!(queue.prepare_source(&changed.id).await.unwrap().bytes, A);
        queue
            .rerender_all(
                FrameTarget::PaperL,
                crate::renderer::OutputMode::ServerDitherBmp,
            )
            .await
            .unwrap();
        let paper_l = queue.current().await.unwrap().unwrap();
        assert_eq!(paper_l.entry.frame_target, FrameTarget::PaperL);
        crate::renderer::validate_bmp4(&paper_l.bytes, FrameTarget::PaperL).unwrap();
    }

    #[tokio::test]
    async fn persists_order_current_seen_and_one_entry_hold() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let a = queue
            .enqueue("a".into(), A.to_vec(), Some("s".into()), Some("a".into()))
            .await
            .unwrap();
        let b = queue
            .enqueue("b".into(), B.to_vec(), Some("s".into()), Some("b".into()))
            .await
            .unwrap();
        let generation = queue.snapshot().await.generation;
        queue
            .reorder(generation, vec![b.id.clone(), a.id.clone()])
            .await
            .unwrap();
        assert_eq!(queue.current().await.unwrap().unwrap().bytes, B);
        assert!(queue.advance_if_current(&b.id).await.unwrap());
        assert!(!queue.advance_if_current(&a.id).await.unwrap());
        assert_eq!(queue.current().await.unwrap().unwrap().bytes, A);
        drop(queue);
        let reloaded = QueueStore::new(dir.path(), false).await.unwrap();
        assert_eq!(reloaded.current().await.unwrap().unwrap().bytes, A);
        assert!(reloaded.seen("s").await.contains("b"));
    }

    #[tokio::test]
    async fn whole_queue_directory_backup_restores_source_derivative_and_order() {
        let original = tempdir().unwrap();
        let queue = QueueStore::new(original.path(), false).await.unwrap();
        let a = queue
            .enqueue(
                "a.jpg".into(),
                A.to_vec(),
                Some("s".into()),
                Some("a".into()),
            )
            .await
            .unwrap();
        queue.preserve_legacy_source(&a.id).await.unwrap();
        let b = queue
            .enqueue(
                "b.jpg".into(),
                B.to_vec(),
                Some("s".into()),
                Some("b".into()),
            )
            .await
            .unwrap();
        queue.preserve_legacy_source(&b.id).await.unwrap();
        let before = queue.snapshot().await;
        drop(queue);

        let backup = tempdir().unwrap();
        fs::copy(
            original.path().join(STATE_FILE),
            backup.path().join(STATE_FILE),
        )
        .unwrap();
        fs::create_dir(backup.path().join(OBJECTS_DIR)).unwrap();
        for entry in fs::read_dir(original.path().join(OBJECTS_DIR)).unwrap() {
            let entry = entry.unwrap();
            fs::copy(
                entry.path(),
                backup.path().join(OBJECTS_DIR).join(entry.file_name()),
            )
            .unwrap();
        }
        let restored = QueueStore::new(backup.path(), false).await.unwrap();
        assert_eq!(restored.snapshot().await.entries, before.entries);
        assert_eq!(restored.current().await.unwrap().unwrap().bytes, A);
        assert_eq!(
            restored
                .prepare_source(&before.entries[0].id)
                .await
                .unwrap()
                .bytes,
            A
        );
    }

    #[tokio::test]
    async fn removing_current_promotes_next_and_sole_remove_empties() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let a = queue
            .enqueue("a".into(), A.to_vec(), None, None)
            .await
            .unwrap();
        let b = queue
            .enqueue("b".into(), B.to_vec(), None, None)
            .await
            .unwrap();
        let snapshot = queue
            .remove(queue.snapshot().await.generation, &a.id)
            .await
            .unwrap();
        assert_eq!(snapshot.entries[0].id, b.id);
        queue.remove(snapshot.generation, &b.id).await.unwrap();
        assert!(queue.current().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn migrates_legacy_current_and_detects_missing_or_corrupt_state() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("current.jpg"), A).unwrap();
        let queue = QueueStore::new(dir.path(), true).await.unwrap();
        assert_eq!(queue.current().await.unwrap().unwrap().bytes, A);
        assert!(!dir.path().join("current.jpg").exists());
        let object = queue.snapshot().await.entries[0].object_name.clone();
        drop(queue);
        fs::remove_file(dir.path().join(OBJECTS_DIR).join(object)).unwrap();
        assert!(QueueStore::new(dir.path(), false).await.is_err());

        let corrupt = tempdir().unwrap();
        fs::write(corrupt.path().join(STATE_FILE), "not valid toml = [").unwrap();
        assert!(QueueStore::new(corrupt.path(), false).await.is_err());
    }

    #[tokio::test]
    async fn version_one_descriptor_migration_preserves_a_source_copy() {
        let dir = tempdir().unwrap();
        let objects = dir.path().join(OBJECTS_DIR);
        fs::create_dir(&objects).unwrap();
        fs::write(objects.join("legacy.jpg"), A).unwrap();
        fs::write(
            dir.path().join(STATE_FILE),
            format!(
                "version = 1\ngeneration = 7\n\n[[entries]]\nid = 'legacy'\nobject_name = 'legacy.jpg'\ndisplay_name = 'legacy.jpg'\ndigest = '{}'\ncontent_length = {}\n",
                digest(A),
                A.len()
            ),
        )
        .unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let entry = queue.snapshot().await.entries.remove(0);
        assert_eq!(entry.source_object_name.as_deref(), Some("legacy.source"));
        assert_eq!(queue.prepare_source(&entry.id).await.unwrap().bytes, A);
        let persisted = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(persisted.contains("version = 2"));
    }

    #[tokio::test]
    async fn descriptor_object_names_cannot_escape_queue_storage() {
        let parent = tempdir().unwrap();
        let queue_root = parent.path().join("queue");
        fs::create_dir(&queue_root).unwrap();
        let outside = parent.path().join("outside.jpg");
        fs::write(&outside, A).unwrap();
        fs::write(
            queue_root.join(STATE_FILE),
            format!(
                "version = 2\ngeneration = 1\n\n[[entries]]\nid = 'escape'\nobject_name = '../outside.jpg'\ndisplay_name = 'outside.jpg'\ndigest = '{}'\ncontent_length = {}\n",
                digest(A),
                A.len()
            ),
        )
        .unwrap();
        assert!(QueueStore::new(&queue_root, false).await.is_err());
        assert_eq!(fs::read(outside).unwrap(), A);
    }

    #[tokio::test]
    async fn events_report_committed_changes_and_blocked_demand() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let mut events = queue.subscribe();
        queue
            .enqueue("a".into(), A.to_vec(), None, None)
            .await
            .unwrap();
        events.changed().await.unwrap();
        assert_eq!(events.borrow().reason, QueueEventReason::Enqueue);
        queue.advance().await.unwrap();
        events.changed().await.unwrap();
        assert_eq!(events.borrow().reason, QueueEventReason::BlockedAdvance);
    }

    #[tokio::test]
    async fn batch_enqueue_is_atomic_bounded_and_generation_checked() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let generation = queue.snapshot().await.generation;
        let invalid = queue
            .enqueue_batch(
                Some(generation),
                None,
                vec![
                    EnqueueRequest {
                        display_name: "a".into(),
                        bytes: A.to_vec(),
                        supplier_id: None,
                        source_key: None,
                    },
                    EnqueueRequest {
                        display_name: "bad".into(),
                        bytes: b"bad".to_vec(),
                        supplier_id: None,
                        source_key: None,
                    },
                ],
            )
            .await;
        assert!(invalid.is_err());
        assert!(queue.snapshot().await.entries.is_empty());
        let added = queue
            .enqueue_batch(
                Some(generation),
                Some(1),
                vec![
                    EnqueueRequest {
                        display_name: "a".into(),
                        bytes: A.to_vec(),
                        supplier_id: None,
                        source_key: None,
                    },
                    EnqueueRequest {
                        display_name: "b".into(),
                        bytes: B.to_vec(),
                        supplier_id: None,
                        source_key: None,
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(added.len(), 1);
        assert!(
            queue
                .enqueue_batch(Some(generation), None, Vec::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn descriptor_persistence_failure_keeps_live_queue_unchanged() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        fs::remove_file(dir.path().join(STATE_FILE)).unwrap();
        fs::create_dir(dir.path().join(STATE_FILE)).unwrap();
        let result = queue.enqueue("a".into(), A.to_vec(), None, None).await;
        assert!(result.is_err());
        assert!(queue.snapshot().await.entries.is_empty());
        assert_eq!(
            fs::read_dir(dir.path().join(OBJECTS_DIR)).unwrap().count(),
            0
        );
    }

    #[tokio::test]
    async fn cpu_render_phase_does_not_hold_mutation_lock_and_rejects_stale_publication() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        queue
            .enqueue("a.jpg".into(), A.to_vec(), None, None)
            .await
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        *queue.rerender_test_hook.lock().unwrap() = Some(RerenderTestHook {
            started: started_tx,
            resume: resume_rx,
        });
        let rerender_queue = queue.clone();
        let rerender = tokio::spawn(async move {
            rerender_queue
                .rerender_all(
                    FrameTarget::PaperL,
                    crate::renderer::OutputMode::ServerDitherBmp,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
            .await
            .unwrap()
            .unwrap();
        let snapshot = queue.snapshot().await;
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            queue.remove(snapshot.generation, &snapshot.entries[0].id),
        )
        .await
        .expect("queue mutation was blocked by CPU rendering")
        .unwrap();
        resume_tx.send(()).unwrap();
        assert!(rerender.await.unwrap().is_err());
        assert!(queue.snapshot().await.entries.is_empty());
    }

    #[tokio::test]
    async fn rerender_publication_failure_keeps_old_derivative_and_descriptor_live() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let decoded = crate::renderer::decode_source(A).unwrap();
        let rendered = crate::renderer::render(
            &decoded,
            FrameTarget::Paper7,
            crate::renderer::OutputMode::DirectJpeg,
            decoded.orientation,
        )
        .unwrap();
        queue
            .enqueue_rendered_batch(
                None,
                None,
                vec![RenderedEnqueueRequest {
                    display_name: "a.jpg".into(),
                    source_bytes: A.to_vec(),
                    derivative_bytes: rendered.bytes,
                    media_type: rendered.media_type.into(),
                    source_width: decoded.pixels.width(),
                    source_height: decoded.pixels.height(),
                    orientation: decoded.orientation,
                    frame_target: FrameTarget::Paper7,
                    supplier_id: None,
                    source_key: None,
                }],
            )
            .await
            .unwrap();
        let before = queue.current().await.unwrap().unwrap().bytes;
        fs::remove_file(dir.path().join(STATE_FILE)).unwrap();
        fs::create_dir(dir.path().join(STATE_FILE)).unwrap();
        assert!(
            queue
                .rerender_all(
                    FrameTarget::Paper7,
                    crate::renderer::OutputMode::ServerDitherBmp,
                )
                .await
                .is_err()
        );
        assert_eq!(queue.current().await.unwrap().unwrap().bytes, before);
        assert_eq!(queue.snapshot().await.entries[0].media_type, "image/jpeg");
        assert_eq!(
            fs::read_dir(dir.path().join(OBJECTS_DIR)).unwrap().count(),
            2
        );
    }

    #[tokio::test]
    async fn rejects_stale_reorder_and_recovers_orphans() {
        let dir = tempdir().unwrap();
        let queue = QueueStore::new(dir.path(), false).await.unwrap();
        let a = queue
            .enqueue("a".into(), A.to_vec(), None, None)
            .await
            .unwrap();
        assert!(queue.reorder(1, vec![a.id]).await.is_err());
        fs::write(dir.path().join(OBJECTS_DIR).join("orphan.jpg"), A).unwrap();
        drop(queue);
        QueueStore::new(dir.path(), false).await.unwrap();
        assert!(!dir.path().join(OBJECTS_DIR).join("orphan.jpg").exists());
    }
}
