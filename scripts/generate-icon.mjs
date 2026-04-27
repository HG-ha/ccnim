// One-shot icon generator. Re-creates the in-app `.brand-mark` (gradient
// rounded square + white "CC") at 1024×1024 and writes it to
// `src-tauri/icons/source.png`. After running this, hand the file to the
// Tauri CLI to fan out into all platform-specific sizes:
//
//   node scripts/generate-icon.mjs
//   npx tauri icon src-tauri/icons/source.png
//
// This script intentionally does no platform-specific encoding itself —
// `tauri icon` already knows how to emit multi-resolution `.ico`,
// `.icns`, and PNG variants. Keeping this file at ~30 lines of SVG-to-PNG
// rasterization avoids pulling in a heavy icon-encoding stack.

import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const __dirname = dirname(fileURLToPath(import.meta.url));
const OUT_PATH = resolve(__dirname, "..", "src-tauri", "icons", "source.png");
const SIZE = 1024;
// Match the in-app brand-mark proportions: 36px square with 10px radius
// scales to a 1024px square with ~284px radius (10/36 ≈ 0.278).
const RADIUS = Math.round(SIZE * 0.278);

// SVG mirroring `.brand-mark` styling in src/style.css so the desktop
// icon and the in-app sidebar mark stay visually identical.
const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${SIZE}" height="${SIZE}" viewBox="0 0 ${SIZE} ${SIZE}">
  <defs>
    <linearGradient id="brand" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="#2563eb"/>
      <stop offset="100%" stop-color="#06b6d4"/>
    </linearGradient>
    <filter id="inner" x="-10%" y="-10%" width="120%" height="120%">
      <feGaussianBlur in="SourceAlpha" stdDeviation="8"/>
      <feOffset dx="0" dy="6" result="off"/>
      <feComposite in="off" in2="SourceAlpha" operator="arithmetic" k2="-1" k3="1" result="inner"/>
      <feColorMatrix in="inner" values="0 0 0 0 1
                                       0 0 0 0 1
                                       0 0 0 0 1
                                       0 0 0 0.18 0"/>
    </filter>
  </defs>
  <rect x="0" y="0" width="${SIZE}" height="${SIZE}" rx="${RADIUS}" ry="${RADIUS}" fill="url(#brand)"/>
  <rect x="0" y="0" width="${SIZE}" height="${SIZE}" rx="${RADIUS}" ry="${RADIUS}" fill="url(#brand)" filter="url(#inner)"/>
  <text x="50%" y="50%"
        text-anchor="middle"
        dominant-baseline="central"
        font-family="-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Helvetica Neue', Arial, 'PingFang SC', 'Microsoft YaHei', sans-serif"
        font-weight="900"
        font-size="${Math.round(SIZE * 0.5)}"
        letter-spacing="${Math.round(SIZE * 0.02)}"
        fill="#ffffff">CC</text>
</svg>`;

mkdirSync(dirname(OUT_PATH), { recursive: true });

await sharp(Buffer.from(svg))
  // Force the rasterizer to honor the SVG viewBox at native size.
  .resize(SIZE, SIZE, { fit: "contain", background: { r: 0, g: 0, b: 0, alpha: 0 } })
  .png({ compressionLevel: 9 })
  .toFile(OUT_PATH);

console.log(`✓ wrote ${OUT_PATH}`);
console.log("Next: npx tauri icon src-tauri/icons/source.png");
