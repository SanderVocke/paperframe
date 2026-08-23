# Shared Managed Queue and Supplier Refactor Plan

## Goals

Refactor the server so that image sourcing, queue management, advancement policy, and frame serving are independent components:

- Use one application-owned filesystem queue for every supplier and serve the device only from that queue.
- Reduce DigiKam and manual-folder modes to suppliers that append suitable JPEG copies to the queue in SQL-query or alphabetical order.
- Share queue inspection, upload, reorder, removal, current-image selection, readiness, and frame responses across all suppliers.
- Support advancement after each completed frame-image response, at one or more configured local times, or only when explicitly requested through the web UI.
- Keep the current single-supplier configuration initially, while making supplier coordination capable of supporting multiple suppliers later without replacing the queue or serving architecture.

## Scope

Included:

- A persistent, atomically replaced queue state descriptor plus opaque application-owned JPEG objects under the existing cache/queue storage root; no database.
- A single ordered queue whose first entry is the current image and whose total target length defaults to 10.
- Common queue mutation events that wake a refill coordinator after enqueue, advance, reorder, removal, settings changes, source refreshes, and blocked advancement caused by a one-item queue.
- DigiKam and polling manual-folder suppliers, supplier provenance/seen state, direct web upload, and explicit supplier refresh.
- Common queue thumbnails and controls available regardless of the configured supplier.
- Compatibility migration from the existing DigiKam and sequential-folder settings/storage formats.

Not included:

- Non-JPEG input/output, resizing, transcoding, recursive manual-folder traversal, database-backed application state, or modifying supplier-owned files/records.
- Automatic image effects, multiple device-specific queues, authentication/TLS, distributed coordination, or multiple replicas.
- Enabling multiple active suppliers in the first UI/configuration schema; the internal coordinator boundary must nevertheless accept a collection of suppliers.
- Heuristic device detection from `User-Agent` or other forgeable browser headers.

## Immutable acceptance criteria

