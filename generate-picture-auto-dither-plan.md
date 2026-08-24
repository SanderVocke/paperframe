# GeneratePicture Auto-Optimized Rust Rendering Plan

## Goal

Replace the active firmware-compatible production ditherer with a pure-Rust rendering pipeline that reproduces, as closely and deterministically as possible, the output produced by PaperlessPaper's `GeneratePicture.js` when a user:

1. loads an image with the web UI defaults,
2. selects the corresponding Paper 7 or Paper L display,
3. clicks **Auto-Optimieren**, and
4. uploads the generated packed image to the frame.

The reference behavior is the offline firmware web app at commit `0961aa69ffeddfe505f7a2bb4b7c45f49308b2d6` and its locked `epdoptimize` 1.2.0 dependency, corresponding to this repository's pinned epdoptimize commit `555c7a3c42d9233a17b26ec399f3d57340dde8f5`. The upstream file is named `GeneratePicture.js`; references to `GenerateImage.js` in the request mean this implementation.

## Scope

### In scope

- Replace `renderer::firmware_dither` and its firmware-specific preprocessing with the epdoptimize-compatible Rust path.
- Reproduce the default web palette, image placement, auto-analysis, recommendation, image adjustments, quantization/error diffusion, device-index mapping, and Paper 7/Paper L output.
- Port every epdoptimize path reachable from `GeneratePicture.js` defaults plus **Auto-Optimieren** with intent `natural`, including image classification, preset/recommendation resolution, RGB/LAB matching, quantization-only output, tone mapping, dynamic-range compression, level compression, paper normalization, Floyd–Steinberg/Stucki diffusion where reachable, and white preservation.
- Force the web UI's selected default matrix (Floyd–Steinberg) after auto recommendation, exactly as `app.js` does, and always enable serpentine scanning for error diffusion.
- Introduce typed internal rendering configuration that can later accept palette selection and brightness/exposure, contrast, and saturation controls without redesigning the renderer.
- Keep production runtime pure Rust; JavaScript and a browser may be used only to regenerate upstream reference fixtures.
- Replace profile-v1 firmware regression evidence with profile-v2 GeneratePicture/epdoptimize evidence and rerender existing server-dithered queue entries on upgrade.

### Out of scope

- Adding palette, brightness, contrast, saturation, auto-optimize, kernel, or serpentine controls to the web settings page in this change.
- Porting epdoptimize modes that cannot be selected by the specified GeneratePicture default-plus-auto flow, such as ordered, random, blue-noise, Riemersma, or arbitrary user-selected processing presets.
- Running JavaScript, Chromium, Canvas, Node, or epdoptimize in production.
- Changing direct-JPEG mode, queue behavior, supplier behavior, frame advancement, or HTTP contracts beyond the renderer profile migration.

## Immutable acceptance criteria

1. `ServerDitherBmp` no longer calls or retains the current firmware-specific dither implementation, clean-color snapping, RGB565 reduction, integer firmware error buffers, staged firmware prescaling, or stretch-to-fill behavior.
2. The built-in profile reproduces these web defaults exactly in configuration:
   - `spectra6Custom` palette: `#1f2226`, `#d6d6d6`, `#416ce1`, `#067406`, `#ea4843`, `#dbd529` in black/white/blue/green/red/yellow order;
   - device indices `0,6,1,2,3,5` for that order;
   - Auto-Optimieren intent `natural`;
   - default selected Floyd–Steinberg override after recommendation;
   - serpentine enabled for error diffusion;
   - default manual brightness, contrast, and saturation values of zero before auto options are applied.
3. Paper L auto-analysis uses the same aspect-fit, white-backed 600×800 analysis canvas as `GeneratePicture.js`, then independently renders the final 1200×1600 output. Paper 7 uses 800×480 for both analysis and final output. Orientation mismatch handling and centering match the web app.
4. For committed upstream RGBA canvas fixtures that bypass browser decoding/resizing, Rust matches epdoptimize/GeneratePicture exactly for:
   - classification kind and recommendation option object;
   - every reachable adjustment branch and RGB/LAB matching;
   - quantization-only and serpentine error-diffusion palette indices;
   - packed device nibbles and the final top-down 4-bit BMP bytes.
