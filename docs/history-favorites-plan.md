# History and favorites implementation plan

## Outcome and agreed behavior

Implement persistent recent history and favorites in the existing Rust server and control UI.

- History records pictures when they enter the **current slot (queue index 0)**. It does not observe HTTP delivery or claim that the physical frame displayed anything.
- History retains the most recent N distinct compositions, newest first. Default N = 100; N = 0 disables and clears history.
- Entering current again moves an existing composition to the top and updates its timestamp.
- Favorites persist until explicitly removed, independently of queue membership, history retention, supplier availability, and original source files.
- Queue and history cards can be added to favorites.
- History and favorite cards support **Queue at front** and **Queue at back**. Front becomes current immediately; back becomes current only if the queue was empty.
- Favorites can also be created from a server-side relative path or a client-side upload, without adding anything to the queue.
- Saved items preserve source and composition (orientation, text, dedicated-text state). Later edits to a queue entry do not change saved items.
- Favorites are not a supplier and do not participate in automatic refill in this implementation.

## Scope and constraints

- Reuse the existing image validation, renderer, upload limits, CSRF protection, generation checks, and atomic descriptor persistence.
- Keep source folders and DigiKam databases read-only. Never serve arbitrary source paths directly.
- Preserve existing queue, supplier-seen, advancement, and rendering behavior except for retaining objects referenced by history/favorites.
- No new database or service. Extend the versioned managed-store descriptor under `queue_dir`.
- Do not add delivery tracking or change frame-request qualification rules.
- Each phase below should build and pass its relevant tests before continuing. If a storage invariant cannot be preserved, stop and document the failing test and required design decision rather than introducing best-effort persistence.

## 1. Inspect and establish baseline

- [ ] Read `src/queue.rs`, `src/state.rs`, `src/config.rs`, `src/routes.rs`, `src/refill.rs`, `src/supplier.rs`, `src/source.rs`, and `templates/settings.html`.
- [ ] Inspect tests, rendering helpers, descriptor migration/recovery, atomic publication, thumbnail naming, and generation/event consumers.
- [ ] Enumerate every mutation that can change index 0: initial enqueue, batch enqueue, front insertion, reorder, remove, advance/rotate, startup import/recovery, and edit/rerender paths.
- [ ] Enumerate every object/thumbnail deletion path, including rollback and startup orphan cleanup.
- [ ] Run `cargo test` and record any pre-existing failures before making changes.

## 2. Define persistent records and identity

Primary files: `src/queue.rs`, with helper types/modules extracted if useful.

- [ ] Add a new descriptor version with serde-defaulted history/favorites collections and migration from all currently supported versions.
- [ ] Introduce an immutable saved-picture record containing retained source/derivative/thumbnail references, display metadata, provenance, and the full composition recipe. Reuse existing queue metadata instead of creating a parallel renderer schema.
- [ ] Reference saved pictures from history and favorites using opaque stable IDs; keep queue-instance IDs independent.
- [ ] Define a deterministic composition key using source content digest plus normalized composition fields. Include orientation, text recipe, and dedicated-text state; exclude queue ID, timestamps, display filename, and supplier location. Rendering target/profile changes alone should not create duplicate favorites/history.
- [ ] Specify a fallback identity for legacy entries without retained sources: derivative digest plus available composition fields. Do not fabricate missing originals.
- [ ] Add history records with `picture_id` and `last_current_at`; favorite records with `picture_id` and `added_at`.
- [ ] Make adding the same favorite idempotent; preserve its original `added_at`.
- [ ] Retain supplier provenance as metadata only; saving or requeueing does not reset or mutate supplier-seen tracking.
- [ ] Commit queue/history/favorites changes atomically in the same descriptor under the existing mutation lock.
- [ ] Add descriptor round-trip, migration, identity, and idempotency tests.

## 3. Make storage lifetime reference-aware

This must land before enabling retained history or favorites.