1. The application remains one gracefully shutting-down Rust process and retains `/settings`, `GET`/`HEAD /frame/current.jpg`, `/health/live`, and `/health/ready` while replacing supplier-specific serving paths with one common queue-serving path.
2. Mutable settings independently configure common queue behavior, one active supplier, and one advancement policy. The queue target length defaults to 10 and acts as a refill minimum, not a hard cap on direct user uploads.
3. Canonical bootstrap configuration names an application-owned queue root. Existing `cache_dir` bootstrap configuration and both currently supported settings shapes have a documented, tested compatibility migration to the new model.
4. The managed queue root is separate from the settings file and every DigiKam/manual source root. Only the queue module may create, replace, reorder, or delete queue objects; suppliers receive enqueue capabilities/readers and cannot mutate queue internals.
5. Queue JPEGs use opaque, order-independent object names. Queue order, current-first position, entry display name, digest, content length, supplier provenance, imported source identity, and supplier seen/checkpoint state are stored in a small versioned state descriptor using same-directory temporary-file sync and atomic replacement.
6. Startup recovers recognized temporary operations and reconciles the descriptor with queue objects without serving arbitrary/orphaned/corrupt files. Failed enqueue, reorder, remove, descriptor persistence, or recovery never publishes a partial logical queue or overwrites an existing object.
7. The ordered queue includes the current image as its first ordinary entry. The settings page can reorder or remove every entry, including the current one; reordering may immediately change which entry is current, and removing the current entry promotes the next submitted-order entry.
8. Advancement consumes the current entry and promotes the next only when at least two entries exist. If only one entry remains, advancement leaves it current and requests supplier refill; thus natural queue exhaustion continues serving the last valid current image. Explicit removal of the sole entry is allowed and leaves no current image.
9. Every committed queue change increments a generation, persists before publication, and emits a coalescible queue event with its revision/reason. The refill coordinator also receives demand events for startup, polling/explicit refresh, settings changes, and blocked one-entry advancement, and safely rechecks actual queue length before adding anything.
10. Any queue event notifies the configured supplier through the coordinator. The supplier appends enough successful imports to approach the configured target, does nothing when the queue is already at/above target, marks source identities seen only after successful enqueue, and does not synchronously block frame responses or web mutations.
11. Supplier-originated enqueues use the same event path without busy-looping: refill runs are serialized/coalesced, queue length is re-read between batches, retries use bounded delay, and failures retain the queue while appearing in supplier status.
12. DigiKam remains read-only and retains SQL statement/result validation, path mappings, candidate limits, source confinement, JPEG validation, and blocking-work isolation. It supplies unseen candidates strictly in configured SQL query order and no longer decides the current image or owns daily advancement/cache publication.
13. The manual supplier treats its configured top-level source folder as unstable and read-only. It discovers regular non-symlink JPEGs in alphabetical filename order through periodic polling and explicit refresh, copies accepted bytes into the queue, and never renames/removes/writes/serves source files directly.
14. A manual source copy already in the queue remains unchanged if its source is modified, renamed, or deleted. Deleted candidates only disappear from future discovery; modified content is eligible as a new source identity, and unimported new files remain candidates until queue capacity permits import.
15. Supplier provenance and seen identities prevent a user-removed entry from being immediately re-added during normal refill; suppliers continue with the next suitable unseen candidate. Seen/checkpoint state survives restart and can be explicitly reset through a protected supplier action when the user wants to cycle candidates again.
16. The internal supplier registry/coordinator operates over a list of uniquely identified supplier instances and apportions enqueue requests through a supplier-neutral contract, although validation initially permits exactly one configured supplier and the UI exposes one.
17. Advancement policy is a tagged common setting with `every_fetch`, `scheduled`, and `manual` variants. Scheduled policy accepts an IANA timezone and one or more unique local `HH:MM` times, handles DST deterministically, does not replay missed times on startup, and routes scheduled/manual advancement through the same queue operation.
18. In `every_fetch` policy, only a qualifying frame-client GET advances. Qualification uses the direct TCP peer against an optional configured frame-client IP allowlist; forwarding and `User-Agent` headers are ignored. An empty allowlist preserves compatibility by treating all direct peers as frame clients, and the UI uses a dedicated non-advancing preview/thumbnail route.
19. A qualifying `every_fetch` GET advances only after its response body has been fully polled by the HTTP server. A dropped/disconnected/incomplete body, HEAD, conditional/bodyless response, preview, thumbnail, health request, or non-qualifying GET does not advance. Documentation states that server-side body completion cannot prove device receipt, decode, or display.
20. Qualifying concurrent `every_fetch` responses are serialized through completion so each completed frame GET consumes at most its prepared current entry. Completion uses an entry-ID conditional advance, preventing an in-flight response from removing a different current entry after a concurrent user mutation.
21. `every_fetch` responses return a body with `Cache-Control: no-store` rather than `304`. Scheduled/manual policies serve the stable current entry with `Last-Modified` or `ETag` conditional semantics and never advance merely because the image was fetched.
22. The common settings/status UI always shows the current-first ordered queue with thumbnails and provenance, queue/refill/supplier status, and generation-protected CSRF forms to reorder all entries, remove any entry, upload one or more bounded JPEGs directly to the queue, advance explicitly, refresh the supplier, and reset supplier seen state.
23. Direct web uploads are copied into managed queue objects and appended regardless of supplier type. Queue reorder/remove/upload actions never modify DigiKam, the manual source folder, or supplier discovery order.
24. Readiness succeeds while a valid current queue entry can be served, including when no upcoming replacement exists; it returns `503` only when there is no servable queue entry. Supplier/refill failures do not make readiness fail while the last current image remains valid.
25. Automated tests cover migration, descriptor durability/recovery, queue mutation/generation/events, current manipulation, one-entry retention, each advancement policy, completion/disconnect/concurrency behavior, frame-client classification, supplier refill/coalescing/retry, DigiKam SQL order, manual polling/refresh/change handling, seen-state behavior, universal UI mutations, path safety, readiness, restart, and graceful shutdown.
26. Documentation and examples explain the decoupled supplier/advancement model, queue descriptor/storage lifecycle, target/refill behavior, current-entry semantics, direct-peer classification limitations, polling, source immutability, compatibility migration, security boundary, and one-process/one-replica operation.
27. A clean release-binary end-to-end run proves both suppliers feeding the same managed queue, all three advancement policies, universal queue controls, source-change isolation, restart persistence, compatibility migration, body-completion advancement, last-image retention, readiness, and graceful SIGTERM, with formatting, strict linting, tests, and release build passing.

## Design rules and important constraints