5. The oracle corpus covers ordinary photos, low-contrast photos, high-contrast photos, flat illustrations, line art/text-like images, pixel art, warm paper/poster content, grayscale/desaturated content, transparency, both orientations, both frame targets, and resize/letterbox cases.
6. For end-to-end source-image fixtures where browser decoding or Canvas resampling prevents byte identity:
   - recommendation/classification still matches exactly;
   - after native-color previews are box-filtered to one eighth of each dimension, SSIM against the pinned Chromium GeneratePicture output is at least `0.97` for every fixture;
   - each of the six device-color coverage ratios differs by no more than `2` percentage points;
   - fixture metadata records browser version, upstream commits, options, hashes, exact mismatches, and perceptual metrics rather than claiming byte equality.
7. A fixture whose source is already a canonical, orientation-correct target-sized RGBA image produces a byte-identical BMP to the pinned Chromium oracle for both Paper 7 and Paper L.
8. The rendering profile version is bumped, startup detects old server-dithered derivatives, and the existing atomic rerender/rollback behavior upgrades them without exposing mixed profiles.
9. The internal profile model has typed fields for palette entries/device indices, auto intent, dithering mode/kernel, serpentine, color matching, brightness/exposure, contrast, saturation, tone mapping, dynamic-range compression, level compression, paper normalization, placement, and resize policy. The current UI and persisted user settings expose none of the new tuning fields.
10. All existing non-rendering tests continue to pass. New unit, parity, migration, route, queue, preview, and full-output tests pass under `cargo test --locked --all-targets --all-features`; formatting and clippy pass with warnings denied; release build succeeds.

## Design rules and constraints

- Treat the pinned deployed web behavior as the oracle, including observable quirks in `GeneratePicture.js` option merging; do not silently substitute epdoptimize's intended API behavior when it produces different bytes.
- Keep source-pixel preparation, auto-analysis, option resolution, image adjustment, dithering, and output packing as separate testable stages.
- Use `f64` and explicit JavaScript-compatible rounding, clamping, percentile, sort/tie, and scan-order helpers wherever upstream semantics affect decisions or pixels.
- Preserve palette order because nearest-color ties and device-index mapping are order-dependent.
- Analyze the undithered, aspect-fitted preview canvas, then redraw and process the undithered source at final resolution; never derive final output by scaling the dithered preview.
- Auto recommendations are per image and must not be persisted as global user settings.
- Keep the renderer deterministic, bounded by the existing decode/allocation limits, serialized by the existing render semaphore, and safe for queue-wide rerenders.
- Browser Canvas behavior is browser-dependent. Pin Chromium for the oracle, isolate resampling differences from algorithm differences, and never weaken exact post-resize parity to hide an algorithm mismatch.
- Keep JavaScript fixture generation and upstream sources outside the production binary and Docker runtime context.
- Retain the existing BMP dimensions, top-down layout, hardware nibble values, content type, preview semantics, and frame-serving endpoints.

## Staged implementation plan

### Stage 1 — Pin and capture the executable upstream oracle

- [x] Record the exact offline-firmware commit, epdoptimize npm lock version/tarball integrity, pinned epdoptimize commit, and Chromium version in rendering-reference documentation and fixture metadata.
- [x] Add a development-only fixture generator that runs the actual `GeneratePicture.js` flow in pinned headless Chromium: draw default preview, invoke Auto-Optimieren, apply the app's Floyd override and serpentine state, generate the full packed buffer, and wrap/download the BMP exactly as the web app does.
- [x] Capture separately: undithered analysis RGBA, classification metrics/kind, raw suggested options, post-`GeneratePicture.js` resolved options, final pre-dither adjusted RGBA, palette indices, packed bytes, and BMP.
- [x] Build the compact acceptance corpus listed above with license-safe synthetic and repository-owned image fixtures.
- [x] Add a reproducibility command and verify two clean oracle generations have identical hashes.

