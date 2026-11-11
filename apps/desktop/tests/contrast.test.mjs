import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";

const themes = ["ink", "parchment"];

async function tokens(name) {
  const css = await readFile(new URL(`../src/styles/themes/${name}.css`, import.meta.url), "utf8");
  // A token may name another (`--side: var(--base-side)`); follow it to the colour, as the browser does.
  const raw = Object.fromEntries([...css.matchAll(/--([\w-]+):\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()]));
  const resolve = (value, depth = 0) => {
    const ref = value?.match(/^var\(--([\w-]+)\)$/);
    return ref && depth < 8 ? resolve(raw[ref[1]], depth + 1) : value;
  };
  return Object.fromEntries(
    Object.keys(raw).map((name) => [name, resolve(raw[name])]).filter(([, value]) => /^#[0-9a-f]{6}$/i.test(value ?? "")),
  );
}

function luminance(hex) {
  const [r, g, b] = [1, 3, 5].map((i) => {
    const c = parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a, b) {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

for (const name of themes) {
  test(`${name}: text, secondary text and focus ring stay readable on every surface`, async () => {
    const t = await tokens(name);
    for (const surface of ["canvas", "side", "raised", "overlay"]) {
      assert.ok(contrast(t.text, t[surface]) >= 7, `${name} text on ${surface}`);
      assert.ok(contrast(t.dim, t[surface]) >= 4.5, `${name} dim on ${surface}: ${contrast(t.dim, t[surface]).toFixed(2)}`);
      // Muted text is small meta and code comments sit at code size, so both need the small-text bar.
      assert.ok(contrast(t.faint, t[surface]) >= 4.5, `${name} faint on ${surface}: ${contrast(t.faint, t[surface]).toFixed(2)}`);
      assert.ok(contrast(t["syntax-comment"], t[surface]) >= 4.5, `${name} code comment on ${surface}: ${contrast(t["syntax-comment"], t[surface]).toFixed(2)}`);
      assert.ok(contrast(t.gold, t[surface]) >= 3, `${name} gold focus ring on ${surface}: ${contrast(t.gold, t[surface]).toFixed(2)}`);
    }
    assert.ok(contrast(t["gold-ink"], t.gold) >= 4.5, `${name} text on the gold button`);
    assert.ok(contrast(t["gold-ink"], t["gold-hover"]) >= 4.5, `${name} text on the hovered gold button`);
  });
}