- Model configuration as separate `queue`, `advancement`, and `suppliers` sections. Represent suppliers internally as a uniquely identified collection and validate exactly one active supplier for now; do not couple queue or advancement APIs to supplier enums.
- Treat queue target length as a minimum desired inventory including the current entry. User uploads may exceed it. Supplier reconciliation appends only the number needed at the time it commits.
- Use a single linear serving order: entry zero is current. Reorder replaces the whole order; remove deletes the chosen entry; advance removes entry zero only when a successor exists.
- Persist queue and supplier state in a versioned atomic descriptor, but keep transient locks, in-flight response reservations, retry timers, request history, and status in memory. Store no state in DigiKam or manual source folders.
- Publish queue objects with unique `create_new` temporaries, streamed digest/JPEG validation, file sync, atomic rename, then descriptor commit. Remove from the descriptor before best-effort object deletion so crashes can only leave recoverable orphans, never dangling published entries.
- Preserve queue copies as immutable snapshots after enqueue. A supplier source identity/provenance is metadata, not a live link or authority over the queued object.
- Put supplier discovery/materialization behind a source-neutral contract and put target calculation, serialization, event coalescing, retry, seen-state commit, and future multi-supplier iteration in a shared refill coordinator.
- Manual polling must be portable Tokio interval/polling plus explicit refresh, not an OS-specific watcher. Poll/SQLite/digest/file work stays off executor threads and does not hold queue locks.
- Serialize queue descriptor mutations with generation checks, but do not hold queue/global locks while transferring HTTP bodies or scanning/copying sources. Use entry-ID compare-and-advance at response completion.
- Automatic device recognition is not reliable at HTTP level. Use explicit direct-peer IP matching when configured, provide a non-advancing preview path, ignore forwarding/UA heuristics, and describe empty-allowlist compatibility behavior honestly.
- Implement response-completion notification with a body wrapper/stream whose successful EOF hands its per-frame serialization permit and prepared entry ID to the advancement controller; dropping before EOF releases without advancement. Do not claim this proves network delivery or display.
- Scheduled advancement uses the existing tested timezone/DST resolution principles generalized to multiple times. Manual “Next” remains available under every policy but is always CSRF/generation protected.
- Every mutation and demand signal may wake refill; coordinator-side revision coalescing and actual-length checks are mandatory to prevent supplier enqueue event loops.
- Use automatic HTML escaping, opaque queue IDs, bounded forms/uploads, CSRF tokens, settings/queue generations, and POST/Redirect/GET. Keep the documented trusted-LAN/no-auth/no-TLS boundary.
- Commit `Cargo.lock` changes because this is an application. Do not add a database, Kubernetes resources, image transformation, or distributed coordination.

## Implementation stages

### Stage 1 — Decoupled configuration, descriptor schema, and compatibility migration

- [ ] Define canonical common queue settings, tagged advancement settings, and uniquely identified supplier settings, including target default 10, polling interval, schedule validation, frame-client IP allowlist, and one-supplier validation over a collection-shaped model.
- [ ] Define the versioned descriptor schema for ordered queue entries, immutable object metadata/provenance, and supplier seen/checkpoint state; specify startup schema/version errors and atomic persistence rules.
- [ ] Implement compatibility parsing/migration for legacy `cache_dir`, current DigiKam settings, and current sequential-folder settings: map DigiKam to SQL-order supply plus its prior rollover as scheduled advancement, map folder to a read-only manual supplier plus `every_fetch`, and preserve a valid DigiKam `current.jpg` as the first migrated queue entry where applicable.
- [ ] Remove DigiKam `first`/`daily_random` and rollover ownership from canonical supplier settings while retaining compatibility inputs; update example TOML for all supplier/advancement combinations.
- [ ] Test defaults, invalid mixed/unknown fields, duplicate supplier IDs/times, all compatibility mappings, descriptor serialization/versioning, migration failure retention, and canonical restart reload.
- [ ] Verify formatting, strict linting, and all tests; commit the configuration/state-migration milestone.

### Stage 2 — Common durable managed queue

Depends on Stage 1.

- [ ] Introduce a queue module owning the canonical queue root, object/staging layout, descriptor persistence, startup temporary/orphan recovery, and separation validation against every source/settings path.
- [ ] Implement streamed JPEG enqueue from a read-only reader with digest/length validation, opaque order-independent IDs, immutable provenance, atomic object publication, descriptor commit, and cleanup on every failure edge.
- [ ] Implement generation-checked snapshot, full reorder, remove-any-entry, direct upload append, conditional advance, one-entry hold, supplier seen-state reset/update, and readiness/current-image preparation.
- [ ] Emit revisioned queue-change/demand events for all committed mutations and blocked advancement while ensuring persistence precedes live publication/notification.
- [ ] Retire filename-encoded ordering and supplier-specific queue mutation from `src/folder.rs`/`src/cache.rs`; retain only reusable safe reader/publication primitives that respect the new ownership boundary.
- [ ] Test current-first semantics, moving/removing current, sole-entry retention/removal, target-overflow uploads, generation conflicts, event reasons/coalescing inputs, digest/object isolation, crash points, descriptor write/delete failures, corrupt/missing/orphan objects, recovery, and source-root separation.
- [ ] Verify formatting, strict linting, and all tests; commit the durable queue milestone.

