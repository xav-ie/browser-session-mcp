#!/usr/bin/env node
// Draws the logo, a turtle whose shell is a browser tab, and the repo's social preview from it.
// Colors come from the takeover UI (docs/diagrams/palette.json). Writes:
//   docs/logo/logo.svg            the turtle on its circle
//   docs/logo/social-preview.svg  1280x640, for the repo's social preview image
//   frontend/public/favicon.svg   the takeover page's favicon, the same logo
// Run from anywhere: node scripts/logo.mjs
// CI runs it and fails if anything it writes differs from what's committed.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const { dark } = JSON.parse(readFileSync(path.join(root, "docs/diagrams/palette.json"), "utf8"));
const DIR = "docs/logo";
const BG = dark.surface;
const FG = dark.key;
const f = (v) => +v.toFixed(1);
const bez = (p0, p1, p2, t) => [0, 1].map((i) => (1 - t) ** 2 * p0[i] + 2 * t * (1 - t) * p1[i] + t * t * p2[i]);

// --- the turtle, in drawing units ------------------------------------------------------
// The shell is a browser tab, drawn at full size and scaled by S about the bottom of its base.
const S = 0.82;
const O = [230, 336];
const T = ([x, y]) => [O[0] + (x - O[0]) * S, O[1] + (y - O[1]) * S];
const BASE = 336; // the bottom of the shell, which the neck's underside continues
const shellPath =
  "M70 320 Q96 320 113 278 L138 192 Q146 170 168 170 L352 170 Q374 170 382 192 L404 280 Q410 316 396 336 L70 336 A8 8 0 0 1 70 320 Z";
// The shell's right side: a line (382,192) to (404,280), then a curve down to the base.
const side = (t) =>
  t < 0.5 ? T([382 + 44 * t, 192 + 176 * t]) : T(bez([404, 280], [410, 316], [396, 336], (t - 0.5) / 0.5));
// The parameter and point on that side at height y (y grows with t).
const sideAt = (y) => {
  let lo = 0;
  let hi = 1;
  for (let i = 0; i < 50; i++) {
    const m = (lo + hi) / 2;
    if (side(m)[1] < y) lo = m;
    else hi = m;
  }
  return [lo, side(lo)];
};

const R = 52; // the head's radius
const W = 48; // the neck's thickness
// C: where the neck's top edge meets the shell's side.
const [tc, C] = sideAt(BASE - W);
// The neck is a tube: straight out of the shell along the base, then bending up into the head,
// a separate, bigger circle a little below where the tube ends.
const hx = C[0] + W * 0.6 + R * 0.9;
const hy = BASE - W - R * 1.05;
const headY = hy + 8;
const y0 = BASE - W / 2;
const bend = C[0] + W * 0.4;
const neck = `M${f(C[0] - 40)} ${f(y0)} L${f(bend)} ${f(y0)} C${f(bend + R * 0.9)} ${f(y0)} ${f(hx)} ${f(hy + R * 1.2)} ${f(hx)} ${f(hy)}`;
// A rounded join in the inside corner C: it fills from inside the solid shape out to a curve
// from B (on the neck's top) to A (on the shell's side), tangent to both. Its straight sides run
// through the solid shape, so none of them lies on the outline, where antialiasing would show.
const d = Math.min(26, W * 0.7);
let ta = tc;
while (Math.hypot(side(ta)[0] - C[0], side(ta)[1] - C[1]) < d && ta > 0) ta -= 0.0005;
const A = side(ta);
const B = [C[0] + d, BASE - W];
const fillet = `M${f(A[0] - 4)} ${f(A[1])} L${f(C[0] - 12)} ${f(C[1] + 12)} L${f(B[0])} ${f(B[1] + 4)} L${f(B[0])} ${f(B[1])} Q${f(C[0])} ${f(C[1])} ${f(A[0])} ${f(A[1])} Z`;
// Thick legs under the shell.
const legs = [128, 318]
  .map((x) => `<rect x="${f(T([x, 0])[0])}" y="${BASE - 14}" width="${f(60 * S + 6)}" height="${f(50 * S + 10)}" rx="${f(20 * S + 2)}" fill="${FG}"/>`)
  .join("");
// The eye, and a side profile smile that cuts out through the front of the head.
const k = R / 34;
const P = (dx, dy) => `${f(hx + dx * k)} ${f(headY + dy * k)}`;
const face =
  `<circle cx="${f(hx + 8 * k)}" cy="${f(headY - 12 * k)}" r="${f(7 * k)}" fill="${BG}"/>` +
  `<path d="M${P(12, 13.5)} Q${P(26, 22.5)} ${P(40, 2.5)}" fill="none" stroke="${BG}" stroke-width="${f(6 * k)}" stroke-linecap="round"/>`;
const turtle =
  legs +
  `<path d="${shellPath}" fill="${FG}" transform="translate(${f(O[0] * (1 - S))} ${f(O[1] * (1 - S))}) scale(${S})"/>` +
  `<path d="${neck}" fill="none" stroke="${FG}" stroke-width="${W}" stroke-linecap="round"/>` +
  `<path d="${fillet}" fill="${FG}"/>` +
  `<circle cx="${f(hx)}" cy="${f(headY)}" r="${R}" fill="${FG}"/>` +
  face;

// The turtle on its circle, scaled up and centered on its extent (in a 512 square).
const left = T([54, 0])[0];
const right = hx + R;
const top = Math.min(hy - R, T([0, 170])[1]);
const bottom = BASE - 14 + 50 * S + 10;
const z = Math.min(1.08, 410 / (right - left));
const onCircle = (size) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" width="${size}" height="${size}">` +
  `<circle cx="256" cy="256" r="256" fill="${BG}"/>` +
  `<g transform="translate(${f(256 - (z * (left + right)) / 2)} ${f(256 - (z * (top + bottom)) / 2)}) scale(${f(z)})">${turtle}</g></svg>`;

// --- write ------------------------------------------------------------------------------
mkdirSync(path.join(root, DIR), { recursive: true });
const logo = onCircle(512).replace("<svg ", `<svg role="img" aria-label="browser-session-mcp logo: a turtle whose shell is a browser tab" `);
writeFileSync(path.join(root, DIR, "logo.svg"), logo + "\n");
mkdirSync(path.join(root, "frontend/public"), { recursive: true });
writeFileSync(path.join(root, "frontend/public/favicon.svg"), logo + "\n");

// The social preview: the logo and the project's name and pitch, on the takeover UI's background.
const FONT = `-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif`;
const social = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1280 640" width="1280" height="640">
<rect width="1280" height="640" fill="${dark.bg}"/>
<svg x="96" y="160" width="320" height="320" viewBox="0 0 512 512">${onCircle(512).replace(/^<svg[^>]*>|<\/svg>$/g, "")}</svg>
<text x="480" y="300" font-family="${FONT}" font-size="64" font-weight="700" fill="${dark.text}">browser-session-mcp</text>
<text x="480" y="360" font-family="${FONT}" font-size="30" fill="${dark.muted}">Isolated, long-lived Chrome sessions for AI agents,</text>
<text x="480" y="402" font-family="${FONT}" font-size="30" fill="${dark.muted}">with human login handoff and anti-bot stealth.</text>
</svg>
`;
writeFileSync(path.join(root, DIR, "social-preview.svg"), social);
console.log(`Wrote ${DIR}/logo.svg, ${DIR}/social-preview.svg and frontend/public/favicon.svg`);
