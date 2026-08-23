use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use crate::config::FolderSettings;

const OP_PREFIX: &str = ".paperframe-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderEntry {
    pub id: String,
    pub filename: String,
    pub content_length: u64,
}

#[derive(Clone, Debug)]
pub struct FolderSnapshot {
    pub generation: u64,
    pub entries: Vec<FolderEntry>,
    pub next_index: usize,
}

impl FolderSnapshot {
    pub fn serving_order(&self) -> Vec<FolderEntry> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        self.entries[self.next_index..]
            .iter()
            .chain(self.entries[..self.next_index].iter())
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub filename: String,
}

#[derive(Debug)]
struct Inventory {
    generation: u64,
    entries: Vec<FolderEntry>,
    next_index: usize,
}

pub struct FolderMode {
    root: PathBuf,
    max_upload_bytes: u64,
    inventory: RwLock<Inventory>,
    operation_lock: Mutex<()>,
}

impl FolderMode {
    pub async fn new(
        settings: &FolderSettings,
        cache_root: &Path,
        settings_path: &Path,
    ) -> Result<Arc<Self>> {
        settings.validate_basic()?;
        let configured = settings.directory.clone();
        let cache_root = cache_root.to_path_buf();
        let settings_path = settings_path.to_path_buf();
        let (root, entries) = tokio::task::spawn_blocking(move || {
            initialize_folder(&configured, &cache_root, &settings_path)
        })
        .await
        .context("folder initialization worker stopped")??;
        Ok(Arc::new(Self {
            root,
            max_upload_bytes: settings.max_upload_bytes,
            inventory: RwLock::new(Inventory {
                generation: 1,
                entries,
                next_index: 0,
            }),
            operation_lock: Mutex::new(()),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_upload_bytes(&self) -> u64 {
        self.max_upload_bytes
    }

    pub async fn snapshot(&self) -> FolderSnapshot {
        let inventory = self.inventory.read().await;
        FolderSnapshot {
            generation: inventory.generation,
            entries: inventory.entries.clone(),
            next_index: inventory.next_index,
        }
    }

    pub async fn is_ready(&self) -> bool {
        !self.inventory.read().await.entries.is_empty()
    }

    pub async fn next_image(&self) -> Result<PreparedImage> {
        let _operation = self.operation_lock.lock().await;
        let (generation, index, entry) = {
            let inventory = self.inventory.read().await;
            let Some(entry) = inventory.entries.get(inventory.next_index).cloned() else {
                bail!("folder contains no images");
            };
            (inventory.generation, inventory.next_index, entry)
        };
        let root = self.root.clone();
        let to_read = entry.clone();
        let image = tokio::task::spawn_blocking(move || read_entry(&root, &to_read))
            .await
            .context("folder image worker stopped")??;

        let mut inventory = self.inventory.write().await;
        if inventory.generation != generation
            || inventory.next_index != index
            || inventory.entries.get(index).map(|item| &item.id) != Some(&entry.id)
        {
            bail!("folder inventory changed while preparing the image");
        }
        inventory.next_index = (index + 1) % inventory.entries.len();
        Ok(image)
    }

    pub async fn peek_image(&self) -> Result<PreparedImage> {
        let _operation = self.operation_lock.lock().await;
        let entry = {
            let inventory = self.inventory.read().await;
            inventory
                .entries
                .get(inventory.next_index)
                .cloned()
                .context("folder contains no images")?
        };
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || read_entry(&root, &entry))
            .await
            .context("folder image worker stopped")?
    }

    pub async fn thumbnail(&self, id: &str) -> Result<PreparedImage> {
        let entry = self
            .inventory
            .read()
            .await
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .context("thumbnail is not in the current inventory")?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || read_entry(&root, &entry))
            .await
            .context("folder thumbnail worker stopped")?
    }

    pub async fn refresh(&self, expected_generation: u64) -> Result<FolderSnapshot> {
        let _operation = self.operation_lock.lock().await;
        self.require_generation(expected_generation).await?;
        let root = self.root.clone();
        let entries = tokio::task::spawn_blocking(move || scan_folder(&root))
            .await
            .context("folder refresh worker stopped")??;
        self.replace_inventory(expected_generation, entries).await
    }

    pub async fn upload(
        &self,
        expected_generation: u64,
        uploads: Vec<(String, Vec<u8>)>,
    ) -> Result<FolderSnapshot> {
        let _operation = self.operation_lock.lock().await;
        self.require_generation(expected_generation).await?;
        if uploads.is_empty() {
            bail!("select at least one JPEG to upload");
        }
        let total = uploads.iter().try_fold(0_u64, |total, (_, bytes)| {
            total
                .checked_add(bytes.len() as u64)
                .context("upload is too large")
        })?;
        if total > self.max_upload_bytes {
            bail!(
                "upload exceeds the configured {} byte limit",
                self.max_upload_bytes
            );
        }
        let current = self.inventory.read().await.entries.clone();
        let root = self.root.clone();
        let entries =
            tokio::task::spawn_blocking(move || upload_and_reorder(&root, &current, uploads))
                .await
                .context("folder upload worker stopped")??;
        self.replace_inventory(expected_generation, entries).await
    }

    pub async fn reorder(
        &self,
        expected_generation: u64,
        ordered_filenames: Vec<String>,
    ) -> Result<FolderSnapshot> {
        let _operation = self.operation_lock.lock().await;
        self.require_generation(expected_generation).await?;
        let current = self.inventory.read().await.entries.clone();
        validate_complete_order(&current, &ordered_filenames)?;
        let root = self.root.clone();
        let entries = tokio::task::spawn_blocking(move || {
            let sources = ordered_filenames
                .into_iter()
                .map(|filename| (root.join(&filename), filename))
                .collect::<Vec<_>>();
            rename_in_order(&root, sources)?;
            scan_folder(&root)
        })
        .await
        .context("folder reorder worker stopped")??;
        self.replace_inventory(expected_generation, entries).await
    }

    async fn require_generation(&self, expected: u64) -> Result<()> {
        let actual = self.inventory.read().await.generation;
        if actual != expected {
            bail!(
                "folder inventory changed; expected generation {expected}, current generation is {actual}"
            );
        }
        Ok(())
    }

    async fn replace_inventory(
        &self,
        expected_generation: u64,
        entries: Vec<FolderEntry>,
    ) -> Result<FolderSnapshot> {
        let mut inventory = self.inventory.write().await;
        if inventory.generation != expected_generation {
            bail!("folder inventory changed while completing the operation");
        }
        inventory.generation = inventory.generation.saturating_add(1);
        inventory.entries = entries;
        inventory.next_index = 0;
        Ok(FolderSnapshot {
            generation: inventory.generation,
            entries: inventory.entries.clone(),
            next_index: 0,
        })
    }
}

fn initialize_folder(
    configured: &Path,
    cache_root: &Path,
    settings_path: &Path,
) -> Result<(PathBuf, Vec<FolderEntry>)> {
    let root = fs::canonicalize(configured)
        .with_context(|| format!("failed to canonicalize folder {}", configured.display()))?;
    if !root.is_dir() {
        bail!("configured folder is not a directory: {}", root.display());
    }
    let cache = fs::canonicalize(cache_root).context("failed to canonicalize cache directory")?;
    if root.starts_with(&cache) || cache.starts_with(&root) {
        bail!("managed folder and cache directory must be separate");
    }
    let canonical_settings = fs::canonicalize(settings_path).with_context(|| {
        format!(
            "failed to canonicalize settings file {}",
            settings_path.display()
        )
    })?;
    if canonical_settings.starts_with(&root) {
        bail!("settings file must not be inside the managed folder");
    }
    recover_temporary_files(&root)?;
    verify_writable(&root)?;
    let entries = scan_folder(&root)?;
    Ok((root, entries))
}

fn verify_writable(root: &Path) -> Result<()> {
    let path = root.join(format!("{OP_PREFIX}write-test-{}", random_suffix()));
    let result = OpenOptions::new().write(true).create_new(true).open(&path);
    match result {
        Ok(file) => {
            file.sync_all()
                .context("failed to sync folder write test")?;
            drop(file);
            fs::remove_file(path).context("failed to remove folder write test")?;
            Ok(())
        }
        Err(error) => Err(error).context("managed folder is not writable"),
    }
}

fn recover_temporary_files(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root).context("failed to inspect managed folder")? {
        let entry = entry.context("failed to inspect managed folder entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(".paperframe-upload-") && name.ends_with(".tmp") {
            fs::remove_file(entry.path()).context("failed to remove abandoned upload")?;
        } else if name.starts_with(".paperframe-reorder-") {
            let (_, original) = name
                .split_once("--")
                .context("invalid abandoned reorder filename")?;
            let destination = root.join(original);
            if destination.exists() {
                bail!(
                    "cannot recover abandoned reorder file because {} exists",
                    destination.display()
                );
            }
            fs::rename(entry.path(), destination).context("failed to recover abandoned reorder")?;
        }
    }
    Ok(())
}

fn scan_folder(root: &Path) -> Result<Vec<FolderEntry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(root).context("failed to scan managed folder")? {
        let item = item.context("failed to inspect managed folder entry")?;
        let name = item.file_name();
        let name = name
            .to_str()
            .context("image filenames must be valid UTF-8")?;
        if name.starts_with(OP_PREFIX) || !is_jpeg_name(name) {
            continue;
        }
        if name.chars().any(char::is_control) {
            bail!("JPEG filename contains control characters: {name:?}");
        }
        let metadata = fs::symlink_metadata(item.path())
            .with_context(|| format!("failed to inspect {name}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("JPEG entry is not a regular non-symlink file: {name}");
        }
        let bytes = fs::read(item.path()).with_context(|| format!("failed to read {name}"))?;
        validate_jpeg(&bytes).with_context(|| format!("invalid JPEG {name}"))?;
        entries.push(FolderEntry {
            id: entry_id(name),
            filename: name.to_owned(),
            content_length: bytes.len() as u64,
        });
    }
    entries.sort_by(|left, right| left.filename.cmp(&right.filename));
    Ok(entries)
}

fn read_entry(root: &Path, entry: &FolderEntry) -> Result<PreparedImage> {
    let path = confined_entry_path(root, &entry.filename)?;
    let metadata = fs::symlink_metadata(&path).context("failed to inspect folder image")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("folder image is no longer a regular file");
    }
    let canonical = fs::canonicalize(&path).context("failed to resolve folder image")?;
    if !canonical.starts_with(root) {
        bail!("folder image resolves outside the managed folder");
    }
    let bytes = fs::read(canonical).context("failed to read folder image")?;
    validate_jpeg(&bytes)?;
    Ok(PreparedImage {
        bytes,
        filename: entry.filename.clone(),
    })
}

