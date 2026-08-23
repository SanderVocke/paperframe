# Generic Image Modes and Sequential Folder Mode Plan

## Goals

Evolve the single-process Rust server so that frame serving, health/status reporting, and the settings interface are mode-independent, while image discovery and advancement behavior are supplied by an active image mode.

- Preserve DigiKam SQLite daily selection as a fully working mode with its current safety and scheduling behavior.
- Add a writable server-folder mode that serves JPEGs sequentially in filename order, advancing once per frame `GET` and wrapping after the last image.
- Make folder contents visible as an ordered thumbnail queue and support safe web-based reordering, upload, and explicit refresh from disk.
- Deliver both modes end-to-end through the existing frame URL and one settings/status interface.

## Scope

Included:

- A mode discriminator and mode-specific settings/runtime abstractions designed to admit future image sources without putting mode branches throughout HTTP handlers.
- Compatibility handling for existing DigiKam settings and preservation of DigiKam query, source-confinement, cache, scheduler, and conditional-request behavior.
- One configured top-level folder containing regular JPEG files; startup scanning and explicit refresh establish the in-memory queue.
- Sequential frame serving, queue/cursor status, thumbnail display, reordering by filesystem rename, and bounded JPEG upload.
- Serialized folder mutations, CSRF protection, stale-inventory protection, safe filenames/path confinement, and actionable errors.
- Updated examples, documentation, unit/integration tests, and release-level end-to-end validation.

Not included:

- Recursive folder traversal, filesystem watching, remote/cloud sources, non-JPEG frame output, resizing/transcoding, or generated thumbnail files.
- Multiple independent frames/cursors, per-client queues, persisted request history, or persistence of the folder cursor across process restarts.
- Authentication, authorization, TLS, multiple replicas, or distributed filesystem coordination.
- Changing DigiKam records or DigiKam-managed source files.

## Immutable acceptance criteria

