# Rust Server-Side E-Paper Rendering Plan

## Goals

- Add a pure-Rust image-processing pipeline that ports the deterministic `epdoptimize` behavior needed by this project and validates the port against a pinned upstream reference.
- Make the production rendering profile hard-coded and versioned so its resizing, six-color palette mapping, and Floyd–Steinberg behavior match the offline firmware's on-device JPEG path as closely as practical.
- Decode supported raster source formats, preserve an immutable queued source, and create the frame representation at enqueue time: a firmware-compatible 4-bit BMP in server-dither mode or a JPEG in direct-JPEG mode.
- Infer portrait or landscape from each decoded image, let the user override that value per queue entry, and rotate the rendered framebuffer so either orientation uses the panel's full pixel area when the user physically repositions the frame.
- Add non-advancing dithered previews and the required global/per-image controls to the existing settings interface without weakening queue, source-confinement, advancement, or persistence guarantees.

## Scope

Included:

- A pinned `epdoptimize` v1.2.0 reference and committed regression fixtures generated from it; the reference implementation is development/test provenance, not a production runtime dependency.
- A Rust port of RGB nearest-palette quantization, non-serpentine and serpentine error diffusion, and all v1.2.0 named error-diffusion kernels: Floyd–Steinberg, Atkinson, false Floyd–Steinberg, Jarvis/Jarvis–Judice–Ninke, Stucki, Burkes, Sierra 3, Sierra 2, Sierra 2-4A, Fan, Shiau–Fan, and Shiau–Fan 2.
- Explicit Spectra 6 calibrated/native palette handling and the fixed ideal six-color palette used by the firmware.
- A separate, versioned firmware-compatible production profile: RGB matching, ideal Spectra 6 colors, clean-color snapping, Floyd–Steinberg weights, left-to-right scanning, no tone/range adjustment, and firmware-like nearest-neighbor resizing.
- Global frame-output settings for target hardware (`paper_7` or `paper_l`) and serving mode (`server_dither_bmp` or `direct_jpeg`). Dither algorithm parameters remain code constants rather than user-editable settings.
- Raster formats enabled by the selected Rust image-decoding crate, with byte-level decoding as the authority; documentation will list the exact compiled codec set. Animated formats use their first frame, and alpha is composited onto white.
- Enqueue-time source validation, EXIF-aware dimensions where supported, orientation inference and persistence, derived BMP/JPEG publication, existing-queue migration, global-mode/target changes, per-entry orientation rerendering, original and dithered previews, and updated documentation/tests.

Not included:

- A JavaScript or Node.js production renderer, runtime calls to `epdoptimize`, or depending on browser-side rendering for supplier imports.
- Full v1.2.0 parity for ordered/random/blue-noise/Hilbert/experimental modes, automatic recommendations, LAB/chroma matching, tone mapping, dynamic-range compression, clarity, edge processing, or image-derived palettes. These may be added later without changing the production profile.
- User-editable dither kernels, palette calibration, tone controls, crop controls, or per-entry arbitrary rotation angles.
- Animation playback, SVG/PDF/vector rendering, recursive manual-folder scanning, multiple frame targets, authentication/TLS, or changing firmware code.

## Immutable acceptance criteria

