# Paper frame server

Paper frame server keeps a rotating list of JPEGs for a PaperlessPaper frame and serves the current image over HTTP.

You choose:

1. **Where images come from** — a normal folder or a DigiKam catalog.
2. **When the frame moves to the next image** — after a completed frame download, at daily times, or only when you click **Advance now**.

The server copies selected images into its own managed queue. It never renames, removes, or modifies your source photos or DigiKam database. Once queued, an image is unaffected if its source is later edited or deleted.

> [!IMPORTANT]
> The service has no login or TLS. Run it only on a trusted LAN, or put it behind an authenticating HTTPS proxy.

## Quick start with a folder

### 1. Prepare the files

You need Rust 1.85 or newer and this repository. Only JPEG input is supported; the server does not convert other image formats. From the repository root:

```sh
mkdir -p data/queue data/source-images
cp config.example.toml config.toml
```

Create `data/settings.toml` with an **absolute** source-folder path. This copy-and-paste example uses the folder created above:

```sh
SOURCE_DIR="$(pwd)/data/source-images"
cat > data/settings.toml <<EOF
[queue]
target_length = 10
max_upload_bytes = 26214400

[advancement]
type = "manual"

[[suppliers]]
id = "primary"
type = "manual_folder"

[suppliers.settings]
directory = "$SOURCE_DIR"
poll_interval_seconds = 60
EOF
```

Put `.jpg` or `.jpeg` files directly in `data/source-images`. Subfolders are not scanned.

### 2. Start the server

```sh
cargo run --release -- config.toml
```

The configuration path is optional and defaults to `config.toml`. To enable debug logging, set `RUST_LOG=debug` before the command.

### 3. Open the control page and connect the frame

- Control page: `http://SERVER_IP:8080/settings`
- Frame image: `http://SERVER_IP:8080/frame/current.jpg`

Replace `SERVER_IP` with the computer's LAN address. Configure the frame to fetch the frame-image URL. The control page shows readiness, refill errors, the current image, upcoming images, and all queue/settings controls.

The quick-start policy is `manual`, so test downloads cannot unexpectedly move the queue. After confirming that the frame works, choose another policy on the control page if wanted.

## Everyday use

### Manage the queue

The first queue entry is always the current image.

- **Advance now** moves to the next entry.
- **Remove** deletes that managed copy. Removing the current entry promotes the next one.
- **Upload JPEGs** adds independent copies directly to the queue.
- **Save order** applies the complete current-first order shown in the text box. Move whole lines and do not alter their opaque IDs.
- **Non-advancing preview** lets you inspect an entry without triggering fetch-based advancement.

Advancing never consumes the only remaining image, so the frame keeps displaying it. Removing that sole entry is allowed and empties the queue. An empty queue makes the frame URL return `404` and `/health/ready` return `503`.

`target_length` is a refill target, not a maximum. Supplier images are added until the queue reaches the target; direct uploads may make it longer.

### Refresh and supplier history

- **Refresh supplier and refill** scans now instead of waiting for the next poll.
- **Reset supplier history** makes previously imported source images eligible again.

The server remembers which source images it has imported. Removing a queue entry does not immediately re-add the same source; refill continues with the next unseen candidate. Use history reset only when you intentionally want old candidates to cycle again.

### Change settings

Use the control page for live changes. Editing `data/settings.toml` by hand while the process is running has no effect until restart. Changes to `config.toml` also require restart.

Keep the supplier ID stable. Changing it creates a new supplier history namespace, so existing source images become eligible again.

If a form reports that the queue or settings changed, reload the page and retry. This protects against one browser tab overwriting newer changes from another tab.

## Choose an image supplier

Exactly one supplier is currently supported.

### Manual folder

Use [`settings.folder.example.toml`](settings.folder.example.toml) as a reference.

The folder supplier:

- Reads only top-level regular `.jpg` and `.jpeg` files.
- Ignores subfolders, symlinks, and other extensions.
- Discovers files in alphabetical filename order.
- Polls at `poll_interval_seconds` and can be refreshed from the control page.
- Never writes, renames, removes, links, or directly serves source files.

The source folder must be an absolute path, must exist when you save it through the control page, and must be separate from the managed queue. If the folder is temporarily unavailable after startup, already queued images remain usable and the error appears under **Status**.

If a source file's contents change, it receives a new identity and can be imported again later. Editing or deleting it does not alter an existing queued copy.

### DigiKam SQLite

Use [`settings.example.toml`](settings.example.toml) as the DigiKam reference.

You must configure:

- `database_path`: the accessible DigiKam SQLite database.
- `query`: one read-only `SELECT` or CTE.
- `path_mappings`: how paths recorded in DigiKam map to photo directories visible to this process.
- `max_candidates`: a safety limit for query results.

The query must return these exact columns:

| Column | Meaning |
|---|---|
| `image_id` | Stable DigiKam image ID |
| `root_path` | DigiKam album-root path |
| `album_path` | Path below that root |
| `file_name` | JPEG filename only |

The SQL `ORDER BY` controls supply order. Each returned `root_path` must exactly match a `database_root` mapping. `mounted_root` is the corresponding directory on the machine or inside the container running this service.

For example, if DigiKam records `/home/alice/Pictures`, but that library is mounted at `/photos` for the server:

```toml
[[suppliers.settings.path_mappings]]
database_root = "/home/alice/Pictures"
mounted_root = "/photos"
```

The database is opened read-only and query-only. Photos are opened read-only beneath mapped roots. Invalid paths, symlink escapes, missing files, and invalid JPEGs are skipped or reported without changing DigiKam data.

For a live DigiKam catalog using SQLite WAL mode, expose a consistent database/WAL/SHM set or, preferably, point the service at a safe snapshot.

## Choose when images advance

You can change policy from the control page or settings file.

### Manual

```toml
[advancement]
type = "manual"
```

Frame downloads never advance. Use **Advance now**.

### At scheduled daily times

```toml
[advancement]
type = "scheduled"

[advancement.settings]
timezone = "Europe/Amsterdam"
times = ["03:00", "15:00"]
```