1. The application remains one gracefully shutting-down Rust process and continues to expose `GET`/`HEAD /frame/current.jpg`, `/settings`, `/health/live`, and `/health/ready`.
2. Settings identify an active image mode and isolate mode-specific configuration behind a clear mode boundary. Existing settings without a mode discriminator have a tested compatibility path to DigiKam mode, and the example configuration documents the canonical new format.
3. DigiKam SQLite remains selectable as a mode and retains its existing read-only database/query enforcement, path confinement, `first`/`daily_random` choices, startup/daily/settings-triggered selection, atomic cache publication, failure retention, and status reporting.
4. Folder mode has a configured writable server-side directory that is canonicalized and validated as separate from the cache and settings file. Only top-level regular, non-symlink JPEG files are eligible; unrelated, nested, temporary, unsafe, and unsupported files are not served.
5. Folder inventory is loaded at startup and only changes through a successful upload, reorder, or explicit web refresh. Refresh detects eligible files added or removed manually on the server and reports scan/validation errors without corrupting the last valid in-memory queue.
6. A newly initialized or refreshed folder queue is sorted deterministically by filename. Its first successful frame `GET` returns the first image, later successful frame `GET`s return successive images, and the sequence wraps after the final image.
7. Folder cursor state is runtime-only. Restart and successful queue mutations reset the next image to the first item in the resulting order; this behavior is shown in status and documented.
8. Each folder-mode frame `GET` atomically reserves/prepares exactly one queue item so concurrent requests cannot accidentally receive the same cursor position or race through a mutable shared `current.jpg`. A failed preparation does not advance the cursor. `HEAD`, thumbnail requests, health checks, and settings actions never advance it.
9. Because folder mode changes the representation on each frame `GET`, its successful `GET`s return the next JPEG body rather than an incorrect `304`, and use explicit response cache policy appropriate for a rotating URL. DigiKam mode retains its current `Last-Modified`/`If-Modified-Since` behavior.
10. An empty folder queue returns `404` from the frame endpoint and `503` from readiness. Folder readiness becomes successful when the active inventory contains a servable image; liveness remains independent of mode readiness.
11. The settings/status page identifies the active mode and renders folder thumbnails in queue order, with an unambiguous next-image marker/cursor. Thumbnail retrieval uses a confined opaque identifier or equivalent safe lookup and does not expose arbitrary filesystem paths.
12. A CSRF-protected reorder action accepts a complete ordering of the current inventory, rejects stale, duplicate, missing, or foreign entries, and renames the underlying files so ordinary alphabetical filename sorting reproduces the submitted order after refresh and restart.
13. Reordering uses collision-safe generated names and a serialized two-phase/rollback-capable rename procedure so existing files are not overwritten and an error is not presented as a successful partial reorder. Temporary names are confined to the folder and recoverable/cleaned on startup or refresh.
14. A CSRF-protected multipart upload accepts one or more bounded JPEG files, rejects traversal names, symlinks, non-JPEG content, oversized bodies/files, and collisions safely, publishes through `create_new` temporary files plus sync/rename, appends successful uploads deterministically, and never overwrites an existing image.
15. Folder refresh, upload, reorder, cursor advancement, mode changes, and shutdown are coordinated so filesystem I/O occurs outside async locks where practical while inventory generations and mutation locks prevent lost updates and inconsistent queue state.
16. Every settings or folder mutation uses POST/Redirect/GET on success and returns escaped, actionable feedback on failure. CSRF-invalid, stale-generation, invalid-mode, invalid-path, malformed multipart, and failed filesystem operations leave active settings/inventory and unrelated files unchanged.
17. The settings UI can configure and activate each mode without exposing DigiKam-only controls as if they applied to folder mode. Switching to DigiKam triggers immediate selection; switching to folder mode validates/scans the folder immediately and disables DigiKam rollover work while inactive.
18. Automated tests cover compatibility parsing, mode dispatch/switching, all existing DigiKam behavior, folder ordering/wrap/concurrency/failure behavior, refresh, upload, rename persistence/rollback, traversal/symlink/collision defenses, thumbnails, CSRF/stale updates, readiness, and frame caching semantics.
19. Documentation explains both modes, writable/read-only storage boundaries, folder naming side effects, cursor/reset/cache behavior, upload limits, manual refresh, one-replica requirement, endpoint behavior, setup, and security limitations.
20. The final release binary passes a clean end-to-end exercise of both modes, including actual HTTP settings/folder actions and restart persistence of reordered/uploaded filenames, with the full formatting, lint, test, and release-build suite passing.

## Design rules and important constraints

- Introduce a small mode contract for validation/activation, frame preparation, readiness, status, and settings presentation; keep shared routing, request tracking, response construction, health, and security concerns outside source-specific modules.
- Represent mode-specific settings with strongly typed tagged data. Keep compatibility parsing explicit and tested rather than weakening validation with broadly optional fields.
- Keep DigiKam source capabilities read-only and cache-owned writes unchanged. Model the folder directory separately as application-managed writable storage; never reuse its mutation capability for DigiKam paths.
- Canonicalize roots and resolve every inventory/thumbnail/frame operation beneath the configured folder. Operate on regular files without following symlinks and never trust browser filenames or form-provided paths.
- Define one deterministic filename comparison and one generated ordering-name format. Preserve a safe human-recognizable basename where possible, prevent prefix accumulation, and resolve duplicate/colliding names without overwriting.
- Serialize scans and folder mutations. Use inventory generation tokens for browser actions and temporary same-directory files/names for upload and batch rename; sync successful durable mutations before publishing the new in-memory inventory.
- Treat a refresh or mutation as a transaction from the runtime's perspective: build and validate a complete replacement inventory before swapping it in. Preserve the prior inventory and cursor when an operation fails.
- Reset the folder cursor only for startup, active-mode activation, and successful inventory-changing actions. Merely viewing settings/status must not alter order or cursor.
- Prepare a stable response source before committing cursor advancement, and define/test concurrent GET ordering. Do not infer successful device display or body receipt after the server has handed off a response.
- Keep blocking directory scans, JPEG inspection, renames, syncs, and large file operations off Tokio executor threads. Bound upload request/file sizes and avoid holding global state locks during body transfer.
- Use dedicated mutation endpoints/forms for settings, refresh, reorder, and upload, all protected by the existing per-process CSRF token. Retain automatic HTML escaping and generation-based lost-update protection.
- Render browser thumbnails from the original JPEG through a non-advancing, confined endpoint and CSS sizing unless implementation evidence requires image processing; do not add transformation scope speculatively.
- Retain one-process/one-replica deployment assumptions and trusted-LAN security boundary. Commit `Cargo.lock` when dependencies change.