1. Production builds and normal automated tests remain a single Rust application with no Node.js, browser, JavaScript engine, ImageMagick, or native canvas runtime requirement.
2. The Rust dither module implements RGB quantization plus every named v1.2.0 error-diffusion kernel listed in Scope, including deterministic palette tie-breaking, edge clipping, alpha handling at the renderer boundary, and optional serpentine traversal.
3. Committed fixtures identify the pinned upstream commit, options, input RGBA bytes, palette, and expected palette-index output. Deterministic core cases match `epdoptimize` exactly where arithmetic semantics permit; every documented approximation has a fixed, reviewed tolerance and a regression test that fails when that tolerance is exceeded.
4. A second set of characterization fixtures covers the offline firmware algorithm. The production profile fixes RGB matching, ideal black/blue/green/red/yellow/white colors, firmware clean-color thresholds, Floyd–Steinberg `7/16, 3/16, 5/16, 1/16` diffusion, non-serpentine scanning, integer/clamping behavior, no tone mapping, and firmware-like nearest-neighbor scaling. Intentional deviations are documented and regression-tested.
5. The target setting supports Paper 7 (`800x480` physical framebuffer) and Paper L (`1200x1600` physical framebuffer). Generated BMPs use those exact dimensions and the firmware nibble mapping `0=black, 1=blue, 2=green, 3=red, 5=yellow, 6=white`.
6. Source orientation is inferred after metadata orientation is applied: `height > width` is portrait and all other dimensions are landscape. The inferred value is stored per queue entry and can be overridden to portrait or landscape through a CSRF- and queue-generation-protected action.
7. Rendering first fills the orientation-appropriate logical panel dimensions, then applies one documented 90-degree direction when logical and physical panel orientations differ. Portrait and landscape results consume the full physical framebuffer with no accidental letterboxing; the user-facing documentation explains how to rotate the physical frame to match.
8. Manual-folder discovery, DigiKam supply, and direct upload accept every regular raster format enabled in the chosen Rust decoder, reject undecodable/mislabeled/oversized/unsafe inputs without publishing a queue entry, and retain current path-confinement and non-symlink guarantees.
9. Each queue entry durably owns an immutable source plus metadata and a validated served derivative. In server-dither mode the derivative is a top-down, uncompressed 4-bit BMP with a valid header; in direct-JPEG mode it is a valid JPEG, preserving a suitable JPEG source without recompression where safe and converting other supported formats with fixed encoder settings.
10. Enqueue publishes source, derivative, metadata, seen-history changes, and the descriptor as one recoverable queue mutation. Decode/render/write failure publishes none of them, does not mark the supplier candidate seen, and leaves the prior queue servable.
11. Changing the global serving mode or frame target applies consistently to the whole queue from retained originals before the new setting becomes active. Failure leaves the old setting and all old served representations active; clients never observe a mixed global mode.
12. The frame endpoint serves the stored derivative without rendering on request, with correct `Content-Type`, length, caching, `GET`/`HEAD`, readiness, and completed-fetch advancement behavior. A `.bmp` canonical/alias route may be added, but existing configured `/frame/current.jpg` clients retain a tested compatibility path.
13. Original preview remains non-advancing. A dithered-preview option returns a browser-displayable, six-color preview produced by the same profile, target, and per-entry orientation as frame rendering; requesting either preview never advances or mutates queue order.
14. The settings UI exposes the global target and serving mode, labels each entry's inferred/overridden orientation, and provides accessible portrait/landscape and original/dithered-preview controls. Mutations retain CSRF, stale-generation, escaped-error, and POST/Redirect/GET behavior.
15. Existing version-1 JPEG queues and legacy `current.jpg` migration have a tested upgrade path that preserves queue order, IDs where practical, supplier seen history, and source bytes, and either produces valid derivatives or fails startup without deleting the old valid data.
16. Rendering and bulk rerendering are bounded, occur off Tokio executor threads, do not hold async/global locks during CPU-heavy work, and retain the one-process/one-replica and trusted-LAN operational model.
17. Automated coverage includes dither parity and approximation bounds, firmware-profile characterizations, codec acceptance/rejection, resize/rotation, BMP packing, JPEG conversion, queue transaction failure, migration, settings transitions, orientation overrides, preview/frame routes, HTTP headers, advancement, readiness, restart recovery, and security/staleness cases.
18. Documentation and configuration examples describe supported source formats, first-frame/alpha behavior, target selection, serving modes, orientation convention, migration/storage impact, firmware URL setup, and why dither settings are intentionally hard-coded.
19. The final clean-checkout validation passes formatting, strict linting, all tests, and a release build, and demonstrates both output modes and both image orientations through actual HTTP requests.

## Design rules and important constraints