### Stage 3 — Supplier contract and shared refill coordinator

Depends on Stages 1–2.

- [ ] Define a supplier-neutral candidate/open-reader/status contract with stable supplier/candidate identities and implement a registry represented as a collection despite one-active-supplier validation.
- [ ] Add a single-flight refill coordinator subscribed to queue events and explicit demand signals; coalesce revisions, calculate target deficit from fresh snapshots, append through queue APIs, persist seen identity only after enqueue, and retry failures with bounded delay.
- [ ] Ensure supplier-generated enqueue events settle without loops and that settings replacement atomically prepares the new supplier/coordinator configuration before activation.
- [ ] Expose supplier-neutral refresh/reset/status operations for the web layer and leave a clear allocation point for future mixed-supplier ordering/fairness without changing queue APIs.
- [ ] Test startup/event/blocked-advance/settings triggers, above-target no-op, exact deficit fill, user-upload overflow, enqueue races, event storms, one-run-at-a-time behavior, partial candidate failures, retry/shutdown, seen-state persistence/reset, and a test registry containing multiple fake suppliers.
- [ ] Verify formatting, strict linting, and all tests; commit the supplier/refill foundation milestone.

### Stage 4 — DigiKam and polling manual-folder suppliers

Depends on Stage 3.

- [ ] Refactor DigiKam catalog/path/source code into a supplier that yields candidates strictly in SQL query order, skips persisted seen identities, opens sources read-only, and has no cache/current/schedule responsibility.
- [ ] Refactor folder mode into a read-only manual supplier with canonical confinement, alphabetical top-level discovery, regular non-symlink JPEG checks, stable content-version identities, configurable periodic polling, and explicit refresh.
- [ ] Ensure manual discovery changes only candidate availability: queued copies survive source modification/rename/deletion, modified content is newly eligible, deleted unseen candidates disappear, and neither queue/UI operations nor supplier refresh mutate the source folder.
- [ ] Integrate both suppliers with common target refill, provenance/status, seen reset, settings validation, and source-specific error reporting.
- [ ] Test DigiKam read-only enforcement and exact SQL-order fill; manual alphabetical fill, polling and explicit refresh, source add/change/delete behavior, immutable queued bytes, symlink/traversal races, candidate disappearance during copy, target-limited import, removal followed by next-unseen supply, and restart seen-state behavior.
- [ ] Verify formatting, strict linting, and all tests; commit the concrete suppliers milestone.

### Stage 5 — Common advancement controller and completion-aware frame serving

Depends on Stages 2–4.

- [ ] Replace the DigiKam scheduler and folder GET cursor with a common advancement controller implementing `every_fetch`, multiple-time `scheduled`, and `manual` policies over queue conditional advance.
- [ ] Generalize timezone/DST scheduling to sorted unique daily times, no startup replay, settings-change recalculation, bounded retry, single-flight operation, and graceful shutdown.
- [ ] Implement direct-peer frame-client classification with optional IP allowlist, ignored forwarding/UA headers, empty-list compatibility, and a dedicated non-advancing preview path.
- [ ] Implement a completion-aware JPEG response body and serialization permit for qualifying `every_fetch` GETs: prepare a stable queue entry, advance by entry ID only after successful EOF, and release without advancement on drop/error.
- [ ] Keep every-fetch responses bodyful and `no-store`; implement stable conditional GET/HEAD semantics for scheduled/manual policy; retain common direct-peer request status without treating preview/HEAD as frame advancement.
- [ ] Make readiness depend only on a servable current queue entry and keep the last sole entry serving through blocked advancement and supplier failures.
- [ ] Test all policy transitions, manual override under every policy, multiple schedule times/DST/reconfiguration, qualifying/unmatched/forwarded/UA clients, `200`/`304`/HEAD/preview, full body versus disconnect, simultaneous qualifying GET serialization, concurrent reorder/remove during transfer, one-entry demand, readiness, and shutdown with in-flight bodies.
- [ ] Verify formatting, strict linting, and all tests; commit the advancement/serving milestone.

### Stage 6 — Universal queue and supplier web interface

Depends on Stages 2–5.

