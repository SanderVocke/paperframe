use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    catalog::Candidate,
    config::{DigikamSettings, PathMapping},
};

#[derive(Clone, Debug)]
struct CanonicalMapping {
    database_root: PathBuf,
    mounted_root: PathBuf,
}

pub struct SourceStore {
    mappings: Vec<CanonicalMapping>,
}

pub struct ReadOnlySource {
    image_id: i64,
    file: File,
}

impl SourceStore {
    pub fn new(settings: &DigikamSettings, cache_dir: &Path) -> Result<Self> {
        let cache_root = fs::canonicalize(cache_dir).with_context(|| {
            format!(
                "failed to canonicalize cache directory {}",
                cache_dir.display()
            )
        })?;
        let mappings = settings
            .path_mappings
            .iter()
            .map(|mapping| canonical_mapping(mapping, &cache_root))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { mappings })
    }

    pub fn open(&self, candidate: &Candidate) -> Result<ReadOnlySource> {
        let mapping = self
            .mappings
            .iter()
            .find(|mapping| mapping.database_root == candidate.root_path)
            .with_context(|| {
                format!(
                    "candidate {} uses an unmapped DigiKam root_path {:?}; database_root must exactly match this value",
                    candidate.image_id, candidate.root_path
                )
            })?;
        let album = safe_relative_album(&candidate.album_path)?;
        let file_name = safe_file_name(&candidate.file_name)?;
        let requested = mapping.mounted_root.join(album).join(file_name);
        let canonical = fs::canonicalize(&requested).with_context(|| {
            format!(
                "candidate {} is missing or inaccessible",
                candidate.image_id
            )
        })?;
        if !canonical.starts_with(&mapping.mounted_root) {
            bail!(
                "candidate {} resolves outside its mounted source root",
                candidate.image_id
            );
        }
        if !fs::metadata(&canonical)
            .with_context(|| format!("failed to inspect candidate {}", candidate.image_id))?
            .is_file()
        {
            bail!("candidate {} is not a regular file", candidate.image_id);
        }
        let file = OpenOptions::new()
            .read(true)
            .open(&canonical)
            .with_context(|| {
                format!("failed to open candidate {} read-only", candidate.image_id)
            })?;
        Ok(ReadOnlySource {
            image_id: candidate.image_id,
            file,
        })
    }
}

impl ReadOnlySource {
    pub fn image_id(&self) -> i64 {
        self.image_id
    }
}

impl Read for ReadOnlySource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

fn canonical_mapping(mapping: &PathMapping, cache_root: &Path) -> Result<CanonicalMapping> {
    let mounted_root = fs::canonicalize(&mapping.mounted_root).with_context(|| {
        format!(
            "failed to canonicalize mounted source root {}",
            mapping.mounted_root.display()
        )
    })?;
    if !mounted_root.is_dir() {
        bail!(
            "mounted source root is not a directory: {}",
            mounted_root.display()
        );
    }
    if cache_root.starts_with(&mounted_root) || mounted_root.starts_with(cache_root) {
        bail!("cache directory and mounted source roots must be separate");
    }
    Ok(CanonicalMapping {
        database_root: mapping.database_root.clone(),
        mounted_root,
    })
}

/// Import a relative picture beneath an explicitly configured root. Every
/// relative component is opened from a pinned directory descriptor, never by
/// rejoining an unchecked path after canonicalization.
#[cfg(unix)]
pub fn read_relative_picture(root: &Path, relative: &Path, limit: u64) -> Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => anyhow::bail!("picture path must be relative and contain no traversal"),
        })
        .collect::<Result<Vec<_>>>()?;
    if components.is_empty() {
        bail!("picture path is empty");
    }
    let root = fs::canonicalize(root).context("source root is unavailable")?;
    let mut directory = open(
        &root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("failed to open source root")?;
    for component in &components[..components.len() - 1] {
        directory = openat(
            &directory,
            *component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("path directory is inaccessible or a symlink")?;
    }
    let file = openat(
        &directory,
        *components.last().unwrap(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("picture is inaccessible or a symlink")?;
    let file = File::from(file);
    if !file.metadata()?.is_file() {
        bail!("picture is not a regular file");
    }
    let limit = limit.min(100 * 1024 * 1024);
    if file.metadata()?.len() > limit {
        bail!("picture exceeds configured size limit");
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("picture exceeds configured size limit");
    }
    Ok(bytes)
}

#[cfg(not(unix))]
pub fn read_relative_picture(_root: &Path, _relative: &Path, _limit: u64) -> Result<Vec<u8>> {
    bail!("secure server-relative imports are currently supported only on Unix; use client upload")
}

fn safe_relative_album(path: &Path) -> Result<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => relative.push(component),
            Component::ParentDir | Component::Prefix(_) => {
                bail!("album path contains traversal or a platform prefix")
            }
        }
    }
    Ok(relative)
}

