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

test("each text size scales the whole interface, code and terminal included, not only messages", async () => {
  const css = (await readFile(new URL("styles/tokens.css", src), "utf8")).replace(/\/\*[\s\S]*?\*\//g, "");
  const block = (selector) => css.slice(css.indexOf(selector)).match(/\{([^}]*)\}/)[1];
  const sizes = (text) => Object.fromEntries([...text.matchAll(/--([\w-]+):\s*([\d.]+)px/g)].map((m) => [m[1], Number(m[2])]));
  const base = sizes(block(":root {"));
  const small = { ...base, ...sizes(block(':root[data-text-size="small"]')) };
  const large = { ...base, ...sizes(block(':root[data-text-size="large"]')) };
  for (const name of ["text-sm", "text-md", "text-base", "text-lg", "reading-size", "code-size", "terminal-size"]) {
    assert.ok(small[name] < base[name] && base[name] < large[name], `--${name}: ${small[name]} < ${base[name]} < ${large[name]}`);
  }
  assert.ok(large["text-xs"] > base["text-xs"], "small labels grow at Large");
  const markdown = await readFile(new URL("styles/surfaces/markdown.css", src), "utf8");
  assert.doesNotMatch(markdown, /font-size:\s*13px/, "code blocks follow --code-size");
});

test("a style preview starts from the plain theme, so it never shows the active style's depth", async () => {
  const theme = async (name) => (await readFile(new URL(`styles/themes/${name}.css`, src), "utf8")).replace(/\/\*[\s\S]*?\*\//g, "");
  const firstSelector = (css) => css.slice(0, css.indexOf("{")).replace(/\s+/g, " ").trim();
  // The base tokens land on every look sample too; the light reset stays at the looks' own weight so they still apply.
  assert.equal(firstSelector(await theme("ink")), ":root, [data-look]");
  assert.equal(firstSelector(await theme("parchment")), ':root[data-theme="light"], :where([data-theme="light"]) [data-look]');
});

test("Windows glass keeps at least 80% of the theme's colour, so a white window behind cannot wash out the text", async () => {
  const css = (await readFile(new URL("styles/themes/looks.css", src), "utf8")).replace(/\/\*[\s\S]*?\*\//g, "");
  const top = css.replace(/@media[^{]*\{(?:[^{}]*\{[^{}]*\})*[^{}]*\}/g, "");
  const rules = [...top.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map(([, selectors, body]) => ({ selectors: selectors.split(",").map((s) => s.trim()), body }));
  const density = (theme, platform, name) => {
    let best = { weight: -1, value: undefined };
    for (const [order, rule] of rules.entries()) {
      const value = rule.body.match(new RegExp(`--glass-${name}:\\s*(\\d+)%`))?.[1];
      if (!value) continue;
      for (const selector of rule.selectors) {
        if (!selector.startsWith(':root[data-window-glass]') || !selector.includes('[data-look="glass"]')) continue;
        if (selector.includes('[data-theme="light"]') && theme !== "light") continue;
        if (selector.includes(':not([data-platform="mac"])') && platform === "mac") continue;
        const weight = (selector.match(/\[|:not/g) || []).length * 1000 + order;
        if (weight > best.weight) best = { weight, value: Number(value) };
      }
    }
    return best.value;
  };
  for (const theme of ["dark", "light"])
    for (const name of ["frame", "page", "card"]) {
      assert.ok(density(theme, "windows", name) >= 80, `Windows ${theme} ${name} is ${density(theme, "windows", name)}%`);
      assert.ok(density(theme, "mac", name) <= density(theme, "windows", name), `Mac ${theme} ${name} is denser than Windows`);
    }
  assert.equal(density("dark", "mac", "frame"), 40, "macOS keeps its own glass");
});

test("styles change how dense the theme's surfaces are, never their colour", async () => {
  const css = (await readFile(new URL("styles/themes/looks.css", src), "utf8")).replace(/\/\*[\s\S]*?\*\//g, "");
  const surfaces = [...css.matchAll(/--(side|canvas|raised):\s*([^;]+);/g)];
  assert.ok(surfaces.length > 0, "the glass surfaces are no longer found");
  for (const [, name, value] of surfaces)
    assert.match(value, /var\(--base-(side|canvas|raised)\)/, `--${name} in looks.css is a colour of its own: ${value.trim()}`);
});