## Implementation stages

### Stage 1 — Generic configuration and mode boundary

- [ ] Add strongly typed active-mode settings with explicit DigiKam and folder variants, compatibility deserialization for the existing DigiKam settings shape, per-mode validation, and updated example TOML.
- [ ] Define mode-neutral frame metadata/status/readiness interfaces and refactor shared app state so routes and health checks do not depend directly on `SelectionResult` or DigiKam scheduler fields.
- [ ] Move current DigiKam selection/scheduling behavior behind the DigiKam mode implementation without changing its source/cache capability boundary.
- [ ] Add tests for old/new settings loading and persistence, invalid/mixed mode fields, mode-specific validation, and unchanged DigiKam selection behavior.
- [ ] Verify with `cargo fmt --check`, strict Clippy, and the full tests; commit the generic configuration/mode milestone.

### Stage 2 — Folder inventory and safe filesystem operations

Depends on Stage 1.

- [ ] Add a folder-mode module that canonicalizes/validates its managed root, recovers recognized temporary operations, scans only eligible top-level regular JPEGs, assigns safe opaque runtime identifiers, and builds a filename-sorted inventory.
- [ ] Implement synchronized inventory generation and cursor state with deterministic reset, sequential reservation/preparation, wraparound, no-advance-on-failure, and a snapshot API for status/UI.
- [ ] Implement explicit refresh as a complete validated inventory replacement while preserving the previous snapshot on failure.
- [ ] Implement bounded, content-validated multipart upload using confined `create_new` temporary files, collision-safe destination naming, sync/rename publication, and deterministic queue update.
- [ ] Implement complete-order validation and collision-safe two-phase renaming so refreshed alphabetical order matches the submitted order, including rollback/recovery behavior.
- [ ] Unit-test ordering, wrap/reset, empty/invalid folders, manual add/remove refresh, concurrent reservations, unreadable/deleted files, filename edge cases, traversal/symlink rejection, upload failures/collisions, reorder persistence, injected rename failure, and temporary recovery.
- [ ] Verify formatting, strict linting, and all tests; commit the folder storage/queue milestone.

### Stage 3 — Mode-aware runtime, scheduler, frame serving, and health

Depends on Stages 1–2.

- [ ] Add active-mode activation/switching that validates and initializes the replacement runtime before publishing it, triggers immediate DigiKam selection or folder scan as applicable, and prevents inactive DigiKam rollover work.
- [ ] Refactor the scheduler/trigger loop to delegate only mode-relevant background work while preserving single-flight retries, graceful cancellation, and stale DigiKam cache retention.
- [ ] Refactor frame `GET`/`HEAD` through the generic mode contract: preserve DigiKam conditional responses, make folder `GET` advance and return the next body with rotating-resource cache headers, and make folder `HEAD` peek without advancing.
- [ ] Make readiness and status mode-aware while preserving common direct-peer last-GET tracking and error handling.
- [ ] Add integration tests for dynamic mode switches, startup activation, folder GET sequence/wrap, simultaneous GETs, failed GET/no advance, HEAD/no advance, mode-specific conditional headers, readiness transitions, request tracking, and shutdown during mode work.
- [ ] Verify formatting, strict linting, and all tests; commit the mode runtime/serving milestone.