fn confined_entry_path(root: &Path, filename: &str) -> Result<PathBuf> {
    let path = Path::new(filename);
    let mut components = path.components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        bail!("invalid folder filename");
    }
    Ok(root.join(path))
}

fn upload_and_reorder(
    root: &Path,
    current: &[FolderEntry],
    uploads: Vec<(String, Vec<u8>)>,
) -> Result<Vec<FolderEntry>> {
    let nonce = random_suffix();
    let mut temporary = Vec::new();
    let result = (|| -> Result<Vec<FolderEntry>> {
        for (index, (browser_name, bytes)) in uploads.into_iter().enumerate() {
            validate_jpeg(&bytes)
                .with_context(|| format!("upload {browser_name} is not a JPEG"))?;
            let safe_name = safe_upload_name(&browser_name)?;
            let path = root.join(format!(".paperframe-upload-{nonce}-{index}.tmp"));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .context("failed to create upload temporary file")?;
            file.write_all(&bytes).context("failed to write upload")?;
            file.sync_all().context("failed to sync upload")?;
            drop(file);
            temporary.push((path, safe_name));
        }
        let mut sources = current
            .iter()
            .map(|entry| (root.join(&entry.filename), entry.filename.clone()))
            .collect::<Vec<_>>();
        sources.extend(temporary.iter().cloned());
        rename_in_order(root, sources)?;
        scan_folder(root)
    })();
    if result.is_err() {
        for (path, _) in temporary {
            let _ = fs::remove_file(path);
        }
    }
    result
}