**Verification:** fixture provenance is complete; regeneration is deterministic; fixtures demonstrate the current server/web quality and byte differences before replacement.

### Stage 2 — Introduce the extensible internal profile model

- [x] Define typed internal configuration for placement/resizing, palette and device mapping, auto analysis/intent, manual exposure/contrast/saturation, tone/range/level/paper adjustments, color matching, dither mode/kernel, and serpentine scan.
- [x] Add a single immutable `generate_picture_auto_v2` profile constructor populated with the acceptance-criterion defaults; do not add form fields or persisted tuning settings.
- [x] Separate user-selected frame target/output mode from per-image resolved auto options.
- [x] Add validation for unique palette names/device indices, supported hardware indices, finite/ranged numeric options, and compatible algorithm combinations.

**Verification:** profile serialization/debug snapshots are stable, invalid combinations are rejected, and tests prove future slider/palette values can be represented without changing pipeline APIs.

### Stage 3 — Match GeneratePicture canvas preparation

- [x] Replace firmware prescaling, RGB565 reduction, nearest-neighbor stretch, and current rotation convention with white-backed, centered aspect-fit preparation matching `drawOriginalToCanvas`.
- [x] Produce separate analysis and final canvases at the exact Paper 7/Paper L dimensions used upstream.
- [x] Evaluate deterministic Rust resize filters against the pinned Chromium canvas fixtures and select/document the closest implementation; keep the resize policy behind the internal profile abstraction.
- [x] Match alpha compositing, EXIF orientation, explicit frame-orientation override, rotation direction, centering, and integer canvas dimensions.
- [x] Add pixel-level preprocessing tests and report exact and bounded browser-resampling deltas.

**Verification:** target-sized RGBA fixtures are exact; resized/rotated/letterboxed fixtures meet the preprocessing bounds needed by the end-to-end acceptance metrics.

### Stage 4 — Port Auto-Optimieren analysis and option resolution

- [x] Port `image-style.ts` sampling, metrics, tile/edge/color statistics, classification scores, thresholds, tie order, and kind selection using JavaScript-compatible numeric semantics.
- [x] Port the `natural` recommendation path from `auto-processing.ts`, palette profiling, preset scoring, learned/low-contrast/poster tuning, quantization guard, minimum contrast, and white-preservation resolution.
- [x] Reproduce `GeneratePicture.autoOptimise` and `app.js` option merging exactly, including preset expansion behavior at the pinned versions, default Floyd override, and forced serpentine behavior.
- [x] Return a per-image resolved profile without mutating global settings.
- [x] Add table-driven parity tests for every classification and recommendation branch in the oracle corpus plus boundary-focused synthetic metrics.

**Verification:** classification metrics within explicit floating-point tolerances, classification kind, selected strategy, and normalized resolved option snapshots match the oracle for every fixture; branch coverage shows every reachable auto path is exercised.

### Stage 5 — Port reachable image adjustments and color matching

- [x] Port the relevant epdoptimize processing order and exact semantics for off/contrast/S-curve tone mapping, exposure, contrast, saturation, display/auto/off dynamic-range compression, percentile handling, white preservation, luma level compression, and warm-paper normalization.
- [x] Port exact RGB and LAB palette matching, including conversion constants, delta calculation, palette order, and tie behavior.
- [x] Keep adjustment functions independent and driven only by the typed resolved profile.
- [x] Add upstream-generated small RGBA fixtures for each operation alone and representative combined pipelines.

**Verification:** every isolated operation and combined adjusted RGBA fixture is byte-identical where upstream writes clamped bytes; all resolved auto profiles can execute without fallback or unsupported branches.

