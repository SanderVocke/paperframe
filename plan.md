# DigiKam Daily Selection and Persistent Settings Plan

## Goals

Advance the existing Rust server into a single-process application that:

- Selects a daily JPEG from a read-only DigiKam SQLite catalog using a user-configurable query and selection mode.
- Copies the selected source into an application-owned cache and serves only that cached copy.
- Provides a web settings page that can edit and persist the DigiKam query and other mutable selection settings.
- Preserves the existing frame request tracking and HTTP cache behavior while allowing runtime status to reset on restart.
- Is operable in a container and documented sufficiently for a Kubernetes administrator, without adding Kubernetes resources or deployment automation.

## Scope

Included:

- SQLite-only DigiKam catalog access.
- A small immutable bootstrap configuration for bind address, settings-file path, and cache directory.
- Atomically persisted mutable settings for database path, SQL query, path mappings, randomization, timezone, rollover time, and candidate limit.
- `GET /settings` and `POST /settings` with validation, safe HTML rendering, CSRF protection, and lost-update protection.
- Daily/startup/settings-change selection in a background Tokio task within the server process.
- Deterministic daily randomization or query-order selection.
- Read-only source handling and atomic cache replacement.
- Liveness/readiness endpoints and container/Kubernetes-administrator documentation.

Not included:

- Kubernetes YAML, Helm charts, operators, sidecars, leader election, or support for multiple replicas.
- MariaDB/MySQL or other catalog backends.
- Image effects, resizing, transcoding, or non-JPEG output.
- Authentication, authorization, TLS, or Internet-facing hardening.
- Persisting request history, scheduler status, or selected-image metadata across restarts.
- Modifying DigiKam records or source image files.

## Immutable acceptance criteria

1. The application remains one Rust process and continues to shut down gracefully on Ctrl-C/SIGTERM.
2. Startup uses a documented bootstrap TOML file containing `bind`, `settings_path`, and `cache_dir`; mutable settings are loaded from a separate documented TOML file on disk.
3. Mutable settings include the DigiKam SQLite path, one SQL query, database-to-mounted-path mappings, selection mode (`first` or deterministic `daily_random`), random seed, timezone, daily rollover time, and a candidate limit.
4. `GET /settings` renders current mutable settings and runtime status, including the last frame GET and latest selection result/error. `POST /settings` can change at least the database query and all other mutable settings, validates the complete submission, atomically persists it, updates the live application, triggers reselection, and redirects after success.
5. Invalid, stale-version, oversized, or CSRF-invalid settings submissions do not alter the settings file or active settings and return an appropriate client error with actionable feedback.
6. A successful settings change survives a process restart. Runtime request/selection status may reset on restart.
7. DigiKam is opened with SQLite read-only flags, creation disabled, and query-only mode. The configured SQL must be one read-only `SELECT`/CTE and return the required aliases `image_id`, `root_path`, `album_path`, and `file_name`; mutation attempts and malformed result contracts are rejected.
8. SQLite work does not block Tokio executor threads, and database/query failures are reported without crashing the HTTP server or discarding a previously cached image.
9. `first` selection follows query order. `daily_random` deterministically ranks candidates from the configured seed, local selection date, and `image_id`, remains stable across restarts for the same inputs, and can change at the configured timezone/rollover boundary.
10. Selection runs at startup, at each daily boundary, and after a successful relevant settings update; overlapping selection runs are prevented. Failures retain the previous cache and are retried without busy-looping.
11. Every candidate path is normalized through an explicit path mapping, rejects traversal or paths outside configured mounted roots, and is opened read-only. Application code never creates, writes, renames, removes, hard-links, or symlinks a DigiKam source image.
12. The cache directory is validated as separate from all source roots. Cache publication always copies bytes through an application-owned temporary file in the cache directory and atomically renames it to `current.jpg`; it never links to a source. Failed or interrupted copies leave the prior cache intact.
13. Only JPEG candidates are published. If a newly selected image has the same content digest as the current cache, the cache file is left unchanged so its `Last-Modified` value is preserved.
14. `GET`/`HEAD /frame/current.jpg` serve the cached file, retain the existing `Last-Modified`/`If-Modified-Since` semantics, and retain direct-peer last-GET tracking. A missing cache is handled without panic and makes readiness fail.
15. `GET /health/live` returns success while the HTTP process is running. `GET /health/ready` returns success only when a readable current cache exists, and otherwise returns `503 Service Unavailable`.
16. Automated tests cover settings persistence and web updates, SQLite/query read-only enforcement and result contracts, both selection modes and rollover calculations, path confinement, source immutability, atomic cache behavior, scheduler triggering/failure retention, health endpoints, and all existing frame endpoint behavior.
17. Documentation covers local/container operation and gives a Kubernetes administrator the required process, probe, volume, permissions, replica, SQLite WAL/snapshot, read-only mount, writable settings/cache, and security-boundary information without adding Kubernetes deployment artifacts.

## Design rules and constraints