fn safe_file_name(path: &Path) -> Result<&std::ffi::OsStr> {
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => Ok(name),
        _ => bail!("file_name must be one normal path component"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn relative_import_rejects_traversal_symlinks_nonfiles_and_oversized_content() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/photo.jpg"), b"inside").unwrap();
        fs::write(dir.path().join("outside.jpg"), b"outside").unwrap();
        symlink(dir.path().join("outside.jpg"), root.join("link.jpg")).unwrap();
        symlink(dir.path(), root.join("linked-dir")).unwrap();
        assert_eq!(
            read_relative_picture(&root, Path::new("nested/photo.jpg"), 100).unwrap(),
            b"inside"
        );
        for path in [
            "",
            "../outside.jpg",
            "/outside.jpg",
            "nested/../../outside.jpg",
            "link.jpg",
            "linked-dir/outside.jpg",
            "nested",
            "missing.jpg",
        ] {
            assert!(
                read_relative_picture(&root, Path::new(path), 100).is_err(),
                "{path}"
            );
        }
        assert!(read_relative_picture(&root, Path::new("nested/photo.jpg"), 2).is_err());
        assert_eq!(fs::read(root.join("nested/photo.jpg")).unwrap(), b"inside");
    }

    #[cfg(unix)]
    #[test]
    fn relative_import_never_follows_directory_replacement_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(root.join("album")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("album/picture.jpg"), b"inside").unwrap();
        fs::write(outside.join("picture.jpg"), b"outside").unwrap();
        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..300 {
                fs::rename(writer_root.join("album"), writer_root.join("pinned-album")).unwrap();
                symlink(&outside, writer_root.join("album")).unwrap();
                std::thread::yield_now();
                fs::remove_file(writer_root.join("album")).unwrap();
                fs::rename(writer_root.join("pinned-album"), writer_root.join("album")).unwrap();
            }
        });
        for _ in 0..1500 {
            if let Ok(bytes) = read_relative_picture(&root, Path::new("album/picture.jpg"), 100) {
                assert_eq!(bytes, b"inside", "replacement race escaped configured root");
            }
        }
        writer.join().unwrap();
        assert_eq!(
            read_relative_picture(&root, Path::new("album/picture.jpg"), 100).unwrap(),
            b"inside"
        );
    }

    fn settings(root: &Path) -> DigikamSettings {
        DigikamSettings {
            database_path: root.join("digikam.db"),
            query: "unused".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/original/library"),
                mounted_root: root.join("photos"),
            }],
            max_candidates: 10,
        }
    }

    fn candidate(album: &str, name: &str) -> Candidate {
        Candidate {
            image_id: 7,
            root_path: PathBuf::from("/original/library"),
            album_path: PathBuf::from(album),
            file_name: PathBuf::from(name),
        }
    }

    #[test]
    fn opens_leading_slash_album_read_only() {
        let directory = tempdir().unwrap();
        let photos = directory.path().join("photos/album");
        let cache = directory.path().join("cache");
        fs::create_dir_all(&photos).unwrap();
        fs::create_dir(&cache).unwrap();
        fs::write(photos.join("one.jpg"), b"source").unwrap();
        let store = SourceStore::new(&settings(directory.path()), &cache).unwrap();

        let mut source = store.open(&candidate("/album", "one.jpg")).unwrap();
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).unwrap();

        assert_eq!(bytes, b"source");
        assert_eq!(source.image_id(), 7);
    }

    #[test]
    fn rejects_traversal_unmapped_roots_and_cache_overlap() {
        let directory = tempdir().unwrap();
        let photos = directory.path().join("photos");
        let cache = directory.path().join("cache");
        fs::create_dir(&photos).unwrap();
        fs::create_dir(&cache).unwrap();
        fs::write(directory.path().join("outside.jpg"), b"outside").unwrap();
        let configured = settings(directory.path());
        let store = SourceStore::new(&configured, &cache).unwrap();

        assert!(store.open(&candidate("../", "outside.jpg")).is_err());
        let mut unmapped = candidate("/", "outside.jpg");
        unmapped.root_path = PathBuf::from("/other");
        let error = store.open(&unmapped).err().unwrap().to_string();
        assert!(error.contains("unmapped DigiKam root_path"));
        assert!(error.contains("/other"));
        assert!(SourceStore::new(&configured, &photos).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let photos = directory.path().join("photos");
        let cache = directory.path().join("cache");
        fs::create_dir(&photos).unwrap();
        fs::create_dir(&cache).unwrap();
        fs::write(directory.path().join("outside.jpg"), b"outside").unwrap();
        symlink(
            directory.path().join("outside.jpg"),
            photos.join("link.jpg"),
        )
        .unwrap();
        let store = SourceStore::new(&settings(directory.path()), &cache).unwrap();

        assert!(store.open(&candidate("/", "link.jpg")).is_err());
    }
}
