import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";

const themes = ["ink", "parchment"];

async function tokens(name) {
  const css = await readFile(new URL(`../src/styles/themes/${name}.css`, import.meta.url), "utf8");
  return Object.fromEntries([...css.matchAll(/--([\w-]+):\s*(#[0-9a-f]{6})\b/gi)].map((m) => [m[1], m[2]]));
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
      assert.ok(contrast(t.faint, t[surface]) >= 3, `${name} faint on ${surface}: ${contrast(t.faint, t[surface]).toFixed(2)}`);
      assert.ok(contrast(t.gold, t[surface]) >= 3, `${name} gold focus ring on ${surface}: ${contrast(t.gold, t[surface]).toFixed(2)}`);
    }
    assert.ok(contrast(t["gold-ink"], t.gold) >= 4.5, `${name} text on the gold button`);
  });
}
