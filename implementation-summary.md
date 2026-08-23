# Generic Image Modes Implementation Summary

## Objective and deliverables

The completed objective is the implementation of [`plan.md`](plan.md): retain DigiKam SQLite as a safe daily-selection mode, add a generic active-mode boundary, and deliver a sequential writable-folder mode end-to-end through the frame endpoint and settings/status UI.

Concrete deliverables:

- Tagged, backward-compatible mode settings and runtime activation/dispatch.
- Preserved DigiKam query, scheduling, source confinement, cache, and HTTP behavior.
- Filename-sorted folder inventory with sequential/wrapping GET behavior.
- Ordered non-advancing thumbnails and next-image status.
- Durable filesystem-backed reorder, bounded upload, and explicit disk refresh.
- Mode-aware health, request tracking, HTTP caching, security checks, examples, operations documentation, tests, and release validation.

## Prompt-to-artifact checklist

| Requirement / plan acceptance criterion | Concrete evidence |
|---|---|
| Generic app with constant frame/settings surface and modular behaviors (AC 1–2) | `ModeSettings` in `src/config.rs`, `ActiveMode` and activation methods in `src/state.rs`, centralized dispatch in `src/routes.rs`, and mode-aware scheduler in `src/scheduler.rs`. Existing untagged DigiKam parsing and canonical tagged round-trip are tested by `config::tests::parses_legacy_and_round_trips_canonical_modes`. |
| DigiKam remains a complete mode (AC 3) | `src/catalog.rs`, `src/source.rs`, `src/selection.rs`, `src/cache.rs`, and `src/scheduler.rs` retain read-only query/source and atomic cache logic using `DigikamSettings`. Catalog, source, selection, scheduler, cache, conditional HTTP, peer tracking, mode-switch, and release E2E tests passed. |
| Safe top-level JPEG folder inventory (AC 4–5) | `initialize_folder`, `scan_folder`, `recover_temporary_files`, and `confined_entry_path` in `src/folder.rs`; cache/settings overlap checks, regular-file/symlink checks, UTF-8/control-character checks, JPEG marker validation, and explicit `refresh`. Symlink, invalid upload, recovery, refresh, and failure-retention tests passed. |
| Alphabetical sequence, advancement, wrap, reset (AC 6–8) | `Inventory`, `next_image`, `peek_image`, `replace_inventory`, and `operation_lock` in `src/folder.rs`. Tests cover sorted sequence, wrap, concurrent distinct positions, failed-read no-advance, HEAD no-advance, and cursor reset. Release E2E served A/B in order, wrapped/reset through mutations, and reset on restart. |
| Correct rotating-resource HTTP semantics (AC 9) | Folder GET/HEAD uses `rotating_image_response` with `Cache-Control: no-store` and ignores `If-Modified-Since`; DigiKam uses `cached_image_response` with `Last-Modified`/`304`. Both paths are covered by route tests and release E2E. |
| Empty queue readiness/frame behavior (AC 10) | `AppState::is_ready`, `health_ready`, and folder frame error dispatch. `routes::tests::readiness_tracks_empty_folder` verifies `503` plus frame `404`; E2E verified ready transitions. |
| Ordered thumbnails and opaque lookup (AC 11) | `FolderSnapshot::serving_order`, SHA-derived inventory IDs, `thumbnail`, `/folder/thumbnail/{id}`, and `templates/settings.html`. Tests verify thumbnail content/order, HTML escaping, and no cursor advancement. |
| Reorder validation and durable alphabetical names (AC 12–13) | `validate_complete_order`, `rename_in_order`, rollback helpers, ordering-prefix replacement, directory sync, temporary recovery, and generation checks in `src/folder.rs`. Tests cover persistence, incomplete order, staged/publish rollback, and recovery. HTTP and release E2E prove renamed files sort into the submitted order and survive restart. |
| Safe bounded multi-file upload (AC 14) | Axum multipart feature and route bound in `Cargo.toml`/`src/routes.rs`; per-mode byte limit, safe component/sanitization, JPEG validation, `create_new`, sync, rename, append, and no-overwrite behavior in `src/folder.rs`. Module/HTTP tests and real curl multipart E2E passed. |
| Coordination and transactional state publication (AC 15) | Folder `operation_lock`, inventory `RwLock`, generation checks, `spawn_blocking` for scans/read/upload/rename, and complete inventory replacement after successful disk operations. Concurrency, stale generation, failure retention, and mode switch tests passed. |
| Protected mutation UX and failures (AC 16) | Existing per-process CSRF plus settings generation; folder inventory generation; POST/Redirect/GET routes for settings/refresh/reorder/upload; escaped template output and bounded handlers. Tests cover CSRF, stale settings/inventory, oversized settings, invalid JPEG/order, escaping, and unchanged generations. |
| Configure and switch both modes (AC 17) | Mode selector and separate fieldsets in `templates/settings.html`; `SettingsForm` tagged conversion; pre-validation/preparation then persistence/activation in `update_settings`; folder-idle scheduler branch. `settings_http_switches_modes_and_triggers_immediate_digikam_selection` and real HTTP E2E switch both directions. |
| Automated coverage (AC 18) | 41 Rust tests across config/catalog/source/cache/selection/scheduler/folder/routes. Strict Clippy covers all targets/features. Release E2E additionally used a real process, SQLite file, HTTP multipart/forms, restart, and SIGTERM. |
| Documentation and examples (AC 19) | `README.md`, `config.example.toml`, `settings.example.toml`, and `settings.folder.example.toml` document modes, compatibility, storage/permission boundaries, filename changes, cursor/cache semantics, upload limit, manual refresh, one replica, security, probes, and setup. |
| Final release validation (AC 20) | Exact checks below passed. Real-process E2E began from legacy DigiKam settings, checked source/database hashes, switched to folder, verified sequence/conditional behavior, uploaded/reordered/refreshed, rejected bad CSRF, restarted, verified filename/order/cursor persistence semantics, switched back to DigiKam, and shut down via SIGTERM with no temporary files. |