fn validate_complete_order(current: &[FolderEntry], submitted: &[String]) -> Result<()> {
    if current.len() != submitted.len() {
        bail!("reorder must contain every current image exactly once");
    }
    let mut expected = current
        .iter()
        .map(|entry| entry.filename.clone())
        .collect::<Vec<_>>();
    let mut actual = submitted.to_vec();
    expected.sort();
    actual.sort();
    if expected != actual {
        bail!("reorder contains missing, duplicate, or unknown filenames");
    }
    Ok(())
}

fn rename_in_order(root: &Path, sources: Vec<(PathBuf, String)>) -> Result<()> {
    let nonce = random_suffix();
    let mut staged: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();
    for (index, (source, original_name)) in sources.into_iter().enumerate() {
        let temporary = root.join(format!(
            ".paperframe-reorder-{nonce}-{index}--{original_name}"
        ));
        if let Err(error) = fs::rename(&source, &temporary) {
            rollback_renames(&staged);
            return Err(error).context("failed to stage image reorder");
        }
        let base = strip_order_prefix(&original_name);
        let destination = root.join(format!("{:06}--{}", index + 1, base));
        staged.push((temporary, source, destination));
    }

    for (published, (temporary, _, destination)) in staged.iter().enumerate() {
        if let Err(error) = fs::rename(temporary, destination) {
            rollback_published(&staged, published);
            return Err(error).context("failed to publish image reorder");
        }
    }
    if let Err(error) = sync_directory(root) {
        rollback_published(&staged, staged.len());
        let _ = sync_directory(root);
        return Err(error).context("failed to make image reorder durable");
    }
    Ok(())
}

