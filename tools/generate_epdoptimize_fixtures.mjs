import { execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "..");
const upstream = resolve(root, "vendor/epdoptimize");
const modulePath = resolve(upstream, "dist/index.mjs");
const { ditherImage } = await import(pathToFileURL(modulePath));

class MemoryCanvas {
  constructor(width, height, data) {
    this.width = width;
    this.height = height;
    this.data = new Uint8ClampedArray(data);
  }
  getContext(kind) {
    if (kind !== "2d") return null;
    return {
      getImageData: () => ({
        width: this.width,
        height: this.height,
        data: new Uint8ClampedArray(this.data),
      }),
      putImageData: (image) => {
        this.width = image.width;
        this.height = image.height;
        this.data = new Uint8ClampedArray(image.data);
      },
    };
  }
}

const width = 5;
const height = 4;
const rgb = [
  [15, 91, 127], [184, 220, 201], [53, 142, 75], [116, 38, 166], [244, 103, 62],
  [149, 211, 85], [174, 29, 230], [126, 126, 126], [250, 250, 250], [2, 2, 2],
  [255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0], [90, 160, 210],
  [31, 34, 38], [185, 199, 201], [35, 63, 142], [53, 86, 58], [98, 32, 30],
];
const rgba = rgb.flatMap(([r, g, b], index) => [r, g, b, index === 0 ? 17 : 255]);
const palette = [
  { name: "black", color: "#000000", deviceColor: "#000000" },
  { name: "white", color: "#FFFFFF", deviceColor: "#FFFFFF" },
  { name: "blue", color: "#0000FF", deviceColor: "#0000FF" },
  { name: "green", color: "#00FF00", deviceColor: "#00FF00" },
  { name: "red", color: "#FF0000", deviceColor: "#FF0000" },
  { name: "yellow", color: "#FFFF00", deviceColor: "#FFFF00" },
];
const paletteRgb = palette.map(({ color }) => [
  Number.parseInt(color.slice(1, 3), 16),
  Number.parseInt(color.slice(3, 5), 16),
  Number.parseInt(color.slice(5, 7), 16),
]);
const kernels = [
  "floydSteinberg", "falseFloydSteinberg", "atkinson", "jarvis",
  "jarvisJudiceNinke", "stucki", "burkes", "sierra3", "sierra2",
  "sierra2-4a", "fan", "shiauFan", "shiauFan2",
];

const cases = [];
for (const kernel of kernels) {
  for (const serpentine of [false, true]) {
    const source = new MemoryCanvas(width, height, rgba);
    const destination = new MemoryCanvas(width, height, rgba);
    await ditherImage(source, destination, {
      palette,
      ditheringType: "errorDiffusion",
      errorDiffusionMatrix: kernel,
      serpentine,
      colorMatching: "rgb",
      processingEngine: "js",
    });
    const indices = [];
    for (let offset = 0; offset < destination.data.length; offset += 4) {
      const r = destination.data[offset];
      const g = destination.data[offset + 1];
      const b = destination.data[offset + 2];
      const index = paletteRgb.findIndex(([pr, pg, pb]) => r === pr && g === pg && b === pb);
      if (index < 0) throw new Error(`non-palette output for ${kernel}`);
      indices.push(index);
    }
    cases.push({ kernel, serpentine, indices });
  }
}

const fixture = {
  schema: 1,
  upstream: {
    repository: "https://github.com/paperlesspaper/epdoptimize.git",
    version: "1.2.0",
    commit: execFileSync("git", ["-C", upstream, "rev-parse", "HEAD"], { encoding: "utf8" }).trim(),
  },
  comparison: "exact palette indices; epdoptimize clampByte/Math.round behavior is reproduced in Rust",
  width,
  height,
  rgba,
  palette: paletteRgb,
  cases,
};

const destination = resolve(root, "tests/fixtures/epdoptimize-v1.2.0-error-diffusion.json");
await mkdir(resolve(root, "tests/fixtures"), { recursive: true });
await writeFile(destination, `${JSON.stringify(fixture, null, 2)}\n`);
console.log(destination);

// A larger deterministic scene-like sample exercises gradients, sky/ground,
// skin-like midtones, foliage, shadows, and highlights without binary image
// decoder differences entering the parity oracle.
const photoWidth = 16;
const photoHeight = 12;
const photoRgba = [];
for (let y = 0; y < photoHeight; y += 1) {
  for (let x = 0; x < photoWidth; x += 1) {
    const sky = y < 5;
    const r = sky ? 60 + x * 5 : 35 + ((x * 17 + y * 11) % 150);
    const g = sky ? 110 + y * 8 : 65 + ((x * 7 + y * 19) % 145);
    const b = sky ? 175 + x * 3 : 25 + ((x * 13 + y * 5) % 95);
    photoRgba.push(Math.min(255, r), Math.min(255, g), Math.min(255, b), 255);
  }
}
const photoCases = [];
for (const kernel of ["floydSteinberg", "atkinson", "jarvisJudiceNinke", "stucki"]) {
  for (const serpentine of [false, true]) {
    const source = new MemoryCanvas(photoWidth, photoHeight, photoRgba);
    const output = new MemoryCanvas(photoWidth, photoHeight, photoRgba);
    await ditherImage(source, output, {
      palette,
      ditheringType: "errorDiffusion",
      errorDiffusionMatrix: kernel,
      serpentine,
      colorMatching: "rgb",
      processingEngine: "js",
    });
    const indices = [];
    for (let offset = 0; offset < output.data.length; offset += 4) {
      const r = output.data[offset], g = output.data[offset + 1], b = output.data[offset + 2];
      const index = paletteRgb.findIndex(([pr, pg, pb]) => r === pr && g === pg && b === pb);
      if (index < 0) throw new Error(`non-palette photo output for ${kernel}`);
      indices.push(index);
    }
    photoCases.push({ kernel, serpentine, indices });
  }
}
const photoFixture = {
  schema: 1,
  upstream: fixture.upstream,
  comparison: fixture.comparison,
  width: photoWidth,
  height: photoHeight,
  rgba: photoRgba,
  palette: paletteRgb,
  cases: photoCases,
};
const photoDestination = resolve(root, "tests/fixtures/epdoptimize-v1.2.0-photo-like.json");
await writeFile(photoDestination, `${JSON.stringify(photoFixture, null, 2)}\n`);
console.log(photoDestination);
