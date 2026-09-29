import assert from "node:assert/strict";
import test from "node:test";
import { readdir, readFile } from "node:fs/promises";

// Raw values allowed outside tokens.css and themes/. Lower these as surfaces move to tokens; never raise them.
const CEILING = {
  fontSize: 7,
  radius: 3,
  colour: 2,
  transitionAll: 0,
  easing: 0,
};

const src = new URL("../src/", import.meta.url);

async function stylesheets(dir = src) {
  const out = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const url = new URL(entry.name + (entry.isDirectory() ? "/" : ""), dir);
    if (entry.isDirectory()) out.push(...(await stylesheets(url)));
    else if (entry.name.endsWith(".css")) out.push(url);
  }
  return out;
}

const isDefinition = (url) =>
  url.pathname.endsWith("/styles/tokens.css") || url.pathname.includes("/styles/themes/");

async function measure() {
  const found = { fontSize: 0, radius: 0, colour: 0, transitionAll: 0, easing: 0 };
  for (const url of await stylesheets()) {
    if (isDefinition(url)) continue;
    const css = (await readFile(url, "utf8")).replace(/\/\*[\s\S]*?\*\//g, "");
    found.fontSize += (css.match(/font-size:\s*[\d.]+(px|rem)\b/g) ?? []).length;
    found.radius += (css.replace(/calc\([^;]*\)/g, "calc()").match(/border-radius:[^;]*\d+px/g) ?? []).length;
    found.colour += (css.match(/#[0-9a-f]{3,8}\b|rgba?\(\s*\d/gi) ?? []).length;
    found.transitionAll += (css.match(/transition:\s*all\b/g) ?? []).length;
    found.easing += (css.match(/cubic-bezier\(/g) ?? []).length;
  }
  return found;
}

// Box shadows only interpolate when both lists line up inset for inset; otherwise the change snaps.
function shadowShape(value) {
  const shadows = [];
  let depth = 0;
  let current = "";
  for (const char of value) {
    if (char === "(") depth++;
    if (char === ")") depth--;
    if (char === "," && depth === 0) {
      shadows.push(current);
      current = "";
    } else current += char;
  }
  shadows.push(current);
  return shadows.map((shadow) => /\binset\b/.test(shadow));
}

test("soft presses and focus rings keep one shadow shape so they animate", async () => {
  const css = (await readFile(new URL("styles/themes/looks.css", src), "utf8")).replace(/\/\*[\s\S]*?\*\//g, "");
  const token = (name) => css.match(new RegExp(`--${name}:([^;]+);`))[1];
  assert.deepEqual(shadowShape(token("soft-raised")), shadowShape(token("soft-sunk")));
  const composer = (selector) =>
    css.match(new RegExp(`\\[data-look="soft"\\] ${selector} \\{[^}]*box-shadow:([^;]+);`))[1].replace("var(--pressed)", token("pressed"));
  assert.deepEqual(shadowShape(composer("\\.composer")), shadowShape(composer("\\.composer:focus-within")));
});

test("stylesheets outside the design tokens do not add raw sizes, colours or easings", async () => {
  const found = await measure();
  for (const [key, limit] of Object.entries(CEILING))
    assert.ok(found[key] <= limit, `${key}: ${found[key]} raw values, ceiling ${limit}. Use a token.`);
});
