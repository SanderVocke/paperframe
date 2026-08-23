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
                    "candidate {} uses an unmapped DigiKam root",
                    candidate.image_id
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

    use crate::config::{SelectionMode, SelectionSettings};

    use super::*;

    fn settings(root: &Path) -> DigikamSettings {
        DigikamSettings {
            database_path: root.join("digikam.db"),
            query: "unused".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/original/library"),
                mounted_root: root.join("photos"),
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
        assert!(store.open(&unmapped).is_err());
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
