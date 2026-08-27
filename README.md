# Paper frame server

Paper frame server keeps a rotating list of photos for a PaperlessPaper frame, converts them to the configured frame representation, and serves the current image over HTTP.

You choose:

1. **Where images come from** — a normal folder or a DigiKam catalog.
2. **When the frame moves to the next image** — after a completed frame download, at daily times, or only when you click **Advance now**.

The server copies selected images into its own managed queue. It never renames, removes, or modifies your source photos or DigiKam database. Once queued, an image is unaffected if its source is later edited or deleted.

> [!IMPORTANT]
> The service has no login or TLS. Run it only on a trusted LAN, or put it behind an authenticating HTTPS proxy.

## Quick start with a folder

### 1. Prepare the files

You need Rust 1.85 or newer and this repository. The server accepts common raster formats and converts them at enqueue time. From the repository root:

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
advance_behavior = "drop_and_refill"

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

Put supported raster images directly in `data/source-images`. Subfolders are not scanned.

### 2. Start the server

```sh
cargo run --release -- config.toml
```

The configuration path is optional and defaults to `config.toml`. To enable debug logging, set `RUST_LOG=debug` before the command.

### 3. Open the control page and connect the frame

- Control page: `http://SERVER_IP:8080/settings`
- Frame image (stable, backward-compatible URL): `http://SERVER_IP:8080/frame/current.jpg`

Replace `SERVER_IP` with the computer's LAN address. Configure the frame to fetch the frame-image URL. Despite the `.jpg` suffix, this endpoint serves the configured current representation: `image/bmp` in server-dither mode or `image/jpeg` in direct-JPEG mode. `/frame/current.bmp` is an alias for the same response. The control page shows readiness, refill errors, the current image, upcoming images, and all queue/settings controls.

The quick-start policy is `manual`, so test downloads cannot unexpectedly move the queue. After confirming that the frame works, choose another policy on the control page if wanted.

## Image processing and orientation

The server has two global output modes:

- **Server dither BMP** decodes the source and applies a pure-Rust port of the PaperlessPaper web editor's default **Auto-Optimieren** flow: image-specific analysis and adjustments, the editor's calibrated six-color palette, and serpentine Floyd–Steinberg diffusion. It stores a ready-to-display 4-bit BMP. Configure either `/frame/current.bmp` or the backward-compatible `/frame/current.jpg`; the latter may return `image/bmp` because the firmware detects the body by its `BM` signature.
- **Direct JPEG** preserves a suitable JPEG without recompression and converts other sources to JPEG. The frame performs its normal on-device processing.

Select Paper 7 (`800×480`) or Paper L (`1200×1600`) in Frame settings. Existing settings without a `[rendering]` section retain the compatibility default of direct JPEG; the supplied new-install examples select server dithering.

The compiled decoder supports JPEG, PNG, GIF, WebP, BMP, TIFF, TGA, PNM, DDS, HDR, OpenEXR, ICO, QOI, and Farbfeld. AVIF is not enabled because `image` 0.25's AVIF decoder requires the optional native backend, while this server deliberately retains a pure-Rust production image pipeline. Animated formats use their first frame, metadata orientation is applied where the decoder exposes it, and transparency is composited onto white. Encoded uploads are bounded by `max_upload_bytes`; every supplier/upload source also has a hard 100 MiB encoded limit, and decoded images are dimension/allocation limited.

Orientation is inferred after metadata orientation: taller images are portrait and square images are landscape. Each queue card can override it. Like the offline web editor, the renderer preserves aspect ratio, centers the image on white, and uses the editor's 270° transform when source and display orientations differ. Auto Rotation in the offline firmware still handles only the final 180° correction.

Dither parameters are intentionally not user-editable yet. Profile version 2 uses the web editor's `spectra6Custom` calibrated palette, per-image natural auto optimization, RGB or LAB matching selected by that analysis, the editor's reachable tone/range/paper adjustments, its default Floyd–Steinberg selection, and serpentine scanning. The internal typed profile leaves room for future palette and exposure/contrast/saturation controls. Each queue card links to the original and to a non-advancing lossless dithered preview.

### Text messages

