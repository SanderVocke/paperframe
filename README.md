# Paper frame server

A single-process Rust service that maintains an application-owned JPEG queue and serves its current entry to a PaperlessPaper frame. Image **suppliers** only copy suitable images into the queue; a separate **advancement policy** decides when the current entry is consumed.

Supported suppliers:

- **DigiKam SQLite** — read-only SQL query order.
- **Manual source folder** — read-only, top-level alphabetical discovery with polling and explicit refresh.

Supported advancement policies:

- `every_fetch` — advance after a qualifying frame response body is fully consumed by the HTTP server.
- `scheduled` — advance at one or more configured local daily times.
- `manual` — advance only through the settings page.

Only one supplier may currently be configured, but suppliers are registered through a collection-shaped supplier-neutral coordinator so mixed supply can be added without changing queue or serving APIs.

## HTTP endpoints

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/settings` | Queue/status view and complete settings update |
| `GET` / `HEAD` | `/frame/current.jpg` | Current queue image |
| `GET` | `/queue/thumbnail/{id}` | Non-advancing queue thumbnail |
| `GET` | `/queue/preview/{id}` | Non-advancing full preview |
| `POST` | `/queue/reorder` | Replace the complete current-first order |
| `POST` | `/queue/remove` | Remove any entry, including current |
| `POST` | `/queue/upload` | Append JPEG copies directly to the queue |
| `POST` | `/queue/advance` | Explicitly consume current when a successor exists |
| `POST` | `/supplier/refresh` | Request supplier discovery/refill now |
| `POST` | `/supplier/reset-seen` | Clear imported-candidate history |
| `GET` | `/health/live` | Process liveness |
| `GET` | `/health/ready` | `200` while a valid current queue entry is servable |

All mutations are bounded, CSRF-protected, generation checked, and use POST/Redirect/GET.

## Setup

```sh
cp config.example.toml config.toml
mkdir -p data/queue data/source-images
# Choose one example supplier:
cp settings.example.toml data/settings.toml
# cp settings.folder.example.toml data/settings.toml
cargo run --release -- config.toml
```

The bootstrap path defaults to `config.toml`:

```toml
bind = "0.0.0.0:8080"
settings_path = "data/settings.toml"
queue_dir = "data/queue"
```

Legacy `cache_dir` is accepted as an alias for `queue_dir`. The bootstrap file is immutable while running. The settings file, its parent, and queue root must be writable.

## Queue model

The queue root is fully application managed:

```text
queue/
  queue-state.toml
  objects/
    <opaque-id>.jpg
