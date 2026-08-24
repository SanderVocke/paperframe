# Rust Server-Side E-Paper Rendering — Implementation and Audit Summary

## Objective

Complete [`plan.md`](plan.md): port the required deterministic `epdoptimize` behavior to Rust, add a firmware-compatible production renderer, render immutable queue derivatives at enqueue time, support common raster sources, expose global output/target and per-image orientation controls, serve firmware-ready BMPs or JPEGs, provide dithered previews, preserve queue safety, and validate the result end to end.

## Prompt-to-artifact and acceptance checklist

| Requirement / acceptance criteria | Concrete evidence |
|---|---|
| Pure Rust production; no Node/browser/canvas runtime (AC 1) | `src/dither.rs` and `src/renderer.rs`; `Cargo.toml` has only Rust dependencies. Clean worktree release build succeeded with the Node installation removed from `PATH`; the Linux/WSL release build also succeeded where `node` is absent. `vendor/` and fixture tools are excluded from Docker context. |
| Pinned upstream and license/provenance (AC 2–3) | `.gitmodules` pins `vendor/epdoptimize`; gitlink is v1.2.0 commit `555c7a3c42d9233a17b26ec399f3d57340dde8f5`. Upstream Apache-2.0 license remains in the submodule. `docs/rendering-reference.md` records provenance and exact comparison policy. |
| Scoped 1:1 dither port (AC 2) | `src/dither.rs` implements RGB quantization, explicit calibrated/native Spectra 6 palette entries, deterministic tie order, forward/serpentine traversal, clipping, immediate byte rounding, and all 13 scoped v1.2.0 kernels/aliases. Table/invariant tests cover maps, ties, malformed dimensions, palette stability, clamping, and determinism. |
| Upstream regression correctness (AC 3) | `tools/generate_epdoptimize_fixtures.mjs` generates diagnostic and photo-like palette-index fixtures for all scoped kernels/both scans. Rust compares exact indices. Regeneration repeated with stable SHA-256 `d9dbaad17e045634fb3141642b3bca00ae81e57ed856d056261c8b1b400f0dde` for the main fixture. There are no approximate cases. |
| Firmware-compatible hard-coded profile (AC 4) | `RENDER_PROFILE_VERSION = 1`, `DEVICE_PALETTE`, `prepare_framebuffer`, and `firmware_dither` encode ideal colors/nibbles, snap thresholds, RGB565 precision, width pre-scaling, floor-mapped nearest resize, non-serpentine integer Floyd–Steinberg, and arithmetic negative shifts. `firmware-profile-v1.json` covers thresholds, negative shifts, resize mapping, and both target BMP sizes. Intentional deviations are documented. |
| Paper 7/Paper L BMP compatibility (AC 5) | Typed `FrameTarget`; exact `800x480` and `1200x1600` dimensions; top-down 4-bit `encode_bmp4` with 118-byte header and nibble map `0,1,2,3,5,6`. Tests validate bytes, lengths, rows, palette, and independently decode BMPs through `image`. Actual HTTP E2E parsed 192,118-byte Paper 7 and 960,118-byte Paper L BMPs. |
| Orientation inference, override, and full-area transform (AC 6–7) | `ImageOrientation::infer` uses `height > width`, square is landscape; decoder applies exposed metadata orientation first. `prepare_framebuffer` renders orientation-appropriate logical dimensions and fixed clockwise rotation into the physical framebuffer. Queue metadata records inferred/override source. `/queue/orientation` rerenders atomically. Unit, route, HTTP E2E, and committed visual fixtures cover portrait/landscape/targets. |
| Common raster ingestion and safety (AC 8) | `image = 0.25.6` enables JPEG, PNG, GIF, WebP, BMP, TIFF, TGA, PNM, DDS, HDR, OpenEXR, ICO, QOI, and Farbfeld. `decode_source_named` combines byte guessing with extension hints, decoder dimensions/allocation limits, first-frame handling, EXIF orientation, and white alpha compositing. Manual, DigiKam, and upload paths use the same decoder. Tests cover codec round trips, DigiKam PNG, manual PNG/nesting, multipart PNG, animation, alpha, corruption, oversize headers, path confinement, traversal, and symlinks. |
| Immutable source plus validated derivative (AC 9) | Queue descriptor v2 stores opaque source/derivative names, digests, lengths, media type, source dimensions, orientation provenance, target, and profile. `prepare_source` and `prepare` independently verify confinement, regular-file status, digest, decode/media validity. Direct JPEG passes through suitable JPEGs and deterministically encodes other sources at quality 90. |
| Atomic enqueue and seen semantics (AC 10) | `enqueue_rendered_batch` validates all sources/derivatives, publishes `create_new` synced temporaries, renames, then descriptor/seen state. Failures clean staged objects and do not publish in-memory state. Existing atomic-batch, descriptor-failure, source-immutability, next-unseen, and multipart tests remain green. Rendering happens before the short queue publication lock and supplier candidates are marked seen only after commit. |
| Transactional global rerender (AC 11) | `QueueStore::rerender_all` holds the queue mutation boundary, renders retained sources in bounded blocking work, publishes new uniquely named derivatives as a batch, then swaps descriptor and removes old files. Settings activation holds an application transition write lock, rerenders first, persists settings, and rolls back on stale/persistence failure; frame GET/HEAD/readiness take the read side, so clients cannot observe the activation boundary. Route test switches BMP→JPEG and validates every derivative; injected descriptor failure retains old bytes/media. |
| Static frame serving and HTTP semantics (AC 12) | `/frame/current.jpg` and `/frame/current.bmp` share static derivative serving with dynamic media type/length. Stable conditional `GET`/`HEAD`, every-fetch no-store/completion advancement, dropped-body behavior, allowlists, serialization, readiness, and last-request tracking remain covered. No frame request performs rendering. |
| Original and dithered previews (AC 13) | `/queue/preview` and thumbnail transcode retained source without advancing; Queue thumbnails and the in-page comparison dialog always use an undithered, browser-compatible source preview. Dithered entries add a side-by-side lossless PNG from the same indexed production representation and current orientation/target; a regression test independently decodes the stored BMP and preview PNG and requires exact RGB pixel equality. Non-advancing `/queue/original/{id}` and `/queue/rendered/{id}` links expose the retained source and actual stored derivative. One application-wide render semaphore bounds upload/refill/preview/settings work. Route test proves PNG output and unchanged generation. Service worker allowlist excludes all image/settings routes. |
| Web controls and mutation protection (AC 14) | `templates/settings.html` exposes frame model/output mode, inferred/overridden labels, original/dithered links, and no-JavaScript orientation form. Routes retain CSRF, settings generation, queue generation, escaped Askama rendering, and PRG. Desktop/mobile headless Edge screenshots were generated and manually reviewed; responsive CSS and service-worker behavior were inspected. |
| Queue/settings migration (AC 15) | Settings without `[rendering]` default to direct JPEG. Descriptor v1 migration copies legacy derivatives to immutable `.source`, decodes dimensions/orientation, writes v2, and preserves order/seen data. Legacy `current.jpg` is also preserved before configured startup rerender. Migration, missing/corrupt state, restart, source-copy, and whole-directory backup/restore tests pass. |
| Bounded work and operations (AC 16) | `render_serialization` semaphore admits one render; CPU work uses `spawn_blocking`; optimistic descriptor generations allow CPU-heavy rerender/orientation work outside the queue mutation lock; only stale-checked publication serializes with enqueue/remove/reorder. Source limits are 100 MiB encoded, 20,000 px/axis, and 256 MiB decoded allocation. Release benchmark: Paper 7 13 ms/192,118 bytes and Paper L 58 ms/960,118 bytes on the development host. `docs/containers.md` documents memory, CPU, disk, one-replica, and grace requirements. |
| Automated coverage (AC 17) | 68 tests: 67 passing and one intentionally ignored release benchmark. Coverage includes exact upstream/firmware fixtures, codecs, profile, orientation, BMP/JPEG/PNG, queue atomicity/migration/rollback/backup/events, suppliers, settings, previews, frame HTTP semantics, global mode transitions, upload conversion, concurrency, readiness, and shutdown. |
| Documentation (AC 18) | Updated `README.md`, `docs/rendering-reference.md`, `docs/containers.md`, settings examples, Docker seed, Kubernetes ConfigMap/query/resources, endpoint table, queue layout, storage/migration/backup, source formats, image limits, profile, orientation convention, physical repositioning, and firmware URL instructions. |
| Final validation (AC 19) | Clean detached worktree at implementation commit `0c99994` with initialized pinned submodule built/tested under Rust 1.85 without Node in `PATH`. Actual HTTP E2E exercised Paper L BMP, inferred orientations, global direct JPEG, Paper 7 rerender, PNG upload, override, backup/restore/restart. Linux E2E sent SIGTERM during a 20-image global Paper L rerender and during a 960,118-byte body stream; requests completed, all descriptors/files remained valid, no temporaries remained, and restart/readiness succeeded. A 20-image enqueue-render SIGTERM run likewise completed with 20 BMP entries and no temporary files. |