- [ ] Ensure saved sources, derivatives, and thumbnails are immutable. Edits/rerenders publish new objects rather than overwriting files referenced by saved compositions.
- [ ] Replace queue-only deletion with reachability-based cleanup across queue, history, favorites, and their saved-picture records.
- [ ] Avoid thumbnail ownership tied exclusively to queue IDs: introduce explicit immutable thumbnail references, with legacy migration as needed.
- [ ] Remove unreferenced saved-picture records when their final queue/history/favorite reference disappears.
- [ ] Keep publication order: write objects first, atomically publish descriptor second, clean unreachable objects last. Descriptor publication failure must leave the old state usable.
- [ ] Apply the same reachability rules to startup orphan cleanup, edit/rerender cleanup, rollback, and removal.
- [ ] Keep mutation/cleanup synchronization sufficient that cleanup cannot delete concurrently staged or newly referenced files. Follow the existing render-outside-lock and generation-recheck pattern where appropriate.
- [ ] Test queue removal, history eviction, favorite removal, shared-object retention, failed commits, and restart recovery.

## 4. Implement current-slot history

Primary files: `src/queue.rs`, `src/config.rs`, initialization in `src/state.rs` as needed.

- [ ] Add `[history] max_entries = 100` to settings, defaulting missing sections for backward compatibility. Validate a documented finite upper bound (suggested 100,000).
- [ ] Add one central transition helper called by all descriptor mutations before commit. Compare previous and next current queue-entry IDs; if a new entry occupies index 0, retain its composition and upsert it at the head of history.
- [ ] Record the first entry when an empty queue receives a batch; do not record every member of that batch.
- [ ] Record newly promoted current entries after removal, reorder, front insertion, and both advancement modes.
- [ ] Do not record a no-op reorder, blocked advance with one entry, failed mutation, or removal that leaves the queue empty.
- [ ] Treat editing/rerendering the same current queue entry as an edit, not a new current-slot transition. Previously saved history remains an immutable snapshot; a later genuine promotion records the edited composition.
- [ ] On first migration, seed history from the existing current entry once if history is enabled. On normal restart, do not refresh timestamps or reorder history.
- [ ] Trim history in the same transaction as insertion or settings-limit changes. Setting N = 0 clears history without touching favorites/queue.
- [ ] Ensure live history-limit changes use the existing settings-update synchronization/rollback contract; do not leave runtime retention and persisted settings inconsistent after an error.
- [ ] Keep timestamps as metadata; use explicit persisted ordering rather than relying on wall-clock uniqueness.
- [ ] Test the full transition matrix, distinct-composition deduplication, trimming, disable/re-enable behavior, migration seeding, and restart stability.

## 5. Add favorites and queue insertion store operations

- [ ] Implement favorite creation from queue ID and history ID, resolving the requested item under the mutation lock with stale-state protection.
- [ ] Implement favorite removal without affecting queue/history references.
- [ ] Implement queue insertion from history/favorite ID with a required validated `front` or `back` position.
- [ ] Allocate a new queue-entry ID for every insertion; allow intentional duplicates.
- [ ] Reuse saved composition and source. Produce a derivative for current frame/output settings when necessary, using existing rendering helpers and generation revalidation before publish.
- [ ] Handle legacy derivative-only records explicitly: reuse only when compatible, otherwise return a clear error rather than pretending an original exists.
- [ ] For front insertion, create the resulting current history entry in the same descriptor transaction.
- [ ] For back insertion into an empty queue, likewise record the new current entry.
- [ ] Return existing generation snapshots/events consistently. Ensure library-only changes invalidate the UI without accidentally triggering supplier refill.
- [ ] Test immutable saved compositions after queue edits, duplicate enqueue, stale IDs/generations, front/back order, and empty-queue behavior.

## 6. Import directly into favorites

Primary files: `src/state.rs`, `src/source.rs`, `src/supplier.rs`, `src/routes.rs`.

### Server-side relative path

- [ ] Add a source-root selection helper: current supplier for folder sources; configured allowed root selection for DigiKam sources. Never accept an arbitrary client-supplied absolute root.
- [ ] Resolve the submitted relative path beneath that root. Allow nested paths even though folder polling scans only top-level files.
- [ ] Reject empty paths, absolute paths, parent traversal, symlink components, non-regular files, and paths outside the selected root.
- [ ] Use safe file opening/validation that does not rely solely on an earlier canonicalization check; account for replacement races according to supported platforms.
- [ ] Enforce encoded/decoded limits and raster validation using the existing import pipeline.
- [ ] Copy and render into managed storage, then atomically add the favorite without creating a queue entry or marking a supplier candidate seen.
- [ ] Return clear errors for unavailable roots, unsupported formats, oversized files, and invalid paths.

