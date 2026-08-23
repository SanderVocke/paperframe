use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
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

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "queue-state.toml";
const OBJECTS_DIR: &str = "objects";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: String,
    pub object_name: String,
    pub display_name: String,
    pub digest: String,
    pub content_length: u64,
    pub supplier_id: Option<String>,
    pub source_key: Option<String>,
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
pub struct PreparedImage {
    pub entry: QueueEntry,
    pub bytes: Vec<u8>,
    pub modified: SystemTime,
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

pub struct QueueStore {
    root: PathBuf,
    objects: PathBuf,
    state_path: PathBuf,
    descriptor: RwLock<Descriptor>,
    mutation: Mutex<()>,
    events: watch::Sender<QueueEvent>,
    event_serial: AtomicU64,
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
        });
        if migrate_current && store.snapshot().await.entries.is_empty() {
            let legacy = store.root.join("current.jpg");
            if legacy.is_file() {
                let bytes = tokio::fs::read(&legacy)
                    .await
                    .context("failed to read legacy current.jpg")?;
                store
                    .enqueue(
                        "current.jpg".into(),
                        bytes,
                        Some("migration".into()),
                        Some("legacy-current".into()),
                    )
                    .await?;
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
        let object = self.objects.join(removed.object_name);
        let _ = tokio::task::spawn_blocking(move || fs::remove_file(object)).await;
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
        if let Some(id) = id
            && current.entries.first().map(|entry| entry.id.as_str()) != Some(id)
        {
            return Ok(false);
        }
        if current.entries.len() <= 1 {
            self.notify(QueueEventReason::BlockedAdvance, current.generation);
            return Ok(false);
        }
        let mut next = current;
        let removed = next.entries.remove(0);
        next.generation = next.generation.saturating_add(1);
        self.commit_descriptor(next.clone()).await?;
        let object = self.objects.join(removed.object_name);
        let _ = tokio::task::spawn_blocking(move || fs::remove_file(object)).await;
        self.notify(QueueEventReason::Advance, next.generation);
        Ok(true)
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
        let (bytes, modified) = tokio::task::spawn_blocking(move || -> Result<_> {
            let metadata = fs::symlink_metadata(&path).context("failed to inspect queue object")?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("queue object is not a regular file");
            }
            let bytes = fs::read(&path).context("failed to read queue object")?;
            validate_jpeg(&bytes)?;
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
    let descriptor = if state_path.exists() {
        let value: Descriptor = toml::from_str(
            &fs::read_to_string(&state_path).context("failed to read queue descriptor")?,
        )
        .context("failed to parse queue descriptor")?;
        if value.version != STATE_VERSION {
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
    let expected = descriptor
        .entries
        .iter()
        .map(|entry| entry.object_name.clone())
        .collect::<HashSet<_>>();
    for entry in &descriptor.entries {
        let path = objects.join(&entry.object_name);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("queue object is missing: {}", entry.object_name))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("queue object is unsafe: {}", entry.object_name);
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
        sync_directory(root)
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
        sync_directory(parent)
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
    File::open(path)?
        .sync_all()
        .context("failed to sync queue directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    const A: &[u8] = b"\xff\xd8\xffA\xff\xd9";
    const B: &[u8] = b"\xff\xd8\xffB\xff\xd9";

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