The control page can add a dedicated text message as a queue item or add editable text to an existing photo. A queue card's single **Edit** action opens a larger composition preview with orientation and text controls. Text is laid out with the same bundled Inter font used by the control page in a rounded, opaque color box; black or white foreground is selected automatically for contrast. Choose small, medium, or large text and left/middle/right plus top/middle/bottom placement. The box sizes itself to the text, preserves explicit newlines, and wraps long lines before exceeding 90% of the frame width.

A text edit is a non-destructive recipe change. The server retains the original photo, applies photo adjustments, composites the text, dithers the complete image once, and atomically publishes a stored BMP. Frame requests only read that BMP. Removing text rerenders once from the retained original. Text-bearing entries are always server-dithered BMPs even when ordinary photos use Direct JPEG; `/frame/current.jpg` continues to work because the firmware recognizes the BMP body.

### Install the control page as an app

Paper Frame is a progressive web app. On a supported Android browser, open the control page and use **Install app** when the button appears. The installed app opens `/settings` in a standalone window. A valid HTTPS public origin is required for installation; plain HTTP works only on loopback addresses such as `localhost` during development.

An authenticating reverse proxy such as Authelia is compatible with installation. The installed app shares the browser's cookies and follows the normal login redirect when its session expires. Ensure authenticated requests for `/manifest.webmanifest`, `/sw.js`, and `/pwa/*` return the actual assets rather than a login page. These endpoints contain no private data and can optionally be exempted from authentication, while `/settings`, queue endpoints, previews, and frame images remain protected. The service worker caches only PWA metadata and icons, never authenticated application pages or photos.

## Docker and Kubernetes

A production multi-stage [`Dockerfile`](Dockerfile) and example Kubernetes deployment are included. The image runs as a non-root user, keeps writable queue/settings data under `/data`, and supports read-only photo or DigiKam mounts.

See **[Containers and Kubernetes](docs/containers.md)** for local Docker usage, multi-architecture registry publishing, persistent storage, DigiKam mounts, and a secured single-replica Kubernetes setup.

## Everyday use

### Manage the queue

The first queue entry is always the current image.

- **Advance now** moves to the next entry.
- **Remove** deletes that managed copy. Removing the current entry promotes the next one.
- **Upload photos** accepts any enabled raster format and adds independent source/derived copies to the queue.
- **Add a dedicated text message** creates a normal, reorderable queue entry. Use each photo card's **Add text** or **Edit text** section for overlays; **Remove text** restores the photo-only composition without modifying its retained source.
- **Save order** applies the complete current-first order shown in the text box. Move whole lines and do not alter their opaque IDs.
- **Non-advancing preview** opens an in-page comparison: the undithered original is always shown, and a server-dithered entry also shows its exact six-color result. Each side links to the retained full source or stored frame derivative.

Choose what happens to the previous image under **Frame settings → After moving to the next photo**:

- **Remove the previous photo and find a new one** (`drop_and_refill`, the default) deletes that managed queue copy and refills from the next unseen supplier candidate.
- **Move the previous photo to the end of the queue** (`rotate`) keeps the managed copy and cycles it behind the other queued photos without needing a replacement.

Advancing never consumes or rotates the only remaining image, so the frame keeps displaying it. Removing that sole entry is allowed and empties the queue. An empty queue makes the frame URL return `404` and `/health/ready` return `503`.

`target_length` is a refill target, not a maximum. In drop-and-refill mode supplier images are added until the queue reaches the target; direct uploads may make it longer.

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

- Reads only top-level regular files with enabled raster-image extensions and validates their contents.
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
| `file_name` | Supported raster-image filename only |

The SQL `ORDER BY` controls supply order. Each returned `root_path` must exactly match a `database_root` mapping. `mounted_root` is the corresponding directory on the machine or inside the container running this service.

For example, if DigiKam records `/home/alice/Pictures`, but that library is mounted at `/photos` for the server:

```toml
[[suppliers.settings.path_mappings]]
database_root = "/home/alice/Pictures"
mounted_root = "/photos"
```

The database is opened read-only and query-only. Photos are opened read-only beneath mapped roots. Invalid paths, symlink escapes, missing files, and undecodable images are skipped or reported without changing DigiKam data.

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

