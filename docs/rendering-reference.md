# Rendering reference and regression policy

The production server is pure Rust. JavaScript and Chromium are development-only oracles used to regenerate committed fixtures; neither is present in the runtime image.

## Pinned upstream behavior

Profile version 2 follows the PaperlessPaper offline web editor's observable default-plus-auto flow:

- Offline firmware/web repository: `https://github.com/paperlesspaper/paperlesspaper-firmware-offline`
- Commit: `0961aa69ffeddfe505f7a2bb4b7c45f49308b2d6`
- Reference implementation: `web/src/GeneratePicture.js` and `web/src/app.js`
- epdoptimize package: `1.2.0`
- npm integrity: `sha512-0wZbhbP8lEmvei1Maacqn0dCcFCuAIlOZSNYDfRKwl6n9NUlVQz9OA42IE9V2PwB/UsSmFy/y9VBiPUIVMSKmQ==`
- epdoptimize source commit/submodule: `555c7a3c42d9233a17b26ec399f3d57340dde8f5`
- Browser fixture oracle: Chromium `147.0.7727.137`

The web package declares ISC; epdoptimize is Apache-2.0 and retains its license in the submodule. Reference code is not compiled into the server.

## GeneratePicture-compatible profile

`src/epd_profile.rs` defines the fixed `generate_picture_auto_v2` profile. It deliberately has typed fields that can later back user-facing controls, but no additional tuning controls are currently exposed or persisted.

The built-in profile reproduces these web defaults:

- `spectra6Custom` calibrated colors in black/white/blue/green/red/yellow order: `#1f2226`, `#d6d6d6`, `#416ce1`, `#067406`, `#ea4843`, `#dbd529`;
- hardware indices `0,6,1,2,3,5` for that order;
- Auto-Optimieren with intent `natural`;
- the web selector's Floyd–Steinberg override after recommendation;
- serpentine scanning;
- zero manual exposure, contrast, and saturation before the per-image auto recommendation.

The Rust stages are intentionally separate:

1. `renderer::prepare_analysis_canvas` and `prepare_framebuffer` preserve aspect ratio, center on white, and apply the web editor's orientation-mismatch transform. Paper L analyzes 600×800 then redraws independently at 1200×1600; Paper 7 analyzes and renders at 800×480.
2. `epd_auto` ports epdoptimize image-style sampling/classification and the GeneratePicture-reachable natural recommendation behavior, including the web app's observable option-merging quirks.
3. `epd_processing` applies reachable paper normalization, exposure/saturation/contrast or S-curve tone mapping, display/auto range fitting with chroma guard and white preservation, luma levels, and RGB/LAB conversion.
4. `epd_pipeline` performs GeneratePicture's forced serpentine Floyd–Steinberg path against calibrated colors and maps palette positions directly to hardware indices.
5. `renderer::encode_bmp4` emits the web-compatible top-down 4-bit BMP with a 118-byte header.

The removed profile-v1 path used ideal native colors, RGB565 input, clean-color snapping, firmware integer error buffers, nearest-neighbor staged resize, stretching, and non-serpentine scanning. None of those operations remains in `ServerDitherBmp`.

## Oracle fixtures and regeneration

Initialize and build the pinned epdoptimize submodule, then regenerate RGBA algorithm fixtures:

```sh
git submodule update --init vendor/epdoptimize
npm ci --prefix vendor/epdoptimize
npm run build --prefix vendor/epdoptimize
node tools/generate_generate_picture_fixtures.mjs
cargo test epd_auto::tests::matches_generate_picture_auto_oracle_cases
```

`tests/fixtures/generate-picture-auto-v2.json` covers ordinary, low-contrast, and high-contrast photos; flat illustration; line art; text/UI; pixel art; warm poster paper; grayscale; and transparency. It records source RGBA, classification/recommendation data, adjusted RGBA, palette positions, packed output, and hashes. Two clean generations must be identical.

The actual-browser fixtures use the upstream web application itself:

```sh
npm ci --prefix tools/oracle
npm run --prefix tools/oracle install-browser
OFFLINE_FIRMWARE_DIR=/path/to/paperlesspaper-firmware-offline \
CHROMIUM_PATH=/path/to/pinned/chromium \
node tools/oracle/generate_browser_fixtures.mjs
```

`OFFLINE_FIRMWARE_DIR` must be checked out at the commit above. The generator loads source PNGs through the real UI, selects each display, clicks Auto-Optimieren, and downloads the resulting BMP. `tests/fixtures/browser/metadata.json` records provenance and hashes.

Canonical target-sized Paper 7 and Paper L sources are byte-identical between Rust and Chromium. Browser Canvas resampling is implementation-specific, so resized/letterboxed/rotated fixtures use explicit perceptual bounds after isolating exact algorithm parity:

- one-eighth box-filtered SSIM must be at least `0.97`;
- each hardware color's coverage ratio may differ by at most `0.02`;
- exact mismatch counts and measured results are recorded in `tests/fixtures/browser/parity-report.json`.

Current resized results are SSIM `0.996190762` for Paper 7 and `0.997071284` for rotated Paper L, with maximum color-coverage deltas `0.002679687` and `0.000109375` respectively.

The older general epdoptimize core fixtures remain useful: `tools/generate_epdoptimize_fixtures.mjs` verifies every scoped diffusion kernel in both scan directions against the same pinned epdoptimize release.

## Migration, format, and limits

`RENDER_PROFILE_VERSION = 2`. Queue entries retain their renderer profile. Startup rerenders profile-v1 server-dithered derivatives from retained immutable sources before serving them; the existing transactional publication and settings rollback boundaries prevent mixed-profile visibility.

Paper 7 output is 800×480 and 192,118 bytes; Paper L is 1200×1600 and 960,118 bytes. The BMP encoder, preview encoder, and independent image decoder tests verify dimensions, top-down rows, palette table, nibble order, and exact preview/served pixels.

Encoded sources remain limited to 100 MiB, 20,000 pixels per axis, and 256 MiB decoded allocation. Rendering is serialized by one application-wide semaphore and runs in blocking workers. HTTP frame requests only read validated stored derivatives. The ignored release smoke benchmark on the development host completed Paper 7 in 61 ms and Paper L in 168 ms, producing 192,118-byte and 960,118-byte BMPs; these measurements are regression evidence, not cross-machine guarantees.