- Treat the upstream `epdoptimize` tag/commit and offline firmware revision as test oracles. Record provenance in fixture manifests and never silently refresh golden outputs.
- Keep the reusable dither engine independent of HTTP, queue persistence, image decoding, and BMP/JPEG encoding. Represent dither output internally as palette indices so correctness and nibble packing can be tested separately.
- Keep the production profile in one typed constant/versioned module. UI/configuration may select output mode and panel target, but must not reconstruct or partially override algorithm parameters.
- Distinguish upstream parity from firmware compatibility: use upstream fixtures for the generic Rust kernels and firmware fixtures for snapping, fixed-point error propagation, decode downscaling, and final palette-index behavior.
- Use deterministic arithmetic, scan order, palette order, resampling, alpha background, and JPEG encoder settings. If exact JS floating-point parity is required, use explicit `f64`; if ESP signed-shift behavior is required, encode that rounding rule explicitly rather than relying on platform behavior.
- Decode and validate before accepting a candidate. Enforce encoded-byte, decoded-dimension, decoded-pixel, and render-memory limits to prevent decompression bombs and unbounded CPU/memory use.
- Store source and derivative under opaque confined names. Extend the descriptor with explicit media type, source/derivative digests and lengths, source dimensions, orientation value/source, renderer profile version, and target; do not infer durable state from filename extensions.
- Publish files with `create_new`, sync, rename, descriptor replacement, and startup cleanup/recovery consistent with current queue guarantees. Never overwrite or discard the last known-good representation before its replacement is complete.
- Preserve frame serving as static file I/O. Perform enqueue/rerender work in bounded blocking tasks before queue publication, and define deterministic ordering when a settings-wide rerender races with enqueue/remove/reorder.
- Keep source-format support aligned between DigiKam, manual folder, uploads, validation messages, HTML `accept`, examples, and tests. Content decoding is authoritative even when extension filtering is used to avoid opening unrelated files.
- Preserve current queue identity/seen semantics: source identity derives from original bytes and supplier key, not the rendered derivative, so profile or orientation changes do not make a photo newly eligible.
- Use the actual packed BMP as the source of truth for dithered preview, or prove byte-for-byte that a preview generated earlier in the pipeline has the same palette indices.
- Retain existing request completion semantics: HTTP completion can advance the queue but does not claim that the frame decoded or displayed the derivative.
- Commit `Cargo.lock`, migration logic, fixture provenance, and required license/attribution when dependencies or vendored references change.

## Implementation stages

### Stage 1 — Pin references and define parity contracts

- [x] Add the `epdoptimize` v1.2.0 source as a pinned development/reference submodule or equivalent immutable fixture-generation reference, record its commit and Apache-2.0 attribution, and ensure production Cargo/Docker builds do not depend on it being executable.
- [x] Inventory v1.2.0 palette ordering, diffusion maps, scan/edge/tie behavior, and numeric operations for the scoped algorithms; separately transcribe the checked-out firmware palette, snapping, JPEG scale thresholds, integer diffusion, framebuffer dimensions, and BMP expectations.
- [x] Create a deterministic fixture generator that accepts raw RGBA inputs and explicit options, emits palette-index goldens plus a manifest, and includes tiny diagnostic patterns, gradients, hard palette ties, edges, odd dimensions, alpha, and representative photos.
- [x] Add firmware characterization fixtures produced by a small faithful host-side reference or independently checked vectors, including negative error rounding, clean-color thresholds, nearest-neighbor resize mapping, and Paper 7/Paper L packing cases.
- [x] Document which comparisons must be exact and define fixed metrics/thresholds for any unavoidable approximate cases before accepting the Rust port.
- [x] Verify fixture regeneration from the pinned reference, stable fixture hashes across repeated runs, and license/provenance completeness; commit the reference/fixture-contract milestone.

### Stage 2 — Implement and verify the pure-Rust dither core

Depends on Stage 1.

- [x] Add typed palette, color, diffusion-kernel, scan, and indexed-image models with checked dimensions and deterministic palette tie-breaking.
- [x] Port RGB nearest-palette quantization and all scoped v1.2.0 diffusion maps, including forward and serpentine traversal, boundary clipping, error accumulation, and aliases such as Jarvis/Jarvis–Judice–Ninke.
- [x] Add table-driven unit tests for each kernel's weights/divisor, scan direction, edge behavior, palette ties, clamping, and malformed/overflowing inputs.
- [x] Run the committed upstream fixtures through the Rust engine and enforce exact index equality or the predeclared approximation metric; make mismatch diagnostics emit useful per-pixel counts/diffs without rewriting goldens.
- [x] Add property tests/invariants where useful: every output index belongs to the palette, dimensions are preserved, pure palette colors remain stable, deterministic runs are identical, and serpentine reversal mirrors the intended kernel offsets.
- [x] Benchmark representative Paper 7 and Paper L buffers and establish bounded memory/concurrency defaults suitable for enqueue-time processing.
- [x] Verify formatting, strict Clippy, dither unit/regression tests, and release-mode benchmark smoke tests; commit the Rust dither-core milestone.

### Stage 3 — Add the firmware-compatible renderer and encoders