### Stage 6 — Make the Rust epdoptimize path the only production ditherer

- [x] Route `ServerDitherBmp`, queue enqueue/rerender, orientation rerender, and dithered previews through the new analysis → resolve → adjust → quantize/diffuse → device-map pipeline.
- [x] Use the existing exact Rust error-diffusion core for matching kernels, add quantization-only and LAB dispatch, and map custom-palette positions to hardware nibbles without converting the working palette to ideal native RGB first.
- [x] Force serpentine for error diffusion in the built-in profile and preserve the exact web scan/mirroring/clamp behavior.
- [x] Delete `firmware_dither`, `snap_color`, firmware integer error propagation, firmware-only resize logic/tests, and obsolete profile-v1 fixtures; retain only output-format constants needed by the BMP encoder.
- [x] Bump `RENDER_PROFILE_VERSION` and update derivative validation/profile metadata so all old BMP derivatives are rerendered.

**Verification:** canonical RGBA oracle BMPs are byte-identical; all end-to-end corpus fixtures satisfy the immutable perceptual and color-coverage thresholds; no production call site references the discarded firmware ditherer.

### Stage 7 — Integration, migration, performance, and documentation

- [x] Verify startup migration, settings-wide rerender, per-entry orientation rerender, enqueue/refill/upload, stored derivative serving, previews, stale-generation handling, and rollback under the new per-image auto profile.
- [x] Add tests proving preview pixels and served BMP indices remain identical and that direct-JPEG mode remains unchanged.
- [x] Recheck CPU time and peak memory for Paper L analysis plus final rendering; reuse buffers or bound temporary allocations without changing upstream-visible arithmetic.
- [x] Update README, rendering reference, examples, implementation summary, and Docker/Kubernetes notes to describe auto-optimized epdoptimize rendering and remove firmware-profile claims.
- [x] Ensure Docker remains pure Rust and excludes upstream/browser fixture tooling.

**Verification:** debug and ignored release benchmarks are recorded for both targets; queue migration is atomic; documentation and examples agree with actual defaults and profile version.

### Stage 8 — Final end-to-end validation

- [x] Regenerate oracle fixtures from a clean checkout and verify committed hashes.
- [x] Run `cargo fmt --check`.
- [x] Run `cargo clippy --locked --all-targets --all-features -- -D warnings`.
- [x] Run `cargo test --locked --all-targets --all-features`.
- [x] Run the ignored release Paper 7/Paper L rendering benchmark and `cargo build --locked --release`.
- [x] Run the full source-image corpus through both pinned Chromium GeneratePicture and the Rust server, produce a per-fixture parity report, and verify all immutable exact/perceptual thresholds.
- [x] Start the server with an existing profile-v1 queue backup, verify automatic profile-v2 rerender, inspect served BMP dimensions/nibbles, and confirm rollback behavior under an injected persistence failure.
- [x] Record that no physical frame was available in this environment; use actual-browser byte/perceptual fixtures as the automated gate, with physical inspection explicitly remaining supplementary.

## Dependencies and ordering

- Stage 1 must complete before port parity claims or implementation tuning.
- Stage 2 defines interfaces used by Stages 3–6.
- Stage 3 must stabilize analysis/final canvas inputs before Stage 4 recommendation parity and final visual comparisons.
- Stages 4 and 5 must pass fixture parity before Stage 6 replaces production behavior.
- Stage 6 profile migration must complete before integration and end-to-end validation.
- A stage may be split into smaller commits, but production must not switch to profile v2 until the oracle-backed adjustment and dither paths are complete.

## Execution contract

- Keep the plan updated as work progresses and check off completed items.
- Commit each completed stage or meaningful milestone.
- Implementation steps may be revised when new evidence warrants it.
- Design rules may be revised for a documented, well-supported reason.
- Goals and acceptance criteria must not be changed without explicit user approval.