### Client-side upload

- [ ] Add a multipart favorite-upload endpoint with the same body, encoded-image, and decoded-image limits as queue uploads.
- [ ] Reuse queue-upload preparation code through a shared import helper with an explicit destination, rather than enqueueing and then moving/removing an item.
- [ ] Support one picture per submission initially; label this clearly in the form.
- [ ] Sanitize the display filename; never use the client filename as a storage path.
- [ ] Publish the favorite only after successful validation/rendering. Failed uploads must not alter queue/history/favorites or leak staged objects.
- [ ] Test both import modes, duplicate favorites, source deletion after import, source read-only behavior, and unchanged queue/supplier-seen state.

## 7. Add HTTP handlers and UI

Primary files: `src/routes.rs`, `templates/settings.html`.

- [ ] Add paginated History and Favorites views, newest first, using existing card/preview styling. Load only the active page's records.
- [ ] Expose thumbnails and non-advancing previews for saved items through opaque IDs. Do not accept filesystem paths in image-serving endpoints.
- [ ] Add **Favorite** actions to queue/history cards and show existing favorite state.
- [ ] Add **Queue at front** and **Queue at back** actions to history/favorite cards; label front insertion as making the picture current immediately.
- [ ] Add **Remove favorite** with appropriate confirmation. No history deletion UI is required initially.
- [ ] Add favorites forms for server-root + relative path and client upload.
- [ ] Add a history-limit control to settings, explaining that history means “recently current,” not confirmed frame deliveries.
- [ ] Use CSRF-protected POSTs for mutations, validated insertion positions, generation conflict handling, redirects, and readable error messages consistent with existing routes.
- [ ] Disable submitting actions while pending; keep intentional repeated queue insertion possible after completion.
- [ ] Extend settings form parsing/error redisplay so history settings survive unrelated settings changes and validation failures.
- [ ] Suggested route families (adapt naming to existing conventions):
  - `GET /history`, `GET /favorites`
  - `POST /favorites/add` (typed queue/history source ID)
  - `POST /favorites/remove`
  - `POST /favorites/path`
  - `POST /favorites/upload`
  - `POST /queue/from-saved` (typed history/favorite source ID + position)
  - `GET /saved/thumbnail/{id}`, `/saved/preview/{id}`, `/saved/original/{id}`
- [ ] Keep PWA behavior unchanged: private pages/photos must not enter service-worker caches.

## 8. Integration and regression verification

- [ ] Add route tests covering CSRF, stale generations, unknown IDs, invalid insertion position, pagination bounds, favorite toggles, and both queue positions.
- [ ] Test favorite multipart upload success, corrupt input, unsupported type, oversized body, and image decode limits.
- [ ] Test path import with nested paths, `..`, absolute paths, symlinks (including intermediate directories), missing files, and root escape attempts.
- [ ] Test history updates for scheduled, manual, and every-fetch advancement via their existing queue operations. Merely requesting an image must not independently write history.
- [ ] Test startup, migration, normal restart, supplier refresh, empty queue refill, rotate, current removal, reorder, front insertion, and current edits.
- [ ] Test simultaneous save/remove/edit/requeue operations and failure injection around descriptor publication where supported.
- [ ] Verify favorites remain usable after source removal, queue removal, history eviction, settings changes, and restart.
- [ ] Verify previews and originals work after the originating queue entry is gone.
- [ ] Run `cargo fmt --check`, `cargo test`, and the repository's existing lint/CI commands; fix new failures.
- [ ] Manually exercise the UI on desktop and narrow/mobile layout with a small history limit, both import forms, both queue positions, and a restart.

## 9. Documentation and completion

- [ ] Update `README.md`: current-slot history semantics, distinct composition retention, N/disable behavior, favorites lifetime, front/back insertion, relative-root rules, and direct upload.
- [ ] Update shipped settings examples with the history section and defaults; update container docs if root mounting instructions need clarification.
- [ ] Explain disk usage: history is count-bounded, favorites are unbounded until removed, and objects are retained while any collection references them.
- [ ] Document migration/rollback implications of the new descriptor version and recommend backing up the managed data directory before upgrade.
- [ ] Completion requires all checklist behaviors implemented, migration/regression tests passing, and no queue-only cleanup path capable of deleting retained favorite/history assets.