fn rollback_renames(staged: &[(PathBuf, PathBuf, PathBuf)]) {
    for (temporary, original, _) in staged.iter().rev() {
        let _ = fs::rename(temporary, original);
    }
}

fn rollback_published(staged: &[(PathBuf, PathBuf, PathBuf)], published: usize) {
    for (_, original, destination) in staged[..published].iter().rev() {
        let _ = fs::rename(destination, original);
    }
    for (temporary, original, _) in staged[published..].iter().rev() {
        let _ = fs::rename(temporary, original);
    }
}

fn safe_upload_name(input: &str) -> Result<String> {
    let path = Path::new(input);
    let mut components = path.components();
    let name = match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => {
            name.to_str().context("upload filename must be UTF-8")?
        }
        _ => bail!("upload filename must be one safe path component"),
    };
    if !is_jpeg_name(name) {
        bail!("upload filename must end in .jpg or .jpeg");
    }
    let stripped = strip_order_prefix(name);
    let sanitized = stripped
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        bail!("upload filename is empty after sanitization");
    }
    Ok(sanitized)
}

fn strip_order_prefix(name: &str) -> &str {
    match name.split_once("--") {
        Some((prefix, rest))
            if prefix.len() == 6 && prefix.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            rest
        }
        _ => name,
    }
}

fn is_jpeg_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".jpg") || lower.ends_with(".jpeg")
}

fn validate_jpeg(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 5 || !bytes.starts_with(&[0xff, 0xd8, 0xff]) || !bytes.ends_with(&[0xff, 0xd9])
    {
        bail!("file does not have valid JPEG boundary markers");
    }
    Ok(())
}