- Keep the HTTP server and selector/scheduler as tasks in one Tokio process. Initial deployments must use one replica; do not add distributed coordination.
- Separate immutable bootstrap configuration from mutable settings. The bootstrap file may be read-only; the settings file and cache directory must be writable application-owned storage.
- Persist settings with a same-directory `create_new` temporary file, flush/sync, and atomic rename. Serialize writes and use a settings generation/version field to reject lost updates.
- Treat settings changes transactionally: validate structure, paths, timezone/time, SQL statement type/read-only status, and required result columns before persistence or live publication.
- Use `rusqlite` on `spawn_blocking` (or an equivalently isolated blocking worker), opening a fresh read-only connection for catalog operations. Enable neither extension loading nor database creation.
- Enforce query safety at multiple layers: one-statement `SELECT`/CTE validation, SQLite's statement-read-only check, read-only open flags, and `PRAGMA query_only = ON`.
- The query only filters/orders candidates; randomization is implemented in Rust. Bound candidate materialization by the configured positive limit.
- Define the local selection date from the configured IANA timezone and rollover time. Inject clock/scheduling dependencies in tests rather than relying on wall-clock sleeps.
- Keep source and cache capabilities separate. Source APIs return readers only; cache APIs alone may create/write/rename/remove files, and only beneath the canonical cache root.
- Normalize DigiKam album paths deliberately, reject `..`, canonicalize candidates, and verify containment beneath the mapped mounted root before opening.
- Never use filesystem copy/link APIs that could target a source path, and never expose a source path as an output path to current or future processing code.
- Stream source bytes to the cache temporary file while computing a cryptographic digest. Validate JPEG content before publication; image transformation remains out of scope.
- Preserve an existing cache on all database, candidate, validation, copy, and scheduler errors. Runtime status must distinguish stale-but-servable cache from no cache.
- Use a template mechanism with automatic HTML escaping for settings/status output. Bound form body size, use a per-process CSRF token, and follow POST/Redirect/GET after successful updates.
- Keep settings mutation unauthenticated only within the explicitly documented trusted-LAN boundary; do not imply CSRF protection is authentication.
- Continue deriving frame identity from the direct TCP peer and ignore forwarding headers.
- Do not add Kubernetes manifests. Documentation may describe fields/settings and conceptual volume/probe/security requirements.
- Commit `Cargo.lock` changes because this is an application.

## Implementation stages

### Stage 1 — Bootstrap configuration and persistent settings foundation

- [x] Refactor the current `Config` into immutable bootstrap fields (`bind`, `settings_path`, `cache_dir`) and define strongly typed mutable settings with validation/default-free example values.
- [x] Add `settings.example.toml` and update `config.example.toml` to reflect the split.
- [x] Implement settings loading and atomic same-directory persistence, including cleanup/handling of abandoned temporary files and clear startup errors.
- [x] Add a generation-aware in-memory settings snapshot suitable for concurrent handler/scheduler reads and serialized updates.
- [x] Test valid/invalid bootstrap and settings parsing, atomic replacement, write failures, generation conflicts, and reload after a simulated restart.
- [x] Verify formatting, strict linting, tests, and startup with temporary bootstrap/settings files.
- [x] Commit the configuration/settings foundation stage.

### Stage 2 — Read-only DigiKam SQLite catalog

Depends on Stage 1.

- [x] Add a catalog module using SQLite read-only open flags, disabled creation/extension loading, query-only mode, and blocking-work isolation from Tokio.
- [x] Define the candidate/result contract (`image_id`, `root_path`, `album_path`, `file_name`) and validate that configured SQL is exactly one read-only `SELECT`/CTE with those aliases.
- [x] Enforce the candidate limit and produce concise user-facing validation/runtime errors without exposing unnecessary internals over HTTP.
- [x] Build representative temporary DigiKam-schema SQLite fixtures for catalog tests.
- [x] Test successful candidate reads, malformed SQL, multiple statements, mutation/DDL/PRAGMA attempts, missing aliases/type errors, limits, unavailable databases, and unchanged database bytes/no journal creation after reads.
- [x] Verify formatting, strict linting, and all tests.
- [x] Commit the read-only catalog stage.

### Stage 3 — Source confinement, selection, and cache publication

Depends on Stages 1–2.

- [x] Implement path mapping/normalization and canonical containment checks, including defined handling of DigiKam leading separators and rejection of traversal, unmapped roots, missing files, and symlink escapes.
- [x] Introduce separate read-only source and writable cache modules; validate at startup/settings update that canonical cache storage is outside every mounted source root.
- [x] Implement `first` and deterministic `daily_random` candidate ranking, including timezone/rollover-derived selection dates and deterministic tie-breaking.
- [x] Implement bounded fallback through ranked candidates when a candidate is missing, outside policy, unreadable, or not a JPEG.
- [x] Implement streaming copy/digest/JPEG validation into a unique cache temporary file, sync and atomic `current.jpg` publication, same-digest no-op behavior, and temporary-file cleanup.
- [x] Test selection stability/variation/order, DST and rollover boundaries, path attacks, symlinks, source read-only opening, source byte/mtime preservation, no links, partial-copy failure retention, atomic replacement, and unchanged-digest mtime preservation.
- [x] Verify formatting, strict linting, and all tests.
- [x] Commit the selection/source/cache stage.