## Safety audit

Production writes remain confined to:

- `src/config.rs`: atomically replace the configured settings file and clean only its recognized temporaries.
- `src/queue.rs`: create/reconcile opaque objects and `queue-state.toml` beneath the canonical queue root; migrate/copy only queue-owned legacy objects; remove only descriptor-owned or recognized temporary/orphan files.

`src/catalog.rs` opens SQLite read-only/query-only. `src/source.rs` opens confined source files read-only. `src/supplier.rs` canonicalizes manual roots/files, rejects symlinks/escapes, and only reads sources. Queue startup also rejects any descriptor object name containing non-normal or multiple path components before joining or migrating it. `src/renderer.rs` performs in-memory decoding/rendering. Browser identifiers are opaque queue IDs; no source path is accepted by preview/orientation routes.

## Milestone commits

- `3aeb630` — actionable rendering implementation plan.
- `a015f03` — pinned reference, exact Rust dither port, renderer, source/derivative queue, modes, previews, controls, docs, and core tests.
- `050b2af` — Rust 1.85 dependency pinning, compatibility hardening, queue recovery/backup/rollback coverage, and completed stage tracking.
- `5f6aed3` — expanded codec/DigiKam/upload/mode regression coverage and manually reviewed visual fixtures.
- `0c99994` — completion-audit hardening: lock-free CPU render phases with stale publication checks, frame-visible transition serialization, confined descriptor names, encoded source limits, OpenEXR coverage, calibrated/native palettes, and exact BMP/preview parity.

## Final gates

Run from the repository root:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
cargo +1.85.0 check --locked
cargo +1.85.0 test --locked
git diff --check
```

The ignored benchmark is run separately with:

```text
cargo test --release renderer::tests::benchmark_full_frame_profiles -- --ignored --nocapture
```