fn entry_id(filename: &str) -> String {
    let digest = Sha256::digest(filename.as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn random_suffix() -> String {
    let value = rand::random::<u64>();
    format!("{}-{value:016x}", std::process::id())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open {} for sync", path.display()))?
        .sync_all()
        .context("failed to sync managed folder")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const A: &[u8] = b"\xff\xd8\xffA\xff\xd9";
    const B: &[u8] = b"\xff\xd8\xffB\xff\xd9";

    async fn mode() -> (tempfile::TempDir, Arc<FolderMode>) {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("images");
        let cache = directory.path().join("cache");
        fs::create_dir(&folder).unwrap();
        fs::create_dir(&cache).unwrap();
        let settings_file = directory.path().join("settings.toml");
        fs::write(&settings_file, "settings").unwrap();
        fs::write(folder.join("b.jpg"), B).unwrap();
        fs::write(folder.join("a.jpg"), A).unwrap();
        let mode = FolderMode::new(
            &FolderSettings {
                directory: folder,
                max_upload_bytes: 1024,
            },
            &cache,
            &settings_file,
        )
        .await
        .unwrap();
        (directory, mode)
    }

    #[tokio::test]
    async fn serves_sorted_sequence_wraps_and_peek_does_not_advance() {
        let (_directory, mode) = mode().await;
        assert_eq!(mode.peek_image().await.unwrap().bytes, A);
        assert_eq!(mode.next_image().await.unwrap().bytes, A);
        assert_eq!(mode.next_image().await.unwrap().bytes, B);
        assert_eq!(mode.next_image().await.unwrap().bytes, A);
    }

    #[tokio::test]
    async fn refresh_reconciles_disk_and_resets_cursor() {
        let (directory, mode) = mode().await;
        mode.next_image().await.unwrap();
        fs::write(directory.path().join("images/0.jpg"), B).unwrap();
        let generation = mode.snapshot().await.generation;
        let snapshot = mode.refresh(generation).await.unwrap();
        assert_eq!(snapshot.entries[0].filename, "0.jpg");
        assert_eq!(snapshot.next_index, 0);
        assert!(mode.refresh(generation).await.is_err());
    }

    #[tokio::test]
    async fn upload_and_reorder_rename_files_durably() {
        let (directory, mode) = mode().await;
        let generation = mode.snapshot().await.generation;
        let snapshot = mode
            .upload(generation, vec![("../c.jpg".into(), A.to_vec())])
            .await;
        assert!(snapshot.is_err());
        let snapshot = mode
            .upload(generation, vec![("c.jpg".into(), A.to_vec())])
            .await
            .unwrap();
        assert_eq!(snapshot.entries.len(), 3);
        assert!(
            snapshot
                .entries
                .iter()
                .all(|entry| entry.filename.starts_with('0'))
        );
        let order = snapshot
            .entries
            .iter()
            .rev()
            .map(|entry| entry.filename.clone())
            .collect();
        let reordered = mode.reorder(snapshot.generation, order).await.unwrap();
        assert_eq!(reordered.next_index, 0);
        assert_eq!(
            scan_folder(&directory.path().join("images")).unwrap(),
            reordered.entries
        );
    }

    #[tokio::test]
    async fn concurrent_gets_take_distinct_positions_and_failure_does_not_advance() {
        let (directory, mode) = mode().await;
        let (first, second) = tokio::join!(mode.next_image(), mode.next_image());
        let mut delivered = vec![first.unwrap().bytes, second.unwrap().bytes];
        delivered.sort();
        assert_eq!(delivered, vec![A.to_vec(), B.to_vec()]);
        assert_eq!(mode.next_image().await.unwrap().bytes, A);

        fs::remove_file(directory.path().join("images/b.jpg")).unwrap();
        // The next item is b.jpg after serving a.jpg above.
        assert!(mode.next_image().await.is_err());
        fs::write(directory.path().join("images/b.jpg"), B).unwrap();
        assert_eq!(mode.next_image().await.unwrap().bytes, B);
    }

    #[tokio::test]
    async fn duplicate_upload_names_do_not_collide_and_oversized_upload_is_rejected() {
        let (_directory, mode) = mode().await;
        let generation = mode.snapshot().await.generation;
        let uploaded = mode
            .upload(
                generation,
                vec![
                    ("same.jpg".into(), A.to_vec()),
                    ("same.jpg".into(), B.to_vec()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(uploaded.entries.len(), 4);
        let mut filenames = uploaded
            .entries
            .iter()
            .map(|entry| entry.filename.as_str())
            .collect::<Vec<_>>();
        filenames.sort_unstable();
        filenames.dedup();
        assert_eq!(filenames.len(), 4);

        let mut too_large = vec![0_u8; 1025];
        too_large[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        too_large[1023..].copy_from_slice(&[0xff, 0xd9]);
        assert!(
            mode.upload(uploaded.generation, vec![("large.jpg".into(), too_large)])
                .await
                .is_err()
        );
        assert_eq!(mode.snapshot().await.generation, uploaded.generation);
    }

    #[tokio::test]
    async fn rejects_invalid_upload_and_incomplete_reorder_without_state_change() {
        let (_directory, mode) = mode().await;
        let before = mode.snapshot().await;
        assert!(
            mode.upload(
                before.generation,
                vec![("bad.jpg".into(), b"not jpeg".to_vec())]
            )
            .await
            .is_err()
        );
        assert!(
            mode.reorder(before.generation, vec!["a.jpg".into()])
                .await
                .is_err()
        );
        let after = mode.snapshot().await;
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.entries, before.entries);
    }

    #[test]
    fn failed_batch_rename_rolls_staged_files_back_without_overwrite() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.jpg"), A).unwrap();
        fs::write(root.join("b.jpg"), B).unwrap();
        fs::create_dir(root.join("000001--a.jpg")).unwrap();
        let result = rename_in_order(
            root,
            vec![
                (root.join("a.jpg"), "a.jpg".into()),
                (root.join("b.jpg"), "b.jpg".into()),
            ],
        );
        assert!(result.is_err());
        assert_eq!(fs::read(root.join("a.jpg")).unwrap(), A);
        assert_eq!(fs::read(root.join("b.jpg")).unwrap(), B);
        assert!(root.join("000001--a.jpg").is_dir());
        assert_eq!(fs::read_dir(root).unwrap().count(), 3);
    }

    #[test]
    fn recovers_staged_reorder_and_removes_abandoned_upload() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join(".paperframe-reorder-x-0--a.jpg"), A).unwrap();
        fs::write(root.join(".paperframe-upload-x.tmp"), B).unwrap();
        recover_temporary_files(root).unwrap();
        assert_eq!(fs::read(root.join("a.jpg")).unwrap(), A);
        assert!(!root.join(".paperframe-upload-x.tmp").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_jpeg() {
        use std::os::unix::fs::symlink;
        let (directory, mode) = mode().await;
        symlink(
            directory.path().join("images/a.jpg"),
            directory.path().join("images/link.jpg"),
        )
        .unwrap();
        assert!(
            mode.refresh(mode.snapshot().await.generation)
                .await
                .is_err()
        );
    }
}