### Stage 4 — Scheduler, runtime state, cached image serving, and health

Depends on Stages 1–3.

- [x] Extend application state with ephemeral selector status, active settings generation, last frame request, and cache readiness while keeping locks out of database/file I/O.
- [x] Add a single-flight background selector that runs on startup, recalculates the next configured rollover, responds to settings-change notifications, and retries failures with bounded backoff.
- [x] Preserve and report an old cache when reselection fails; make startup usable with an existing cache even when DigiKam is temporarily unavailable.
- [x] Change the frame handlers to serve only `<cache_dir>/current.jpg` while preserving byte, HEAD, conditional GET, direct-peer, and status-recording behavior.
- [x] Add `/health/live` and `/health/ready` with the specified status semantics.
- [x] Test startup/settings/rollover triggers with an injected clock, single-flight behavior, failure retention/retry, shutdown cancellation, cache readiness transitions, health responses, and regression coverage for the frame endpoint.
- [x] Verify formatting, strict linting, and all tests.
- [x] Commit the scheduler/runtime/health stage.

### Stage 5 — Editable settings and status web page

Depends on Stages 1–4.

- [x] Replace the minimal page with an automatically escaped template showing mutable fields, current generation, latest selection status/error, cache readiness, and last frame GET without exposing source paths unnecessarily.
- [x] Implement bounded form parsing for `POST /settings`, including generation and per-process CSRF tokens.
- [x] Validate the complete proposed settings and SQLite query contract before atomically persisting, swapping the live snapshot, and notifying the scheduler.
- [x] Use POST/Redirect/GET on success and render actionable field/global errors without changing active/disk settings on failure.
- [x] Test HTML escaping, query editing and persistence, successful reselection notification, all validation failures, oversized forms, CSRF rejection, stale-generation conflict, concurrent submissions, and restart survival.
- [x] Verify formatting, strict linting, and all tests.
- [x] Commit the settings UI stage.

### Stage 6 — Operational documentation and safety review

Depends on Stages 1–5.

- [ ] Update the README for local startup, bootstrap versus mutable settings, the query result contract/default DigiKam query, path mappings, selection modes/timing, cache semantics, settings UI, health endpoints, and failure behavior.
- [ ] Document the source non-modification policy and defence layers, including read-only/no-atime-capable photo mounts, a separate writable cache/settings volume, non-root execution, and the prohibition on links/output paths into the library.
- [ ] Add a Kubernetes-administrator section describing one replica, one process, ConfigMap-suitable bootstrap config, persistent writable settings/cache, read-only SQLite/photo mounts, probe paths, UID/file permissions, read-only root filesystem, graceful termination, and the absence of auth/TLS.
- [ ] Document the live SQLite WAL/SHM consistency caveat and recommend a consistent snapshot or safely exposed read-only catalog when DigiKam writes concurrently.
- [ ] Do not add Kubernetes manifests, charts, or deployment automation.
- [ ] Audit dependencies and all file-opening/mutation call sites; demonstrate that source/database modules cannot obtain write/create access and cache/settings mutations remain confined to their roots.
- [ ] Verify formatting, strict linting, the full test suite, and a release build.
- [ ] Commit the documentation and safety-review stage.

### Stage 7 — Final end-to-end validation

Depends on all previous stages.

- [ ] From a clean checkout, create a temporary representative DigiKam SQLite catalog, read-only photo tree, writable settings/cache directories, and documented bootstrap/settings files.
- [ ] Record source image and database hashes/metadata, start the one-process release binary, and verify liveness plus initial not-ready/no-cache behavior.
- [ ] Verify startup selection creates an independent cached JPEG, readiness becomes successful, and source/database hashes and relevant metadata remain unchanged.
- [ ] Verify normal GET/HEAD, exact cached bytes, direct-peer status, `Last-Modified`, conditional `304`, and settings-page runtime status.
- [ ] Change the SQL query and randomization settings through the web form; verify atomic on-disk persistence, immediate reselection, deterministic behavior, and survival across restart.
- [ ] Submit invalid/mutating SQL, stale generation, invalid CSRF, traversal/path-escape settings, and an oversized body; verify rejection without disk/live-setting changes or source/database mutation.
- [ ] Simulate database/source/copy failures and verify the old cache remains served, readiness/status are accurate, and retry does not busy-loop.
- [ ] Inspect the cache artifact to prove it is neither a hard link nor symlink to a source and that all temporary files are confined/cleaned.
- [ ] Send SIGTERM during normal operation and during selection; verify graceful process exit and intact settings/cache/source/database files.
- [ ] Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, the full test suite, and `cargo build --release`; record exact commands/results in the implementation summary.
- [ ] Commit any final validated fixes as a meaningful milestone and leave the working tree clean.

## Execution contract

- Keep the plan updated as work progresses and check off completed items.
- Commit each completed stage or meaningful milestone.
- Implementation steps may be revised when new evidence warrants it.
- Design rules may be revised for a documented, well-supported reason.
- Goals and acceptance criteria must not be changed without explicit user approval.