A successful qualifying `GET` to either `/frame/current.jpg` or `/frame/current.bmp` advances after the server finishes the response body. `HEAD`, previews, health checks, and streams dropped before server-side completion do not advance.

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
    <opaque-id>.source
    <opaque-id>.<bmp-or-jpg>
  thumbnails/
    <opaque-id>.jpg
```

Do not manually edit, reorder, or remove these files while the server is running. Use the control page. To preserve queue order and supplier history, stop the server and back up the whole queue directory together.

The queue descriptor and settings are atomically replaced. Queue objects use opaque names and immutable metadata. Text recipes are stored in the descriptor; a dedicated message also owns a minimal immutable canvas source so the same transactional source/derivative machinery applies. Thumbnails are bounded, derived cache files; missing thumbnails are regenerated from the immutable source or, for text-bearing entries, the already-dithered derivative. Startup reconciles safe temporary/orphan files and refuses to serve a descriptor that points to a missing, unsafe, or corrupt object.

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
- Images decode in one of the enabled raster formats and, for a manual source, are directly inside the configured folder.
- The DigiKam query returns the four required columns.

After fixing the issue, click **Refresh supplier and refill**.

### A removed image does not come back

This is expected: imported-candidate history prevents immediate repeats. Click **Reset supplier history** only if you want old source images to become eligible again.

### Browser testing changes the current image

Under `every_fetch`, configure `frame_client_ips` with only the frame's direct IP. Use queue preview links for non-advancing inspection. If the list is empty, every direct browser GET qualifies.

### DigiKam returns rows but nothing is queued

Check server logs and verify that each returned `root_path` exactly matches a `database_root`, the corresponding `mounted_root` exists, and `album_path` plus `file_name` resolves to a readable supported image beneath it.

### A queue/settings action is rejected as stale

Reload `/settings` and retry. Another action changed the generation shown in your form.

## Operations and security

- Run one process/replica only; storage is not designed for concurrent writers.
- Run as a non-root user with writes limited to the settings and queue locations.
- Give SIGTERM enough grace for in-flight image responses and state writes.
- `GET /health/live` reports process liveness.
- `GET /health/ready` returns `200` only when the current managed derivative can be served.
- Use persistent storage for the settings file and queue root.

There is no authentication, authorization, or TLS. CSRF and stale-generation checks protect the control page from accidental/cross-site mutations but are not access control. Keep the service on a trusted network or use an authenticating TLS proxy.

## HTTP reference

Most users only need `/settings` and the stable `/frame/current.jpg` endpoint. The `.jpg` and `.bmp` frame paths are aliases; the configured output mode determines the response format.

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/settings` | Control page and complete settings update |
| `GET` | `/manifest.webmanifest` | Progressive web app metadata |
| `GET` | `/sw.js` | Static-assets-only service worker |
| `GET` | `/pwa/*` | Progressive web app icons |
| `GET` / `HEAD` | `/frame/current.jpg` | Stable backward-compatible endpoint; returns the configured JPEG or BMP representation |
| `GET` / `HEAD` | `/frame/current.bmp` | Alias for the same configured representation |
| `GET` | `/queue/thumbnail/{id}` | Non-advancing thumbnail |
| `GET` | `/queue/preview/{id}` | Browser-compatible, undithered original preview |
| `GET` | `/queue/original/{id}` | Retained original source bytes |
| `GET` | `/queue/rendered/{id}` | Stored frame derivative bytes (BMP or JPEG) |
| `GET` | `/queue/dithered-preview/{id}` | Browser-compatible, non-advancing six-color preview |
| `POST` | `/queue/reorder` | Save complete current-first order |
| `POST` | `/queue/remove` | Remove any queue entry |
| `POST` | `/queue/upload` | Decode/render and append uploaded images |
| `POST` | `/queue/message` | Create and render a dedicated text queue item |
| `POST` | `/queue/text` | Add or edit a queue entry's text recipe and stored derivative |
| `POST` | `/queue/text/remove` | Remove a photo's text recipe and rebuild it from its retained source |
| `POST` | `/queue/orientation` | Override and rerender one entry's orientation |
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