Depends on Stage 2.

- [x] Add pure-Rust raster decoding with an explicit codec feature list, first-frame behavior, white alpha compositing, metadata-orientation handling, and encoded/decoded resource limits.
- [x] Implement orientation inference and the deterministic logical-to-physical transform for Paper 7 and Paper L, including the fixed 90-degree convention and full-frame firmware-like nearest-neighbor resize path.
- [x] Implement the versioned production profile using the firmware ideal palette, clean-color snapping thresholds, non-serpentine Floyd–Steinberg kernel, explicit ESP-compatible integer rounding/clamping, and no hidden image-dependent adjustments.
- [x] Implement a 4-bit indexed BMP encoder/parser/validator with top-down rows, correct palette/header fields, firmware nibble mapping, exact dimensions, overflow checks, and no row-order or padding ambiguity.
- [x] Implement deterministic JPEG output for non-JPEG sources and safe pass-through rules for valid JPEG input, including fixed quality/subsampling/alpha behavior where encoding is required.
- [x] Implement a browser preview conversion from the final indexed/BMP representation to PNG or another reliably displayable lossless format without changing indices.
- [x] Test all supported codec families with small fixtures, corrupt/mislabeled/oversized files, square-image inference, both orientation transforms, both targets, alpha/animation rules, exact BMP bytes, firmware-profile vectors, and JPEG decodeability.
- [x] Verify generated BMPs with an independent parser and the offline firmware/Web-UI format expectations, then run formatting, strict linting, tests, and a release build; commit the renderer/encoder milestone.

### Stage 4 — Redesign queue persistence and enqueue-time rendering

Depends on Stage 3.

- [x] Introduce a versioned queue descriptor that records source and derivative metadata, orientation and whether it is inferred/overridden, renderer profile/target, media type, lengths, and digests while preserving supplier identity and seen history.
- [x] Split opaque queue storage into confined source and served-derivative objects, and update startup reconciliation, prepare/current/remove/reorder/advance, backup expectations, and digest validation for both files.
- [x] Refactor supplier and upload requests to carry general encoded raster bytes; broaden manual-folder discovery and DigiKam validation consistently with the decoder while retaining top-level-only, regular-file, non-symlink, and path-confinement rules.
- [x] Add a rendering service that decodes and renders batches in bounded blocking work before acquiring the short queue publication lock, then atomically publishes all source/derivative files, descriptor changes, and seen marks.
- [x] Implement server-dither enqueue to produce BMP and direct-JPEG enqueue to pass through or convert to JPEG; ensure failures skip/report supplier candidates or reject uploads without partial queue or seen-history changes.
- [x] Implement transactional whole-queue regeneration for global mode/target changes and single-entry regeneration for orientation overrides, with explicit coordination against concurrent enqueue/remove/reorder and last-known-good retention.
- [x] Add version-1 queue and legacy `current.jpg` migration/recovery, including interrupted temporary cleanup and a failure path that preserves old objects for diagnosis/rollback.
- [x] Test mixed source formats, batch truncation, render failures, publication/descriptor failures, concurrent mutations, global transition rollback, per-entry rerender rollback, restart, orphan cleanup, migration, source immutability, and unchanged queue advancement/order semantics.
- [x] Verify formatting, strict linting, all queue/renderer tests, and a release build; commit the queue/rendering integration milestone.

### Stage 5 — Mode-aware serving, previews, and web controls

Depends on Stage 4.

- [x] Add validated global settings for frame target and output mode, compatibility defaults for existing settings files, canonical serialization, form parsing, and transaction ordering that prevents settings from selecting unbuilt derivatives.
- [x] Refactor frame response construction to use derivative media metadata and dynamic `Content-Type` while preserving completed-fetch serialization/advancement, `HEAD`, conditional requests, no-store behavior, readiness, last-request tracking, and body-drop semantics.
- [x] Preserve `/frame/current.jpg` as a tested compatibility endpoint and add/document a format-appropriate canonical endpoint or alias without causing redirects or duplicate advancement.
- [x] Keep original preview/thumbnail routes non-advancing and add a dithered-preview route based on the current production profile, target, and per-entry orientation; define bounded on-demand behavior when direct-JPEG mode has no stored BMP preview.
- [x] Add settings-page controls for target and global output mode, per-card inferred/overridden portrait/landscape controls, and an accessible original/dithered preview switch or links with clear current-mode/status labels.
- [x] Protect orientation and global mutations with CSRF and generation checks, render submitted values/errors safely, and retain POST/Redirect/GET on success.
- [x] Add route/template tests for BMP and JPEG bodies/headers, `GET`/`HEAD`/conditional/completed-fetch behavior, non-advancing previews, readiness during failed transitions, global settings persistence/staleness, HTML escaping, and controls usable without drag-and-drop.
- [x] Perform a browser usability pass at desktop/mobile widths and verify that preview requests and settings-page asset caching never expose frame images through the service worker cache.
- [x] Verify formatting, strict linting, all tests, and a release build; commit the complete web/HTTP milestone.

