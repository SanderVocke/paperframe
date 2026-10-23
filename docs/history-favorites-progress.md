# History/favorites execution evidence

## Objective and completion gates

Fully execute `docs/history-favorites-plan.md` on a new branch and push it. Completion requires retained current-slot history, favorites from queue/history/path/upload, front/back insertion, safe storage lifetime, migration, settings/UI/routes, documentation, security/regression tests, and an actual remote push. Passing the current tests alone does not establish completion.

## Work completed

Branch: `feature/history-favorites`.

- Baseline: `cargo test` via Nix toolchain: 91 passed, 0 failed, 1 ignored.
- CI command inspected: `cargo test --locked --all-targets --all-features`.
- Added descriptor v5 with defaulted immutable-picture map, ordered history records, and favorite records. Collections remain empty/inactive until lifetime management is implemented.
- Added composition identity based on source digest (derivative fallback), orientation, text recipe, and dedicated-text state. Excludes instance ID, display name, provenance, and rendering cache/profile metadata.
- Added v4 default/migration/round-trip and identity tests; corrected older migration tests to use the current descriptor version rather than a hardcoded v4.
- Verification: `cargo fmt --check`; CI test command: 93 passed, 0 failed, 1 ignored.

Toolchain: `cargo` is not on the default PATH. Run:

```sh
nix --extra-experimental-features 'nix-command flakes' shell \
  nixpkgs#cargo nixpkgs#rustc nixpkgs#rustfmt --command sh -c \
  'cargo fmt --check && cargo test --locked --all-targets --all-features'
```

## Audit: requirements not yet complete

The main plan's boxes intentionally remain unchecked until their whole requirement has evidence.

| Plan phase | Evidence / remaining work |
| --- | --- |
| 1 baseline/inspection | Baseline and key persistence/initialization paths inspected; exhaustive module/deletion audit still needed. |
| 2 persistent records | Schema/default migration and identity started; library operations, immutable object ownership, atomic reference behavior, and idempotency remain. |
| 3 storage lifetime | NOT implemented. Existing eager deletions and thumbnail overwrites are unsafe for retained records. Do not populate collections yet. |
| 4 history | NOT implemented: settings, centralized transition hook, seeding, trimming, tests. |
| 5 store operations | NOT implemented: favorite/remove/requeue operations and events. |
| 6 imports | NOT implemented: path/root security and favorites multipart upload. |
| 7 HTTP/UI | NOT implemented. |
| 8 verification | Only schema/identity and existing regressions covered; feature/security/concurrency/manual tests remain. |
| 9 docs/completion | User docs/examples, migration guidance, completed checklist, final audit remain. |

## Retention/store increment

- Implemented queue-to-favorite store operation with generation validation and idempotent duplicate adds; implemented favorite removal.
- Saved compositions own independent `saved-<opaque-id>` image/source/thumbnail files. This deliberately trades extra disk space for isolation from existing queue overwrite/delete behavior. Queue-owned files are never referenced by saved records; no queue deletion path needs to keep a saved-owned file alive.
- Saved-file GC follows descriptor reachability, runs under the mutation lock, and only removes the saved namespace. Removing the last favorite retains a snapshot if history still references it.
- Startup validates saved identity, asset namespace, references, and assets, and includes saved assets in orphan-cleanup reachability.
- Added tests for idempotency, queue orientation edits, queue removal, restart retention, final removal, and failed descriptor publication with snapshot cleanup.
- Verification before final namespace validation: CI test command: 95 passed, 0 failed, 1 ignored. Final verification is recorded in the commit/continuation report.
- History, UI, imports, saved previews, and requeue operations remain unimplemented. The objective is not complete.

## Current-slot history increment

- Added central current-transition capture to descriptor commits and both enqueue publication helpers. History/queue are published atomically, and the exact persisted descriptor becomes the live enqueue state.
- First entry of an empty-queue batch and front insertion are recorded; back insertion into a nonempty queue is not. Reorder, remove, drop, and rotate share the commit hook. Same-ID edits/rerenders and reads do not capture history.
- Distinct compositions move to the top; immutable saved recipes remain unchanged when queue entries are edited. Store retention defaults to 100, has a validated limit setter, trims/clears atomically, and garbage-collects only unreferenced saved assets.
- Older descriptor migrations seed their current entry once. Normal v5 startup does not reorder history or refresh timestamps.
- Snapshot copying for favorite creation now runs on a blocking worker. Enqueue failures clean orphan saved assets against the still-live descriptor.
- Added tests for current transitions, edits, no-op reorder, reads, rotation, deduplication, front/back insertion, trimming, disable/re-enable, blocked advancement, restart, and favorite/history shared retention.
- Added a refill-work serial distinct from UI/library event serial so library saves do not trigger scans or hide coalesced queue work; supplier poll deadlines are preserved.
- **Not done:** `[history]` settings and live settings synchronization, migration with configured history disabled, path/upload imports, HTTP/UI, saved previews, requeue, full security/concurrency audit, manual verification, and user documentation.

