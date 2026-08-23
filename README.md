# Paper frame server

A single-process Rust service that serves JPEG images to a PaperlessPaper frame running the offline firmware. The image URL, health endpoints, request tracking, and settings/status page are shared; an active **image mode** supplies the images and decides when they advance.

The supported modes are:

- **DigiKam SQLite:** selects a cached image at startup and a configured daily boundary from a read-only DigiKam catalog.
- **Sequential server folder:** manages top-level JPEGs in a writable directory and returns the next filename-sorted image for every frame `GET`.

It is intended for a trusted home LAN and one running process/replica.

## HTTP endpoints

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/settings` | Active-mode status, settings, and folder queue controls |
| `POST` | `/settings` | Validate, persist, and activate complete mode settings |
| `POST` | `/folder/refresh` | Reconcile folder mode with files currently on disk |
| `POST` | `/folder/upload` | Upload and append one or more JPEGs |
| `POST` | `/folder/reorder` | Rename files to persist a submitted queue order |
| `GET` | `/folder/thumbnail/{id}` | Non-advancing image preview for the current folder inventory |
| `GET` | `/frame/current.jpg` | Current DigiKam cache or next sequential-folder JPEG |
| `HEAD` | `/frame/current.jpg` | Image metadata without advancing folder mode or request status |
| `GET` | `/health/live` | `200` while the HTTP process runs |
| `GET` | `/health/ready` | `200` when the active mode has an image; otherwise `503` |

Configure the offline firmware URL as:

```text
http://SERVER_LAN_IP:8080/frame/current.jpg
```

## Local setup

A current stable Rust toolchain is required. DigiKam mode additionally needs a readable SQLite catalog/photo library; folder mode needs a dedicated writable directory.

```sh
cp config.example.toml config.toml
mkdir -p data/cache data/frame-images
# Choose one:
cp settings.example.toml data/settings.toml
# cp settings.folder.example.toml data/settings.toml
cargo run --release -- config.toml
```

The configuration path defaults to `config.toml`. Relative bootstrap paths are resolved from the process working directory. `RUST_LOG` controls structured logging.

### Bootstrap configuration

```toml
bind = "0.0.0.0:8080"
settings_path = "data/settings.toml"
cache_dir = "data/cache"
```

This file is immutable while the process runs. The settings file and cache directory must be writable. The settings file must already exist and its parent must be writable so updates can use a same-directory temporary file, sync, and atomic rename.

### Mode settings

Canonical settings are tagged and contain only the active mode's strongly typed fields:

```toml
mode = "folder_sequence"