### Stage 6 — Documentation, operations, and safety/performance audit

Depends on Stages 1–5.

- [x] Update README, settings examples, UI help, and endpoint reference for codec support, image limits, first-frame/alpha rules, hard-coded profile behavior, target/output modes, portrait convention, physical-frame repositioning, BMP firmware configuration, and direct-JPEG conversion.
- [x] Document queue layout/version migration, source versus derivative retention, storage growth, backup/rollback requirements, global rerender behavior, and how renderer/profile changes affect existing entries.
- [x] Update Docker/Kubernetes guidance and manifests for expected CPU, memory, temporary disk, persistent queue growth, one-replica operation, and graceful shutdown during rendering without adding non-Rust runtime packages.
- [x] Audit decoder limits and every source/derivative open/create/read/remove/rename path for confinement, symlink behavior, collision safety, cleanup, and preservation of DigiKam/manual source immutability.
- [x] Measure enqueue and whole-queue regeneration on representative Paper 7/Paper L photos; confirm bounded concurrency keeps frame serving responsive and document supported operational limits.
- [x] Verify all documented clean-install, migration, backup, and rollback procedures from temporary directories; commit the documentation/operations milestone.

### Stage 7 — Final end-to-end validation

Depends on all previous stages.

- [ ] From a clean checkout with submodules initialized only for reference verification, regenerate/check fixture provenance, then build and run the release server without Node.js or other non-Rust rendering tools installed.
- [ ] Exercise manual-folder, DigiKam, and multipart-upload ingestion with JPEG, PNG, BMP, WebP, and at least one additional enabled codec; verify corrupt, mislabeled, animated, alpha, oversized, symlink, nested, and traversal cases follow the documented behavior.
- [ ] In Paper 7 server-dither mode, enqueue portrait, landscape, and square images; inspect original/dithered previews, override orientations, fetch frame `GET`/`HEAD`, parse exact 800x480 4-bit BMPs, and verify order/advancement/readiness/restart behavior.
- [ ] Repeat the orientation/render/parse checks for 1200x1600 Paper L output and verify the documented physical-rotation convention uses the complete framebuffer.
- [ ] Switch transactionally to direct-JPEG mode and back through the HTTP settings interface; verify JPEG pass-through/conversion, BMP regeneration from retained originals, correct media headers, no mixed mode, stale/CSRF rejection, and rollback after an injected render/publication failure.
- [ ] Start from a version-1 queue and legacy settings, verify migration preserves order/seen state/source content, and interrupt a batch enqueue/global rerender to verify restart cleanup and last-known-good serving.
- [ ] Exercise simultaneous frame downloads, previews, enqueue, reorder, remove, orientation change, and global rerender; verify no deadlock, event-loop starvation, accidental preview advancement, lost update, or partial descriptor/filesystem state.
- [ ] Confirm representative Rust outputs remain within every exact/approximate upstream and firmware regression bound and manually review committed visual fixtures for portrait/landscape, gradients, photographs, and hard edges.
- [ ] Send SIGTERM during idle operation, enqueue rendering, global rerender, and body streaming; verify graceful exit and intact settings, queue descriptor, originals, derivatives, supplier sources, and restart recovery.
- [ ] Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, and `cargo build --release`; record exact results and fixture/profile versions in `implementation-summary.md`.
- [ ] Commit any final validated fixes as a meaningful milestone and leave the working tree clean.

## Execution contract

- Keep the plan updated as work progresses and check off completed items.
- Commit each completed stage or meaningful milestone.
- Implementation steps may be revised when new evidence warrants it.
- Design rules may be revised for a documented, well-supported reason.
- Goals and acceptance criteria must not be changed without explicit user approval.