## Staged evidence

1. Generic configuration/mode foundation, folder storage/runtime, HTTP integration, and UI were committed in `60a5d9a` (`Add generic image modes and sequential folder workflow`).
2. Rollback/recovery, route/mode-switch regression tests, accessibility-oriented reorder UI, operations docs, and safety guidance were committed in `2aceb38` (`Harden folder mode and document operations`).
3. Final validation/audit updates are in the final milestone commit recorded by repository history.

The settings and folder mutation code uses only intended roots. The write-call audit (`rg` over `OpenOptions`, create/write/rename/remove/link operations) found production mutations only in `src/config.rs` (settings), `src/cache.rs` (cache), and `src/folder.rs` (managed folder). `src/source.rs` opens DigiKam candidates with read-only `OpenOptions`; `src/catalog.rs` opens SQLite with read-only flags.

## Exact final checks

Run from the repository root on 2026-08-23:

```text
cargo fmt --check
  PASS

cargo clippy --all-targets --all-features -- -D warnings
  PASS

cargo test
  PASS — 41 passed; 0 failed; 0 ignored

cargo build --release
  PASS

git diff --check
  PASS
```

The real release-binary E2E script completed with:

```text
E2E_OK root=/tmp/tmp.esnZWFimGT port=49217
```

It verified exact JPEG bodies, DigiKam `304`, folder `no-store` next-image behavior, HTTP upload/reorder/refresh, on-disk generated ordering names, refresh-only visibility of manual files, restart reset/persistence, bidirectional mode switching, unchanged DigiKam database/photo SHA-256 hashes, SIGTERM exit, and absence of abandoned `*.tmp` files. A separate throttled real HTTP transfer completed byte-for-byte after SIGTERM and reported `INFLIGHT_SIGTERM_OK`, demonstrating graceful handling of an in-flight folder response.

## Boundaries retained

No authentication/TLS, recursive scanning, filesystem watcher, image transformation, multiple-frame cursor, multi-replica coordination, Kubernetes artifacts, or non-JPEG frame output was added. These remain explicitly documented out of scope.