[mode_settings]
directory = "/data/frame-images"
max_upload_bytes = 26214400
```

See [`settings.example.toml`](settings.example.toml) and [`settings.folder.example.toml`](settings.folder.example.toml). Existing untagged DigiKam settings files remain accepted as legacy DigiKam mode. The next successful settings save writes the canonical tagged format.

A generation value prevents concurrent settings forms from silently overwriting one another. Switching modes validates and initializes the replacement before persistence/live activation. Switching to DigiKam triggers immediate selection; switching to folder mode scans it immediately and leaves DigiKam rollover work idle.

## Sequential server-folder mode

Folder mode intentionally manages and renames files. Its directory must be writable, separate from the cache, and must not contain the settings file. Do not point it at a DigiKam-managed or otherwise immutable photo library.

### Inventory and advancement

- Only top-level regular, non-symlink `.jpg`/`.jpeg` files with JPEG boundary markers are eligible. Subdirectories and unrelated files are ignored; unsafe JPEG entries make refresh fail.
- The initial inventory is sorted by exact filename. There is no recursive traversal or filesystem watcher.
- Startup, mode activation, upload, reorder, and successful refresh reset the runtime cursor to the first image.
- Every successful `GET /frame/current.jpg` returns the next image and advances once, wrapping after the last image. Concurrent GETs are serialized around reservation/preparation so they take distinct queue positions.
- Failed image preparation does not advance. `HEAD`, thumbnails, settings, and health requests never advance.
- The cursor is not persisted. A restart begins at the alphabetically first file.
- Manual server-side additions/removals are intentionally invisible until **Refresh queue from disk** succeeds. A failed refresh retains the previous in-memory queue.
- An empty queue returns `404` from the frame endpoint and makes readiness return `503`.

The status page rotates its thumbnail list into serving order and marks the next image. Preview URLs use opaque inventory identifiers and resolve only files in the active canonical folder.

### Reordering and filenames

The accessible reorder form contains every current filename, one per line. Submitting a complete new order:

1. Rejects stale inventory generations, duplicates, omissions, and unknown names.
2. Stages every file to a collision-safe temporary name.
3. Renames files to fixed-width ordering names such as `000001--holiday.jpg`.
4. Syncs the directory and publishes a freshly scanned inventory.

Therefore ordinary alphabetical sorting reproduces the chosen order after refresh or restart. Recognized ordering prefixes are replaced rather than accumulated. Files are never overwritten. Failures attempt rollback, and recognized interrupted-operation names are recovered on startup/activation.

### Uploads

The multipart upload form accepts one or more `.jpg`/`.jpeg` files. The total bytes must not exceed `max_upload_bytes` (maximum configurable value: 100 MiB), as well as the HTTP route's fixed defence-in-depth bound. Browser filenames must be one safe path component and are sanitized; JPEG markers are checked from content.

Uploads use `create_new` same-directory temporary files, file sync, collision-safe ordered renaming, and directory sync. They append in submitted order, reset the cursor, and never overwrite existing files. Invalid CSRF, stale generation, malformed/oversized/non-JPEG input, or a filesystem error leaves the published inventory unchanged.

### Rotating HTTP semantics

A folder-mode frame `GET` always returns the selected body with `Content-Type: image/jpeg`, `Content-Length`, and `Cache-Control: no-store`. `If-Modified-Since` is ignored because the representation intentionally changes on every GET; returning `304` would consume a queue position without delivering the next image. `HEAD` reports the next image length without advancing.

## DigiKam SQLite mode

DigiKam mode preserves the original daily cached-image behavior. Its settings include:

- `database_path`: SQLite catalog visible to the process.
- `query`: candidate selection query.
- `path_mappings`: DigiKam database roots translated to mounted roots.
- `selection.mode`: `first` or `daily_random`.
- `selection.random_seed`: stable randomization seed.
- `selection.timezone` and `selection.rollover_time`: IANA timezone and local `HH:MM` boundary.
- `max_candidates`: upper bound on materialized query rows.

### Query contract and read-only safety

The SQL must be exactly one read-only SQLite `SELECT` or CTE returning:

| Alias | Type | Meaning |
|---|---|---|
| `image_id` | integer | Stable DigiKam image identity |
| `root_path` | text | Album-root path stored by DigiKam |
| `album_path` | text | Path relative to the album root |
| `file_name` | text | One filename component |

Safety is enforced with SQL parsing, SQLite's statement-read-only check, `SQLITE_OPEN_READ_ONLY`, no database creation/extension loading, and `PRAGMA query_only = ON`. SQLite work runs outside Tokio executor threads. The service never migrates or modifies DigiKam.

A path mapping translates the database path to a mounted read-only location:

```toml
[[mode_settings.path_mappings]]
database_root = "/home/user/Pictures"
mounted_root = "/photos"
```

Traversal, multi-component filenames, unmapped roots, missing files, symlink escapes, and canonical paths outside a mounted root are rejected. The cache must be separate from every source root.

### Selection, scheduling, and cache

`first` follows query order. `daily_random` deterministically ranks by seed, local selection date, and DigiKam image ID. Selection runs at startup/activation, at the next rollover (including DST handling), after settings updates, and after a bounded retry delay on failure. Only one selection runs at a time.

Candidates are opened read-only and streamed into an application-owned cache temporary while computing SHA-256 and validating JPEG markers. Publication syncs and atomically renames to `current.jpg`; source files are never linked or exposed as output paths. A matching digest leaves cache mtime unchanged. Database/source/copy failures retain the old cache and appear on `/settings`.

DigiKam frame responses retain `Last-Modified` and `If-Modified-Since`/`304` behavior. Both `200` and `304` GETs update direct-peer request status; HEAD does not.

## Storage and permissions

Use separate capabilities:

1. Immutable bootstrap file.
2. Writable persistent settings file and cache directory.
3. DigiKam database and mapped photos mounted read-only.
4. Folder-mode image directory mounted writable and persistent.

Run as a non-root UID/GID with access only to these paths. A read-only live SQLite database in WAL mode requires its matching WAL/SHM state and suitable storage semantics; prefer a consistent SQLite snapshot/backup when DigiKam writes concurrently.

The source safeguards are defence in depth. Use read-only/no-atime-capable DigiKam photo mounts, a read-only container root filesystem, no privilege escalation, and no capabilities that bypass filesystem permissions.

## Web-interface security

All rendered values are automatically HTML-escaped. Mutation bodies are bounded, forms use a per-process CSRF token, and settings/inventory generations reject stale updates. These controls are not authentication.

There is no authentication, authorization, or TLS. Anyone who can reach the service can view settings and attempt mutations. Keep it on a trusted LAN or use an authenticating TLS reverse proxy. Do not expose it directly to the Internet.

## Container/Kubernetes administrator notes

No Kubernetes manifests, Helm chart, operator, or deployment automation are included.

- Run exactly one process and one replica; there is no leader election or multi-writer/cursor coordination.
- Pass the immutable bootstrap path as the sole argument.
- Persist writable settings/cache storage and, when used, the managed folder. They may be sibling directories but cannot overlap in prohibited ways.
- Mount DigiKam catalog/photos read-only and the folder-mode directory writable. Never make a DigiKam source root the managed folder.
- Run non-root with a read-only root filesystem, disabled privilege escalation, dropped capabilities, and writes only on intended volumes.
- Use `/health/live` and `/health/ready` as probes.
- Allow graceful SIGTERM completion for in-flight selection/upload/rename work.
- Restrict network access to the trusted LAN or provide authentication/TLS externally.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```