## Configuration and saved-queue increment

- Added backward-compatible `[history] max_entries` configuration, default 100, valid range 0–100000; legacy settings also default to 100. Added settings form/error redisplay and shipped example sections.
- Startup now receives configured retention before migration seeding; disabled history does not capture the migrated current picture. Restart applies changed retention without promoting current again.
- Live settings use a staged descriptor transaction under the mutation lock: previous assets are retained until settings persistence succeeds; failed settings persistence restores the previous descriptor without losing history. Tests cover simulated settings-file failure through both store and HTTP handler.
- Added history-to-favorite store operation and saved-picture resolver.
- Added saved-to-queue rendering using retained originals, fresh queue-instance IDs, front/back insertion, generation revalidation, and provenance preservation without mutating supplier-seen tracking. Derivative-only records currently return a clear missing-original error; compatible legacy reuse still requires review/implementation.
- Verification: `cargo fmt --check` and `cargo test --locked --all-targets --all-features`: **102 passed, 0 failed, 1 ignored**.
- The objective is still not complete. Settings and descriptor are separate files; cross-file process-crash recovery is not proven by the ordinary failure/rollback tests and remains an audit consideration.

## Browsing and HTTP-action increment

- Added paginated `/history` and `/favorites` pages (24 cards per page), collection navigation, lazy thumbnails, timestamps, favorite state, saved composition/original links, front/back queue actions, and favorite removal. Added Favorite actions to queue cards.
- Added `src/routes/library.rs` and `templates/library.html`. Mutations validate CSRF, generation, typed source and insertion position; saved requeue runs under rendering-transition and render concurrency guards. Pending-submit handling preserves the clicked front/back button value.
- Existing image preparation/thumbnail cache helpers now resolve retained picture keys as well as queue IDs. Saved thumbnails, previews, originals, and rendered assets remain accessible after queue removal, without arbitrary filesystem lookup.
- Store pagination clones only the selected page and validates bounds; added page-size, overflow, and completeness tests.
- Added integration test for pages/bad pagination, CSRF, retained previews, favorite add/remove, and both insertion positions.
- Production-only checking exposed that composition identity uses `serde_json`, previously listed only as a dev dependency. Moved it to production dependencies and verified `cargo check --locked`.
- Latest verification before the final pagination test: 103 passed, 0 failed, 1 ignored. Final results are recorded in the continuation/commit report.
- **Still incomplete:** direct favorites upload/path import, shared in-page preview/card polish, complete stale/unknown-ID/security/concurrency tests, legacy compatible reuse, crash-recovery audit, docs, and manual desktop/mobile/PWA checks. No completion claim is made.

## Direct favorite import increment

- Added direct rendered-favorite publication without any temporary queue membership, current-slot transition or supplier-seen writes. Existing favorite compositions are idempotent; failed publication rolls back staged source/derivative/thumbnail assets.
- Added `/favorites/upload` (one client picture, multipart/body and configured byte limits) and `/favorites/path` (configured source-root selection plus relative path). Both require CSRF, queue generation and settings generation, and use rendering-transition/concurrency guards.
- Added favorites forms and extracted shared plain-photo preparation used by both queue uploads and favorite imports.
- Server-relative reads use Unix `openat` with pinned directory descriptors, `NOFOLLOW` on every relative component, `NONBLOCK` for the final open, regular-file checks and bounded reading. Absolute/parent paths, symlinks, nonfiles and oversized content are rejected. Non-Unix server-path imports fail explicitly; client upload remains available.
- Added production `rustix` dependency and updated lockfile.
- Tests cover nested path success, traversal/root rejection, final/intermediate symlinks, nonfiles, missing/oversized files, upload CSRF/corruption/size limits, unchanged queue/history/seen/source state, source deletion independence, idempotency and failed publication cleanup.
- Updated README and container documentation for behavior, imports, storage growth, backups/migration and downgrade limitations.
- Verification: formatting, production check, CI test command: **107 passed, 0 failed, 1 ignored**.
- **Not complete:** final prompt-to-artifact audit; shared preview/card UI polish and browser checks; remaining stale-ID/generation, race/concurrency/decoder-limit coverage; compatible derivative-only legacy reuse; cross-file settings/descriptor crash recovery review; full checklist evidence.

## Next concrete action

Audit every main-plan checkbox against code/tests and write an explicit evidence table. Close missing legacy reuse, atomicity/crash-recovery and concurrency/security tests; exercise pages and forms in a real desktop/mobile browser if available. Do not mark the goal complete until all requirements have direct evidence and final changes are pushed. In particular inspect `preserve_legacy_source`, `remove`, `advance_internal`, `rerender_all`, orientation/text/edit operations, `publish_rerender`, `replace_derivative`, thumbnail publication, and startup orphan cleanup. Enqueues publish descriptors directly rather than exclusively through `commit_descriptor`, so a current-slot hook only in that method would miss initial/front enqueue transitions.
