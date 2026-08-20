use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::source::ReadOnlySource;

static CACHE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct CacheStore {
    root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub changed: bool,
    pub digest: String,
    pub content_length: u64,
}

impl CacheStore {
    pub fn new(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)
            .with_context(|| format!("failed to create cache directory {}", path.display()))?;
        let root = fs::canonicalize(path).with_context(|| {
            format!("failed to canonicalize cache directory {}", path.display())
        })?;
        cleanup_temporary_files(&root)?;
        Ok(Self { root })
    }

    pub fn current_path(&self) -> PathBuf {
        self.root.join("current.jpg")
    }

    pub fn is_ready(&self) -> bool {
        self.current_path().is_file()
    }

    pub fn publish(&self, source: &mut ReadOnlySource) -> Result<Publication> {
        self.publish_reader(source)
    }

    fn publish_reader<R: Read>(&self, reader: &mut R) -> Result<Publication> {
        let temporary_path = self.root.join(format!(
            ".current.jpg.{}.{}.tmp",
            std::process::id(),
            CACHE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = self.publish_reader_to(reader, &temporary_path);
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }

    fn publish_reader_to<R: Read>(
        &self,
        reader: &mut R,
        temporary_path: &Path,
    ) -> Result<Publication> {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary_path)
            .context("failed to create cache temporary file")?;
        let mut hasher = Sha256::new();
        let mut content_length = 0_u64;
        let mut beginning = Vec::with_capacity(3);
        let mut ending = [0_u8; 2];
        let mut ending_count = 0_usize;
        let mut buffer = [0_u8; 64 * 1024];

        loop {
            let count = reader
                .read(&mut buffer)
                .context("failed while reading source image")?;
            if count == 0 {
                break;
            }
            temporary
                .write_all(&buffer[..count])
                .context("failed while writing cache temporary file")?;
            hasher.update(&buffer[..count]);
            content_length = content_length
                .checked_add(count as u64)
                .context("source image is too large")?;
            for byte in &buffer[..count] {
                if beginning.len() < 3 {
                    beginning.push(*byte);
                }
                ending[ending_count % 2] = *byte;
                ending_count += 1;
            }
        }

        if beginning.as_slice() != [0xff, 0xd8, 0xff]
            || ending_count < 2
            || ending[(ending_count - 2) % 2] != 0xff
            || ending[(ending_count - 1) % 2] != 0xd9
        {
            bail!("selected source is not a JPEG");
        }

        let digest = format_digest(hasher.finalize());
        if let Some(current_digest) = digest_file_if_present(&self.current_path())?
            && current_digest == digest
        {
            drop(temporary);
            fs::remove_file(temporary_path)
                .context("failed to remove unchanged cache temporary")?;
            return Ok(Publication {
                changed: false,
                digest,
                content_length,
            });
        }

        temporary
            .flush()
            .context("failed to flush cache temporary file")?;
        temporary
            .sync_all()
            .context("failed to sync cache temporary file")?;
        drop(temporary);
        fs::rename(temporary_path, self.current_path())
            .context("failed to publish current cache")?;
        sync_directory(&self.root)?;
        Ok(Publication {
            changed: true,
            digest,
            content_length,
        })
    }
}

fn digest_file_if_present(path: &Path) -> Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to open current cache for comparison"),
    };
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).context("failed to hash current cache")?;
    Ok(Some(format_digest(hasher.finalize())))
}

fn format_digest(digest: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn cleanup_temporary_files(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root).context("failed to inspect cache directory")? {
        let entry = entry.context("failed to inspect cache temporary file")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".current.jpg.") && name.ends_with(".tmp") {
            fs::remove_file(entry.path())
                .context("failed to remove abandoned cache temporary file")?;
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .context("failed to open cache directory for sync")?
        .sync_all()
        .context("failed to sync cache directory")
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Cursor, Error},
        thread,
        time::Duration,
    };

    use tempfile::tempdir;

    use super::*;

    const JPEG_A: &[u8] = b"\xff\xd8\xfffirst image\xff\xd9";
    const JPEG_B: &[u8] = b"\xff\xd8\xffsecond image\xff\xd9";

    struct FailingReader {
        delivered: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.delivered {
                return Err(Error::other("injected read failure"));
            }
            self.delivered = true;
            let bytes = b"\xff\xd8\xffpartial";
            buffer[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    #[test]
    fn publishes_atomically_and_preserves_old_cache_on_failure() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path()).unwrap();
        let first = cache.publish_reader(&mut Cursor::new(JPEG_A)).unwrap();
        assert!(first.changed);
        assert_eq!(fs::read(cache.current_path()).unwrap(), JPEG_A);

        assert!(
            cache
                .publish_reader(&mut FailingReader { delivered: false })
                .is_err()
        );

        assert_eq!(fs::read(cache.current_path()).unwrap(), JPEG_A);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn unchanged_digest_preserves_cache_mtime() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path()).unwrap();
        cache.publish_reader(&mut Cursor::new(JPEG_A)).unwrap();
        let before = fs::metadata(cache.current_path())
            .unwrap()
            .modified()
            .unwrap();
        thread::sleep(Duration::from_millis(10));

        let publication = cache.publish_reader(&mut Cursor::new(JPEG_A)).unwrap();

        assert!(!publication.changed);
        assert_eq!(
            fs::metadata(cache.current_path())
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }

    #[test]
    fn replaces_different_content_and_rejects_non_jpeg() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path()).unwrap();
        cache.publish_reader(&mut Cursor::new(JPEG_A)).unwrap();

        let replacement = cache.publish_reader(&mut Cursor::new(JPEG_B)).unwrap();
        assert!(replacement.changed);
        assert_eq!(fs::read(cache.current_path()).unwrap(), JPEG_B);
        assert!(cache.publish_reader(&mut Cursor::new(b"not jpeg")).is_err());
        assert_eq!(fs::read(cache.current_path()).unwrap(), JPEG_B);
    }

    #[test]
    fn removes_abandoned_temporary_files() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join(".current.jpg.1.1.tmp"), b"partial").unwrap();

        CacheStore::new(directory.path()).unwrap();

        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