### Stage 4 — Folder settings, thumbnails, upload, refresh, and reorder UI

Depends on Stages 2–3.

- [ ] Update `/settings` to show active mode, shared runtime state, mode-appropriate configuration/status, and folder queue thumbnails in serving order with the next item clearly marked.
- [ ] Add a safe non-advancing thumbnail route resolved through the current inventory rather than user-provided filesystem paths.
- [ ] Add CSRF- and inventory-generation-protected forms/routes for explicit disk refresh, multipart upload, and complete queue reordering, with operation-specific body limits and POST/Redirect/GET success behavior.
- [ ] Ensure UI mode changes validate/activate transactionally and render escaped submitted values plus actionable errors without changing settings or runtime on failure.
- [ ] Test thumbnail ordering/content/non-advancement, successful and failed refresh/upload/reorder HTTP flows, stale inventory and settings generations, CSRF rejection, multipart/body limits, HTML escaping, PRG redirects, and filename changes visible after reload.
- [ ] Perform a browser-oriented accessibility/usability pass so ordering controls work without relying solely on drag-and-drop or JavaScript.
- [ ] Verify formatting, strict linting, and all tests; commit the complete web-interface milestone.

### Stage 5 — Examples, operations documentation, and safety audit

Depends on Stages 1–4.

- [ ] Update README/configuration examples for selecting both modes, compatibility/migration, folder permissions and separation, startup/refresh behavior, filename-based order and rename side effects, cursor resets/wrap, uploads, thumbnail endpoints, readiness, and mode-specific HTTP caching.
- [ ] Update container/Kubernetes-administrator guidance to distinguish read-only DigiKam mounts from the writable managed folder and retain the one-replica, trusted-LAN, non-root, persistent-storage, and graceful-shutdown requirements.
- [ ] Audit all open/create/remove/rename call sites and demonstrate that DigiKam remains immutable, folder writes remain under its canonical root, cache/settings writes remain under their roots, and all browser-controlled identifiers are confined.
- [ ] Verify the documented local setup from empty temporary directories, then run formatting, strict linting, tests, and a release build; commit the documentation/safety milestone.

### Stage 6 — Final end-to-end validation

Depends on all previous stages.

- [ ] From a clean checkout, build a release binary and create a representative read-only DigiKam catalog/photo tree plus separate writable settings, cache, and managed-folder storage.
- [ ] Start with a legacy DigiKam settings file; verify compatibility, startup selection, daily-mode status/readiness, GET/HEAD/conditional behavior, and unchanged database/source hashes and metadata.
- [ ] Switch through the HTTP settings interface to folder mode; verify immediate alphabetical thumbnails/readiness and exact sequential frame bodies through wrap, with HEAD/thumbnail/health/settings requests not advancing the cursor.
- [ ] Upload JPEGs through HTTP, reorder the complete queue, confirm underlying alphabetical filenames encode the new order, add/remove files directly on disk, and verify only explicit refresh reconciles them.
- [ ] Restart and verify uploaded/reordered files remain in the displayed/served order while the runtime cursor resets to the documented first image; switch back to DigiKam and verify immediate selection still works.
- [ ] Exercise concurrent frame GETs and invalid/stale/CSRF-invalid/oversized/non-JPEG/traversal/collision operations; verify deterministic advancement, proper errors, no overwrite/escape, prior-state retention, and temporary-file cleanup.
- [ ] Send SIGTERM during normal serving and folder/DigiKam work; verify graceful exit and intact settings, cache, catalog, source photos, managed images, and ordering.
- [ ] Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, and `cargo build --release`; record exact results in the implementation summary.
- [ ] Commit any final validated fixes as a meaningful milestone and leave the working tree clean.

## Execution contract

- Keep the plan updated as work progresses and check off completed items.
- Commit each completed stage or meaningful milestone.
- Implementation steps may be revised when new evidence warrants it.
- Design rules may be revised for a documented, well-supported reason.
- Goals and acceptance criteria must not be changed without explicit user approval.
