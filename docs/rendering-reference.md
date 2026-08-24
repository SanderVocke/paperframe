# Rendering reference and regression policy

The production server is pure Rust. JavaScript is used only to regenerate development fixtures from the pinned `vendor/epdoptimize` submodule.

## Pinned upstream

- Repository: `https://github.com/paperlesspaper/epdoptimize.git`
- Release: `v1.2.0`
- Commit: `555c7a3c42d9233a17b26ec399f3d57340dde8f5`
- License: Apache-2.0; the upstream `LICENSE` remains in the submodule.

After initializing submodules, regenerate the deterministic reference fixture with:

```sh
npm ci --prefix vendor/epdoptimize
npm run build --prefix vendor/epdoptimize
node tools/generate_epdoptimize_fixtures.mjs
cargo test dither::tests::matches_pinned_epdoptimize_error_diffusion_fixtures_exactly
```

`tests/fixtures/epdoptimize-v1.2.0-error-diffusion.json` fixes diagnostic input RGBA bytes, the ideal Spectra 6 palette, every scoped error-diffusion kernel, both scan modes, upstream commit, and expected palette indices. `tests/fixtures/epdoptimize-v1.2.0-photo-like.json` adds a larger deterministic scene-like sample with gradients, sky, foliage, skin-like midtones, shadows, and highlights for representative kernels and both scan modes. Comparison is exact. The Rust implementation reproduces upstream `clampByte` (`Math.round` after clamping), immediate byte clamping, RGB distance tie order, mirrored offsets for serpentine rows, and edge clipping. There is currently no approximate upstream case; adding one requires a documented fixed metric and threshold before its golden is accepted.

## Scoped Rust port

The Rust core includes RGB quantization and these v1.2.0 maps: Floyd–Steinberg, false Floyd–Steinberg, Atkinson, Jarvis, Jarvis–Judice–Ninke, Stucki, Burkes, Sierra 3, Sierra 2, Sierra 2-4A, Fan, Shiau–Fan, and Shiau–Fan 2. Ordered, random, LAB/chroma, whole-image, automatic recommendation, tone/range, and edge-processing paths are intentionally outside the accepted subset.

## Firmware-compatible production profile

`src/renderer.rs` defines profile version 1 from the checked-out offline firmware revision `0961aa6`:

- ideal native colors and nibble values `0,1,2,3,5,6`;
- clean-color thresholds from `snapColor`;
- squared RGB nearest-color matching and firmware palette tie order;
- left-to-right Floyd–Steinberg integer propagation using `7/16, 3/16, 5/16, 1/16` and explicit arithmetic right-shift behavior;
- JPEGDEC-like RGB565 channel precision;
- firmware width thresholds for 1/2, 1/4, and 1/8 prescaling followed by nearest-neighbor full-frame resize;
- no tone mapping, dynamic-range compression, or serpentine scan.

Intentional deviations are deterministic and tested:

- Rust decoders apply exposed metadata orientation and support non-JPEG raster sources.
- Alpha is composited onto white.
- Error buffers use `i32` rather than the firmware's `int16_t`, avoiding overflow on pathological rows.
- Paper L output follows the Web UI/streamer canonical `1200x1600` packed BMP rather than reproducing the firmware's inconsistent on-device `1600x1200` conversion layout.
- Animated inputs use the first frame.

The 4-bit BMP encoder is tested independently for top-down height, 118-byte header, row stride, palette table, nibble order, dimensions, and total length. Dithered previews are decoded from the same indexed representation used to construct the BMP. The committed `tests/fixtures/visual/paper7-{landscape,portrait}.png` files provide manually reviewed full-frame visual regressions; automated tests enforce their dimensions and six-color-only pixels.

## Performance and limits

Rendering is serialized through one application-wide semaphore, runs in blocking workers, and is complete before frame serving; HTTP frame requests only read stored derivatives. On the development Windows host, the ignored release benchmark (`cargo test --release renderer::tests::benchmark_full_frame_profiles -- --ignored --nocapture`) rendered a synthetic 1600×1200 source to Paper 7 in 85 ms and Paper L in 159 ms, producing 192,118-byte and 960,118-byte BMPs. These measurements are smoke baselines rather than cross-machine latency guarantees.

Encoded uploads retain the configured request limit. Decoders reject dimensions above 20,000 pixels per axis and allocations above 256 MiB. A single render is admitted at a time, so peak concurrent framebuffer/error-buffer use is bounded even when upload, preview, refill, and settings requests overlap.
