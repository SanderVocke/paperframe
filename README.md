# Paper frame server

A single-process Rust service that selects one daily JPEG from a DigiKam SQLite catalog, copies it into an application-owned cache, and serves the cached copy to a PaperlessPaper frame running the offline firmware. It is intended for a trusted home LAN.

## HTTP endpoints

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/settings` | Runtime status and editable selection settings |
| `POST` | `/settings` | Validate and persist the complete settings form |
| `GET` | `/frame/current.jpg` | Current cached JPEG |
| `HEAD` | `/frame/current.jpg` | Current JPEG metadata without changing last-GET state |
| `GET` | `/health/live` | `200` while the HTTP process is running |
| `GET` | `/health/ready` | `200` when a readable current cache exists; otherwise `503` |

## Storage model

The application uses three distinct kinds of storage:

1. **Bootstrap configuration:** immutable process settings (`bind`, `settings_path`, `cache_dir`).
2. **Mutable settings:** an atomically replaced TOML file edited through `/settings`.
3. **Image cache:** application-owned writable storage containing `current.jpg`.

The DigiKam database and photo library are inputs and are always opened read-only. Runtime status such as the last frame request and latest selector error is in memory and resets on restart. The settings file and cached JPEG remain on disk.

## Local setup

A current stable Rust toolchain, a DigiKam SQLite database, and access to its photo files are required.

```sh
cp config.example.toml config.toml
mkdir -p data/cache
cp settings.example.toml data/settings.toml
```

Edit both files for the local paths, then run:

```sh
cargo run --release -- config.toml
```

The command-line configuration argument defaults to `config.toml`. Relative bootstrap paths are resolved from the process working directory. `RUST_LOG` controls structured logging, for example `RUST_LOG=debug`.

### Bootstrap configuration

```toml
bind = "0.0.0.0:8080"
settings_path = "data/settings.toml"
cache_dir = "data/cache"
```

The bootstrap file is read only at startup. Changing the bind address or storage topology requires a restart.

### Mutable settings

See [`settings.example.toml`](settings.example.toml) for a complete file. It contains:

- `database_path`: DigiKam SQLite database path visible to the process.
- `query`: candidate selection query.
- `path_mappings`: translation from paths recorded by DigiKam to mounted paths.
- `selection.mode`: `first` or `daily_random`.
- `selection.random_seed`: stable per-frame randomization seed.
- `selection.timezone`: IANA timezone such as `Europe/Amsterdam`.
- `selection.rollover_time`: local `HH:MM` daily boundary.
- `max_candidates`: upper bound on query results held for selection.

The settings file must already exist and its parent directory must be writable. Successful web updates use a same-directory temporary file, file/directory sync, and atomic rename. A generation field prevents concurrent browser submissions from silently overwriting each other.

## DigiKam query contract

The configured SQL must be exactly one read-only SQLite `SELECT` or CTE and return these aliases:

| Alias | Type | Meaning |
|---|---|---|
| `image_id` | integer | Stable DigiKam image identity |
| `root_path` | text | Album-root path stored by DigiKam |
| `album_path` | text | Path relative to the album root; a leading slash is accepted |
| `file_name` | text | One filename component |

The example query joins DigiKam's `Images`, `Albums`, and `AlbumRoots` tables and filters for JPEG names. Customize its `WHERE` and `ORDER BY` clauses for ratings, tags, albums, dates, or other desired DigiKam metadata.

Query safety is enforced with SQL statement parsing, SQLite's statement-read-only check, `SQLITE_OPEN_READ_ONLY`, disabled database creation/extension loading, and `PRAGMA query_only = ON`. SQLite work runs outside Tokio executor threads. The application never runs schema migrations or stores its own state in DigiKam.

### Path mappings

DigiKam generally stores host paths that differ from paths inside a container. A mapping translates the database root while keeping path choice out of the cache writer:

```toml
[[path_mappings]]
database_root = "/home/user/Pictures"
mounted_root = "/photos"
```

Candidate album traversal, multi-component filenames, unmapped roots, missing files, and canonical paths escaping the mounted root are rejected. The cache directory must be separate from every mounted source root.

## Daily selection

`first` uses the first usable candidate in query order. `daily_random` hashes the random seed, local selection date, and DigiKam image ID. It is deterministic for the same day and inputs, independent of query row order, and suitable for regeneration after a restart.

Selection runs:

- immediately at startup;
- at the next configured local rollover, including timezone/DST handling;
- immediately after a successful settings update;
- after a bounded retry delay when selection fails.

Only one selection runs at a time. Missing, unsafe, unreadable, and non-JPEG candidates are skipped in rank order. Database/query/source/copy errors leave the prior cache in place and appear on `/settings`. If no cache exists, the image endpoint returns `404` and readiness returns `503`.

## Cache and source-image safety

Source files are never served directly. The source module resolves a candidate beneath a configured mounted root and opens it with read-only `OpenOptions`. It exposes a reader, not an output path. Only the cache module has create/write/rename/remove operations.

For each selected JPEG, the application:

1. Opens the source read-only.
2. Streams bytes into a unique `create_new` temporary file under the cache directory while computing SHA-256.
3. Checks JPEG start/end markers.
4. Flushes and syncs the temporary file.
5. Atomically renames it to `current.jpg` and syncs the cache directory.

It never hard-links or symlinks a source into the cache. A failed copy leaves the old cache intact. If the digest matches the current cache, `current.jpg` is not replaced, preserving `Last-Modified` and avoiding an unnecessary e-ink refresh.

The application safeguards are defence in depth, not a substitute for filesystem controls. Mount the entire photo library read-only, preferably with no-atime behavior where the storage driver supports it. Use a separate writable volume for settings/cache, run as a non-root UID, and do not grant capabilities that can bypass mount permissions.

Image transformation is intentionally not implemented yet. Future effects must retain the same `read-only source reader -> cache-owned output writer` boundary.

## Frame behavior

Configure the offline firmware image URL as:

```text
http://SERVER_LAN_IP:8080/frame/current.jpg
```

Responses include `Content-Type: image/jpeg`, `Content-Length`, and `Last-Modified`. `If-Modified-Since` can produce a bodyless `304 Not Modified`. Both `200` and `304` GETs update the status page; HEAD does not.

The recorded client address is the direct TCP peer. Forwarding headers are ignored. A successful GET means the server handled the request; it does not prove that the frame received every byte, decoded the JPEG, or refreshed the panel.

Example checks:

```sh
curl --dump-header /tmp/frame.headers \
  --output /tmp/current.jpg \
  http://127.0.0.1:8080/frame/current.jpg
