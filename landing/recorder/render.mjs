// Renders the landing page film from the states `evb-visual-capture` records:
//
//   EVB_FILM_DIR=$PWD/.devkit/films/capture cargo run -p zed --bin evb-visual-capture --features visual-tests
//   node landing/recorder/render.mjs [--capture <dir>]
//
// Writes public/films/ide/<theme>.mp4 with a poster and a reduced-motion still per theme, and
// app/films/ide.json. The page plays plain video; the 2880x1800 captures are scaled down, so
// interface text stays sharp.
import { bundle } from "@remotion/bundler";
import { renderMedia, renderStill, selectComposition } from "@remotion/renderer";
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LANDING = path.resolve(HERE, "..");
const args = process.argv.slice(2);
const captureIndex = args.indexOf("--capture");
const CAPTURE =
  captureIndex >= 0 ? path.resolve(args[captureIndex + 1]) : path.resolve(LANDING, "../.devkit/films/capture");
// 1440x900 window at 4/3: 1920x1200, the size the film shows at on a 2x display.
const SCALE = 4 / 3;
/** Share of the film shown as a still to visitors who prefer reduced motion. */
const STILL_AT = 0.62;

const manifest = JSON.parse(readFileSync(path.join(CAPTURE, "manifest.json"), "utf8"));
const serveUrl = await bundle({ entryPoint: path.join(HERE, "film.tsx"), publicDir: CAPTURE });
const outDir = path.join(LANDING, "public/films/ide");
mkdirSync(outDir, { recursive: true });
const index = { fps: manifest.fps, intro: 10, width: 0, height: 0, version: "", variants: {} };

for (const theme of ["light", "dark"]) {
  const inputProps = { manifest, theme };
  const composition = await selectComposition({ serveUrl, id: "film", inputProps });
  const still = Math.round(composition.durationInFrames * STILL_AT);
  console.log(`Rendering ${theme}: ${composition.durationInFrames} frames`);
  await renderMedia({
    serveUrl,
    composition,
    inputProps,
    codec: "h264",
    imageFormat: "png",
    crf: 24,
    x264Preset: "veryslow",
    pixelFormat: "yuv420p",
    scale: SCALE,
    outputLocation: path.join(outDir, `${theme}.mp4`),
  });
  for (const [name, frame] of [
    ["poster", index.intro],
    ["still", still],
  ]) {
    await renderStill({
      serveUrl,
      composition,
      inputProps,
      frame,
      imageFormat: "jpeg",
      jpegQuality: 85,
      scale: SCALE,
      output: path.join(outDir, `${theme}-${name}.jpg`),
    });
  }
  index.variants[theme] = { frames: composition.durationInFrames, still };
  index.width = Math.round(composition.width * SCALE);
  index.height = Math.round(composition.height * SCALE);
}

// Part of every film URL, so a re-rendered film replaces cached copies at once.
const hash = createHash("sha256");
for (const theme of Object.keys(index.variants)) {
  for (const file of [`${theme}.mp4`, `${theme}-poster.jpg`, `${theme}-still.jpg`]) {
    hash.update(readFileSync(path.join(outDir, file)));
  }
}
index.version = hash.digest("hex").slice(0, 12);

mkdirSync(path.join(LANDING, "app/films"), { recursive: true });
writeFileSync(path.join(LANDING, "app/films/ide.json"), `${JSON.stringify(index, null, 2)}\n`);