Use an [IANA timezone](https://en.wikipedia.org/wiki/List_of_tz_database_time_zones) and one or more unique 24-hour `HH:MM` times. The server chooses the next future occurrence and does not replay times missed while it was stopped. Daylight-saving gaps and repeated times are handled deterministically.

### After each completed frame fetch

```toml
[advancement]
type = "every_fetch"

[advancement.settings]
frame_client_ips = ["192.168.1.50"]
```

A successful qualifying `GET /frame/current.jpg` advances after the server finishes the response body. `HEAD`, previews, health checks, and streams dropped before server-side completion do not advance.

Set `frame_client_ips` to the frame's IP so browser requests do not advance the queue. An empty list means **every direct client IP qualifies**. Matching uses the direct TCP peer and deliberately ignores `Forwarded`, `X-Forwarded-For`, and `User-Agent`; when using a reverse proxy, the direct peer will normally be the proxy.

HTTP completion cannot prove that a device received, decoded, or displayed the image. It only means the server's response stream completed.

## Configuration files and storage

The small bootstrap file selects the listening address and persistent paths:

```toml
bind = "0.0.0.0:8080"
settings_path = "data/settings.toml"
queue_dir = "data/queue"
```

See [`config.example.toml`](config.example.toml). Relative paths are resolved from the process working directory. The settings file, its parent directory, and the queue must be writable; source photos and the DigiKam catalog should be mounted read-only.

The managed queue looks like this:

```text
queue/
  queue-state.toml
  objects/
    <opaque-id>.jpg
```

Do not manually edit, reorder, or remove these files while the server is running. Use the control page. To preserve queue order and supplier history, stop the server and back up the whole queue directory together.

The queue descriptor and settings are atomically replaced. Queue objects use opaque names and immutable metadata. Startup reconciles safe temporary/orphan files and refuses to serve a descriptor that points to a missing, unsafe, or corrupt object.

Use three separate storage locations:

1. Bootstrap configuration (`config.toml`).
2. Writable persistent settings and queue storage.
3. Read-only source folder, or DigiKam database and photo mounts.

The settings file and source roots must not be inside the queue, and the queue must not be inside a source root.

## Upgrade from older settings

Back up the settings and old cache before upgrading if you may need to roll back.

The loader accepts:

- Legacy untagged DigiKam settings.
- Earlier tagged `digikam_sqlite` settings.
- Earlier tagged `folder_sequence` settings.
- `cache_dir` as the old bootstrap name for `queue_dir`.

An old DigiKam `current.jpg` is imported as the first queue image when valid. Old DigiKam timezone/rollover settings become one scheduled time. Old `first`/`daily_random` selection no longer controls supply; SQL query order does.

Old folder mode becomes a read-only manual-folder supplier with completed-fetch advancement. Source filenames are no longer renamed. A successful save from the control page writes the new canonical settings format.

## Troubleshooting

### The control page says “not ready”

The queue has no valid current image. Check the **Refill** error, then verify that:

- The source path/database/mapped roots are accessible to the service user.
- The manual source path is absolute and separate from the queue.
- JPEGs are valid and, for a manual source, directly inside the configured folder.
- The DigiKam query returns the four required columns.

After fixing the issue, click **Refresh supplier and refill**.

### A removed image does not come back

This is expected: imported-candidate history prevents immediate repeats. Click **Reset supplier history** only if you want old source images to become eligible again.

### Browser testing changes the current image

Under `every_fetch`, configure `frame_client_ips` with only the frame's direct IP. Use queue preview links for non-advancing inspection. If the list is empty, every direct browser GET qualifies.

### DigiKam returns rows but nothing is queued

Check server logs and verify that each returned `root_path` exactly matches a `database_root`, the corresponding `mounted_root` exists, and `album_path` plus `file_name` resolves to a readable JPEG beneath it.

### A queue/settings action is rejected as stale

Reload `/settings` and retry. Another action changed the generation shown in your form.

## Operations and security

- Run one process/replica only; storage is not designed for concurrent writers.
- Run as a non-root user with writes limited to the settings and queue locations.
- Give SIGTERM enough grace for in-flight image responses and state writes.
- `GET /health/live` reports process liveness.
- `GET /health/ready` returns `200` only when the current managed JPEG can be served.
- Use persistent storage for the settings file and queue root.

There is no authentication, authorization, or TLS. CSRF and stale-generation checks protect the control page from accidental/cross-site mutations but are not access control. Keep the service on a trusted network or use an authenticating TLS proxy.

## HTTP reference

Most users only need `/settings` and `/frame/current.jpg`.

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/settings` | Control page and complete settings update |
| `GET` / `HEAD` | `/frame/current.jpg` | Current frame image |
| `GET` | `/queue/thumbnail/{id}` | Non-advancing thumbnail |
| `GET` | `/queue/preview/{id}` | Non-advancing full preview |
| `POST` | `/queue/reorder` | Save complete current-first order |
| `POST` | `/queue/remove` | Remove any queue entry |
| `POST` | `/queue/upload` | Append uploaded JPEG copies |
| `POST` | `/queue/advance` | Advance when a successor exists |
| `POST` | `/supplier/refresh` | Scan/refill now |
| `POST` | `/supplier/reset-seen` | Clear imported-candidate history |
| `GET` | `/health/live` | Process liveness |
| `GET` | `/health/ready` | Current image readiness |

Control-page mutations are bounded, CSRF-protected, stale-generation checked, and use POST/Redirect/GET.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```