```

`queue-state.toml` is a small versioned descriptor containing generation, current-first order, immutable object metadata, supplier provenance/source identities, and imported-candidate history. It and settings are written through same-directory `create_new` temporaries, file sync, atomic rename, and directory sync. Queue object filenames never encode order.

Entry zero is the current image and is otherwise ordinary:

- Reordering all entries may change current immediately.
- Removing current promotes the next entry.
- Advancing consumes current only if another entry exists.
- If only one entry remains, advance/fetch attempts retain and continue serving it while notifying refill.
- Explicitly removing the sole entry empties the queue; frame GET returns `404` and readiness returns `503`.

Queue copies are immutable snapshots. Once imported, later source modification, rename, deletion, or supplier failure cannot alter them. Direct uploads may exceed `target_length`; the target (default 10) is a refill minimum, not a cap.

At startup, recognized temporaries/orphans are reconciled. A descriptor referencing a missing, unsafe, or corrupt object fails safely rather than silently serving a different file.

## Refill and seen state

Every committed enqueue, reorder, removal, advance, history reset, and settings change emits a revisioned event. Startup, source polling, explicit refresh, and blocked advancement also request reconciliation. The single-flight coordinator coalesces events, re-reads queue length, and asks suppliers only for the current deficit. Supplier-generated enqueue events therefore settle as a no-op once target is reached rather than looping.

A candidate is marked seen only in the same descriptor publication that enqueues its copy. Removing it does not immediately make the supplier add the same source back; the supplier continues with the next unseen candidate. History survives restart and can be reset explicitly from the web UI.

Failures retain the existing queue and appear in status. Refill retries are bounded and never blocks frame body transfer.

## DigiKam supplier

DigiKam settings retain:

- SQLite database path
- One read-only query
- Explicit database-to-mounted path mappings
- Candidate limit

The query must be one `SELECT`/CTE returning `image_id`, `root_path`, `album_path`, and `file_name`. Candidates are supplied strictly in query order. SQLite uses read-only/no-create flags, query-only mode, statement-read-only validation, no extension loading, and blocking-worker isolation.

Mapped photos are canonicalized beneath read-only mounted roots. Traversal, multi-component filenames, symlink escapes, missing files, and invalid JPEGs are rejected. DigiKam no longer owns daily selection, current image, or cache publication.

## Manual-folder supplier

The configured source folder is separate from the queue and treated as externally mutable but read-only:

- Portable periodic polling plus **Refresh supplier now**
- Top-level regular non-symlink `.jpg`/`.jpeg` files only
- Exact alphabetical filename discovery order
- No recursive traversal
- No source writes, renames, removals, links, or direct serving

Files are read and copied only when queue capacity permits. A changed file has a new content-version identity and may be imported later. Deleted unseen candidates disappear; already queued copies remain untouched. Polling errors retain the queue.

## Advancement behavior

### Every completed frame fetch

`every_fetch` returns `Cache-Control: no-store` and a body rather than `304`. A qualifying request holds a serialization permit, prepares the current immutable queue object, and wraps the response body with completion tracking. Only successful body EOF schedules `advance_if_current(entry_id)`; a dropped/disconnected body, HEAD, preview, thumbnail, health request, or bodyless response does not advance. Entry-ID comparison prevents an in-flight completion from removing a different current entry after a web mutation.

The optional `frame_client_ips` list matches the direct TCP peer. Forwarding and `User-Agent` headers are ignored. Empty preserves compatibility by treating every direct peer as a frame client. Configure the frame's IP and use `/queue/preview/{id}` for browser testing when distinction matters.

“Body complete” means the HTTP stack fully polled the response stream. It cannot prove that bytes reached the panel, decoded successfully, or were displayed.

### Scheduled

Scheduled policy takes an IANA timezone and one or more unique `HH:MM` values. It calculates only the next future event (no missed-event replay), handles DST gaps/ambiguity deterministically, recalculates after settings changes, and uses the same queue advance operation. Between advances, the stable current image supports `Last-Modified` and conditional `304` responses.

### Manual

Manual policy never advances on GET. The stable current image has the same conditional behavior; **Advance now** invokes the common operation. That button remains available under every policy.

## Compatibility migration

The loader accepts:

- Legacy untagged DigiKam settings.
- The previous tagged `digikam_sqlite` settings.
- The previous tagged `folder_sequence` settings.
- Legacy bootstrap `cache_dir`.

DigiKam migrates to SQL-order supply and maps its previous timezone/rollover to one scheduled time. Previous `first`/`daily_random` source selection is removed from canonical settings. A valid legacy DigiKam `current.jpg` is imported as the first managed queue object before refill.

Previous folder mode migrates to a manual read-only supplier with `every_fetch`; existing source filenames are no longer renamed. Its old folder remains a source, while queue copies live under `queue_dir`. The next successful settings update persists canonical format. Back up writable settings/cache storage before upgrading if rollback to an older binary is required.

## Storage, deployment, and security

Use distinct storage capabilities:

1. Immutable bootstrap configuration.
2. Writable persistent settings file and queue root.
3. Read-only DigiKam database/photo mounts or read-only manual source mount.

The settings file and every source root must be separate from the queue. Run one process and one replica as a non-root UID with a read-only container root filesystem, dropped capabilities, and writes only on intended volumes. Use `/health/live` and `/health/ready`; allow SIGTERM grace for in-flight body/refill/state operations. A live SQLite WAL catalog requires consistent WAL/SHM exposure; prefer a safe snapshot.

There is no authentication, authorization, or TLS. CSRF and generation checks are not authentication. Keep the service on a trusted LAN or behind an authenticating TLS proxy. Direct-peer classification intentionally ignores forwarding headers, so configure networking accordingly.

No Kubernetes manifests or deployment automation are included.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```