- [ ] Replace mode-specific queue controls with one automatically escaped current-first queue view showing thumbnails, display names, provenance, current marker, queue generation/target, refill state, supplier status, polling/next-schedule status, and last frame GET.
- [ ] Add generation/CSRF-protected common actions to reorder the complete queue, remove any entry including current, upload bounded JPEGs directly to the queue, and explicitly advance; use opaque queue IDs and POST/Redirect/GET.
- [ ] Add supplier-neutral explicit refresh and seen-state reset actions, with source-specific configuration/status sections but no supplier-owned queue manipulation endpoints.
- [ ] Update settings forms to edit queue target/upload bounds, advancement variant/schedule/frame-client matcher, and one supplier while transactionally validating descriptor/source/queue separation before persistence/activation.
- [ ] Provide non-advancing queue thumbnail/preview endpoints and accessible reorder/remove/current controls that do not rely solely on JavaScript or drag-and-drop.
- [ ] Test universal UI under both suppliers and every policy, current reorder/removal, direct upload, supplier refresh/reset, stale settings/queue generations, CSRF/body limits, escaping, opaque-ID confinement, PRG/error feedback, and proof that UI actions do not mutate supplier sources.
- [ ] Verify formatting, strict linting, and all tests; commit the universal UI milestone.

### Stage 7 — Documentation, migration, and safety audit

Depends on Stages 1–6.

- [ ] Rewrite README/examples around supplier versus advancement configuration, shared queue layout/descriptor, target/refill/event behavior, current-first manipulation, one-entry retention, polling/refresh, direct uploads, seen reset, and all three policies.
- [ ] Document compatibility migration and rollback/backup expectations, including how prior DigiKam `current.jpg` and sequential-folder settings/files are treated and that manual source filenames are no longer renamed.
- [ ] Document completion-based advancement and its network-delivery limitation, direct-peer allowlist/empty-list behavior, non-advancing preview use, conditional caching differences, and reverse-proxy implications.
- [ ] Update container/Kubernetes-administrator guidance for one writable managed queue volume, read-only DigiKam/manual source mounts, atomic descriptor semantics, one process/replica, probes, permissions, graceful shutdown, and trusted-LAN security.
- [ ] Audit all filesystem/database mutation call sites and demonstrate queue-only writes, immutable suppliers, confined opaque-ID resolution, no source/cache links, bounded uploads, and recoverable descriptor/object operation ordering.
- [ ] Verify documented clean setup and migration fixtures, formatting, strict linting, tests, and release build; commit the documentation/safety milestone.

### Stage 8 — Final end-to-end validation

Depends on all previous stages.

- [ ] From a clean checkout, build the release binary and create separate queue, settings, read-only DigiKam catalog/photo, and changing read-only manual-source fixtures with recorded hashes/metadata.
- [ ] Start from each prior settings format and verify migration, queue descriptor/object creation, preserved eligible current image, canonical persisted settings, restart recovery, and unchanged supplier files/catalog.
- [ ] With DigiKam supplier, verify SQL-order target-10 fill into managed copies, universal thumbnails/reorder/remove/upload, removal followed by next unseen candidate, seen reset, source failure retention, and no DigiKam mutation.
- [ ] With manual supplier, verify alphabetical target fill, polling and explicit refresh of additions/changes/deletions, immutable existing queue copies, no source writes/renames, direct queue upload, and restart persistence.
- [ ] Verify `every_fetch` with qualifying and non-qualifying direct peers, full throttled body completion, disconnect before EOF, concurrent GETs, and concurrent queue mutation; prove exactly the eligible completed response advances and no other request does.
- [ ] Verify multiple scheduled times/timezone behavior and stable conditional responses between advances, then manual-only stability and explicit advancement, including live policy changes.
- [ ] Reduce the queue to one entry and verify repeated advance/fetch attempts keep serving it and trigger bounded refill; explicitly remove it and verify frame `404`/readiness `503`, then verify recovery when a supplier enqueues again.
- [ ] Exercise stale/CSRF-invalid/oversized/non-JPEG/unknown-ID settings and queue actions plus corrupt/missing object and descriptor/temp recovery; verify prior-state/source retention and actionable status.
- [ ] Send SIGTERM during supplier polling/refill, descriptor mutation boundaries, scheduled waiting, and throttled frame delivery; verify graceful exit and intact recoverable queue/settings/source state.
- [ ] Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, and `cargo build --release`; record exact results and requirement-to-evidence mapping in the implementation summary.
- [ ] Commit final validated fixes as a meaningful milestone and leave the working tree clean.

## Execution contract

- Keep the plan updated as work progresses and check off completed items.
- Commit each completed stage or meaningful milestone.
- Implementation steps may be revised when new evidence warrants it.
- Design rules may be revised for a documented, well-supported reason.
- Goals and acceptance criteria must not be changed without explicit user approval.
