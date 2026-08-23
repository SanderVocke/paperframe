use std::{
    collections::BTreeSet,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    catalog,
    config::{DigikamSettings, ManualFolderSettings, SupplierKind, SupplierSettings},
    source::SourceStore,
};

#[derive(Clone, Debug)]
pub struct SuppliedImage {
    pub source_key: String,
    pub display_name: String,
    pub bytes: Vec<u8>,
}

pub trait Supplier: Send + Sync {
    fn id(&self) -> &str;
    fn poll_interval(&self) -> Option<Duration> {
        None
    }
    fn supply<'a>(
        &'a self,
        seen: BTreeSet<String>,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SuppliedImage>>> + Send + 'a>>;
}

pub struct SupplierRegistry {
    suppliers: Vec<Arc<dyn Supplier>>,
}

impl SupplierRegistry {
    pub async fn prepare(settings: &[SupplierSettings], queue_root: &Path) -> Result<Arc<Self>> {
        let mut suppliers: Vec<Arc<dyn Supplier>> = Vec::new();
        for configured in settings {
            let supplier: Arc<dyn Supplier> = match &configured.kind {
                SupplierKind::DigikamSqlite(digikam) => {
                    catalog::validate(digikam.clone()).await?;
                    let store = SourceStore::new(digikam, queue_root)?;
                    Arc::new(DigikamSupplier {
                        id: configured.id.clone(),
                        settings: digikam.clone(),
                        sources: Arc::new(store),
                    })
                }
                SupplierKind::ManualFolder(manual) => {
                    Arc::new(ManualSupplier::new(&configured.id, manual, queue_root)?)
                }
            };
            suppliers.push(supplier);
        }
        Ok(Arc::new(Self { suppliers }))
    }

    pub fn suppliers(&self) -> &[Arc<dyn Supplier>] {
        &self.suppliers
    }

    pub fn shortest_poll(&self) -> Option<Duration> {
        self.suppliers
            .iter()
            .filter_map(|supplier| supplier.poll_interval())
            .min()
    }
}

struct DigikamSupplier {
    id: String,
    settings: DigikamSettings,
    sources: Arc<SourceStore>,
}

impl Supplier for DigikamSupplier {
    fn id(&self) -> &str {
        &self.id
    }

    fn supply<'a>(
        &'a self,
        seen: BTreeSet<String>,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SuppliedImage>>> + Send + 'a>> {
        Box::pin(async move {
            let candidates = catalog::candidates(self.settings.clone()).await?;
            let sources = self.sources.clone();
            let supplier_id = self.id.clone();
            tokio::task::spawn_blocking(move || {
                let mut supplied = Vec::new();
                for candidate in candidates {
                    let key = format!("image:{}", candidate.image_id);
                    if seen.contains(&key) { continue; }
                    match sources.open(&candidate) {
                        Ok(mut source) => {
                            let mut bytes = Vec::new();
                            std::io::Read::read_to_end(&mut source, &mut bytes)
                                .with_context(|| format!("failed to read DigiKam image {}", candidate.image_id))?;
                            validate_jpeg(&bytes).with_context(|| format!("DigiKam image {} is not a JPEG", candidate.image_id))?;
                            supplied.push(SuppliedImage {
                                source_key: key,
                                display_name: candidate.file_name.to_string_lossy().to_string(),
                                bytes,
                            });
                            if supplied.len() == limit { break; }
                        }
                        Err(error) => tracing::warn!(supplier = %supplier_id, image_id = candidate.image_id, %error, "DigiKam candidate skipped"),
                    }
                }
                Ok(supplied)
            }).await.context("DigiKam supply worker stopped")?
        })
    }
}

struct ManualSupplier {
    id: String,
    root: PathBuf,
    poll: Duration,
}

impl ManualSupplier {
    fn new(id: &str, settings: &ManualFolderSettings, queue_root: &Path) -> Result<Self> {
        let root = std::fs::canonicalize(&settings.directory).with_context(|| {
            format!(
                "failed to canonicalize manual source {}",
                settings.directory.display()
            )
        })?;
        if !root.is_dir() {
            bail!("manual source is not a directory");
        }
        let queue =
            std::fs::canonicalize(queue_root).context("failed to canonicalize queue root")?;
        if root.starts_with(&queue) || queue.starts_with(&root) {
            bail!("manual source and queue root must be separate");
        }
        Ok(Self {
            id: id.into(),
            root,
            poll: Duration::from_secs(settings.poll_interval_seconds),
        })
    }
}

impl Supplier for ManualSupplier {
    fn id(&self) -> &str {
        &self.id
    }
    fn poll_interval(&self) -> Option<Duration> {
        Some(self.poll)
    }

    fn supply<'a>(
        &'a self,
        seen: BTreeSet<String>,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SuppliedImage>>> + Send + 'a>> {
        Box::pin(async move {
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || scan_manual(&root, &seen, limit))
                .await
                .context("manual source scan worker stopped")?
        })
    }
}

fn scan_manual(root: &Path, seen: &BTreeSet<String>, limit: usize) -> Result<Vec<SuppliedImage>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root).context("failed to scan manual source")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("manual JPEG filenames must be UTF-8")?;
        if !is_jpeg_name(name) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        paths.push((name.to_owned(), entry.path()));
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    let mut supplied = Vec::new();
    for (name, path) in paths {
        let canonical =
            std::fs::canonicalize(&path).context("failed to resolve manual source image")?;
        if !canonical.starts_with(root) {
            bail!("manual source image escapes source root");
        }
        let metadata = std::fs::symlink_metadata(&canonical)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let bytes = std::fs::read(&canonical)
            .with_context(|| format!("failed to read manual image {name}"))?;
        validate_jpeg(&bytes).with_context(|| format!("invalid manual JPEG {name}"))?;
        let content_digest = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let key = format!("file:{name}:{content_digest}");
        if seen.contains(&key) {
            continue;
        }
        supplied.push(SuppliedImage {
            source_key: key,
            display_name: name,
            bytes,
        });
        if supplied.len() == limit {
            break;
        }
    }
    Ok(supplied)
}

fn is_jpeg_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".jpg") || lower.ends_with(".jpeg")
}
fn validate_jpeg(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 5 || !bytes.starts_with(&[0xff, 0xd8, 0xff]) || !bytes.ends_with(&[0xff, 0xd9])
    {
        bail!("invalid JPEG markers");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn manual_supply_is_alphabetical_and_changed_content_is_new() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("b.jpg"), b"\xff\xd8\xffB\xff\xd9").unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"\xff\xd8\xffA\xff\xd9").unwrap();
        let first = scan_manual(dir.path(), &BTreeSet::new(), 10).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|item| item.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["a.jpg", "b.jpg"]
        );
        let seen = first.iter().map(|item| item.source_key.clone()).collect();
        std::fs::write(dir.path().join("a.jpg"), b"\xff\xd8\xffNEW\xff\xd9").unwrap();
        let changed = scan_manual(dir.path(), &seen, 10).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].display_name, "a.jpg");
    }
}
