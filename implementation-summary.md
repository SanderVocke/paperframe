# Shared Managed Queue Refactor — Implementation and Audit Summary

## Objective

Complete [`refactor.md`](refactor.md): make one durable application-owned JPEG queue responsible for current/upcoming order and serving; reduce DigiKam and manual-folder modes to read-only suppliers; independently support completed-fetch, scheduled, and manual advancement; expose universal queue controls; retain migration, safety, and one-process operation.

## Requirement-to-artifact checklist

| Acceptance criteria | Evidence |
|---|---|
| AC 1–3: common process/endpoints, independent settings, target default 10, compatibility | `src/config.rs` defines `QueueSettings`, `AdvancementSettings`, collection-shaped `SupplierSettings`, defaults/validation, old tagged/untagged settings migration, and `cache_dir` alias. `config.example.toml` and both settings examples parse in tests. |
| AC 4–6: queue-only writes, opaque objects, versioned durable descriptor and recovery | `src/queue.rs` exclusively owns `objects/` and `queue-state.toml`, uses random IDs, immutable metadata/provenance/seen state, `create_new`, sync, atomic rename, generation publication, rollback, orphan/temp cleanup, digest verification, and descriptor version rejection. Tests cover migration, restart, stale generations, missing/corrupt state, orphans, atomic batch failure, and injected descriptor-persistence failure. |
| AC 7–8: current is ordinary first entry, reorder/remove, last-image retention | `QueueStore::reorder`, `remove`, `advance*`; route and queue tests move/remove current, promote successor, retain sole entry on advance, and allow explicit sole removal. Real HTTP validation reported `SOLE_RETENTION_RESET_E2E_OK`. |
| AC 9–11: revision events, all-change notification, coalesced target refill/retry | `QueueEvent`, `watch` channel, demand reasons, `src/refill.rs` single-flight loop, fresh target checks, atomic bounded enqueue, event coalescing, polling, and bounded retry. Event/refill/background-shutdown tests prove mutation and blocked-demand events plus next-unseen refill without loops. |
| AC 12: DigiKam SQL-order read-only supplier | `src/catalog.rs`, `src/source.rs`, and `DigikamSupplier` retain SQL shape/read-only checks and canonical source reader confinement. `digikam_supplier_preserves_exact_query_order` verifies literal query order; catalog/source tests cover mutation SQL, aliases/types/limits, no sidecars, traversal, and symlinks. |
| AC 13–15: read-only polling manual supplier, immutable copies, persisted seen state | `ManualSupplier`, `validate_manual_root`, and `scan_manual` discover top-level regular JPEGs alphabetically and compute content-version identities. Queue provenance/seen is committed atomically and resettable. Tests cover alphabetical discovery, changed content, source deletion/modification isolation, removal continuing to next unseen, restart seen persistence, source unavailability with queue readiness, and no source writes. |
| AC 16: future multi-supplier boundary | `Supplier` trait and `SupplierRegistry { suppliers: Vec<Arc<dyn Supplier>> }`; canonical settings are a supplier list but currently validate exactly one. `registry_contract_already_supports_multiple_suppliers` exercises two trait implementations and shared polling selection. |
| AC 17: three common advancement policies and multi-time scheduling | `AdvancementSettings`; `src/scheduler.rs` operates only on `QueueStore::advance`, supports multiple times, timezone/DST, future-only startup behavior, runtime reconfiguration, and shutdown. Unit tests cover ordering and DST; real E2E exercised an actual scheduled transition plus manual and every-fetch policies. |
| AC 18–21: frame qualification, completion, concurrency, caching | `get_image`, direct-peer allowlist, ignored forwarding/UA data, `CompletionStream`, owned serialization permit, chunk pacing/backpressure opportunity, drop-without-completion, and `advance_if_current`. Every-fetch is bodyful/no-store; stable policies retain conditional `Last-Modified`/`304`. Tests cover body collection vs dropped body, serialized GETs, unmatched IP, HEAD, manual conditional GET, sole retention; real throttled disconnect/full-transfer E2E covered transport behavior. |
| AC 22–23: universal web queue UI/actions regardless supplier | `templates/settings.html` and `src/routes.rs` expose common current-first thumbnails/provenance, complete reorder, per-entry remove, direct multipart upload, explicit advance, supplier refresh/history reset, and decoupled settings. IDs are opaque; CSRF/settings/queue generations, bounds, escaping, PRG, and source isolation are implemented. HTTP tests cover thumbnail, CSRF rejection, current removal, and direct upload. |
| AC 24: readiness follows current queue, not supplier | `health_ready` validates current managed object. Tests prove valid queue remains ready with unavailable supplier and sole current; actual HTTP proves empty is `503` and history reset/refill returns `200`. |
| AC 25: automated coverage | 34 tests span config/migration, catalog/source, queue durability/events, suppliers/registry, refill, scheduler, serving completion/classification/caching, universal HTTP controls, source isolation, restart, and shutdown. |
| AC 26: documentation | Rewritten `README.md`, `config.example.toml`, `settings.example.toml`, and `settings.folder.example.toml` cover architecture, queue descriptor, current/target semantics, polling, seen reset, migration, body-completion limitation, direct-peer classification, storage, security, and deployment. |
| AC 27: release E2E | Release process started from legacy `cache_dir` + `folder_sequence`; filled alphabetical queue, preserved copied bytes after source modification, explicitly refreshed, directly uploaded/reordered, rejected incomplete throttled transfer advancement, advanced after full transfer, restarted descriptor/order, exercised stable `304`, manual advance, switched to DigiKam with SQL-order append, verified nonmatching IP no-advance, waited for actual scheduled advance, checked source/catalog hashes, SIGTERM, and temp cleanup. Output: `REFACTOR_E2E_OK root=/tmp/tmp.qT3eWRr4nT port=58457 count=3 first=d.jpg`. Follow-ups reported `SOLE_RETENTION_RESET_E2E_OK` and `INFLIGHT_REFACTOR_SIGTERM_OK`. |

## Commits and staged milestones

- `7182845` — shared queue/configuration, supplier registry/refill, advancement serving, universal UI foundation, and retirement of mode-owned cache/folder selection modules.
- `138225b` — atomic batch operations, event/refill/source behavior hardening, expanded tests, and architecture documentation.
- Final audit/validation milestone is recorded by repository history after this file.

## Safety audit

Production writes are confined to:

- `src/config.rs`: atomic settings replacement.
- `src/queue.rs`: managed queue descriptor and opaque objects.

`src/catalog.rs` opens SQLite read-only/query-only; `src/source.rs` and `src/supplier.rs` only read supplier files. No source link/copy destination, source rename/remove/write, database mutation, filename-order mutation, application database, Kubernetes artifact, image transformation, or multi-replica coordination was added.

## Final gates

Executed from the repository root:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
git diff --check
```

All pass. The final test count/result and clean-tree evidence are rechecked after the final milestone commit.