curl --head http://127.0.0.1:8080/frame/current.jpg
curl --include \
  --header "If-Modified-Since: $(awk 'BEGIN { IGNORECASE=1 } /^Last-Modified:/ { sub(/^[^:]*: /, ""); sub(/\r$/, ""); print }' /tmp/frame.headers)" \
  http://127.0.0.1:8080/frame/current.jpg
```

## Settings-page security

The settings form is HTML-escaped, body-size bounded, protected against cross-site form submission by a per-process CSRF token, and protected against lost updates by a generation value. These controls are not authentication.

There is no user authentication, authorization, or TLS. Anyone who can reach the service can view and attempt to change its settings. Keep it on a trusted LAN or place an authenticating/TLS reverse proxy in front of it. Do not expose it directly to the Internet.

## Notes for a Kubernetes administrator

This repository deliberately contains no Kubernetes manifests, Helm chart, operator, or deployment automation. An administrator can package/deploy the release binary using these requirements:

- Run exactly one application process and one replica. There is no leader election or multi-writer cache coordination.
- Put the bootstrap file in immutable configuration storage and pass its path as the sole argument.
- Put the mutable settings file and cache directory on writable persistent storage. They may share one small volume but must remain outside all photo mounts.
- Mount the DigiKam database and every mapped photo root read-only. Keep the container root filesystem read-only and provide writes only for settings/cache.
- Run as a non-root UID/GID that can read the catalog/photos and write settings/cache. Disable privilege escalation and drop Linux capabilities.
- Use `/health/live` for liveness and `/health/ready` for readiness.
- Use one replica and a termination grace period sufficient for an in-flight source copy. The process handles SIGTERM gracefully and leaves published cache/settings files intact.
- Restrict service/ingress access to the trusted LAN, or provide authentication and TLS externally.

A read-only mount of a live SQLite database needs special care. If DigiKam uses WAL mode, a consistent view may require the matching `-wal` and `-shm` files and storage semantics that support read-only access correctly. Mounting only the main file while DigiKam writes can be inconsistent. Prefer a consistent SQLite snapshot/backup, or otherwise arrange a safely exposed read-only catalog. The selector retains the previous cache when the catalog is temporarily unavailable.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```
