// The native bridge admits only the desktop requests it lists, on purpose. A page
// calling one it doesn't list works in every mocked test and fails only in the
// real app, so check the two lists against each other.
import assert from "node:assert/strict";
import test from "node:test";
import { readdir, readFile } from "node:fs/promises";

const src = new URL("../src/", import.meta.url);

/** The methods one bridge command lets through, read from its allowlist. */
export function allowlist(bridge, command) {
  const body = bridge.slice(bridge.indexOf(`async fn ${command}(`));
  const list = body.slice(0, body.indexOf(".contains(&method"));
  return new Set([...list.matchAll(/"([a-z_.]+)"/g)].map((match) => match[1]));
}

/** Extension, settings and instruction requests the pages name, minus the ones sent to a live session. */
export function requested(code) {
  const live = new Set([...code.matchAll(/liveCall\([^,]+,\s*"([a-z_.]+)"/g)].map((match) => match[1]));
  return [...new Set([...code.matchAll(/"((?:extensions|settings|instructions)\.[a-z_.]+)"/g)].map((match) => match[1]))]
    .filter((method) => !live.has(method))
    .sort();
}

test("every request the pages make is one the native bridge lets through", async () => {
  const bridge = await readFile(new URL("../src-tauri/src/main.rs", import.meta.url), "utf8");
  const files = (await readdir(src)).filter((name) => /\.tsx?$/.test(name));
  const code = (await Promise.all(files.map((name) => readFile(new URL(name, src), "utf8")))).join("\n");
  const extensions = allowlist(bridge, "extension_request");
  const settings = allowlist(bridge, "settings_request");
  assert.ok(extensions.size > 10 && settings.size > 10, "both allowlists were found");

  const blocked = requested(code).filter((method) => !(method.startsWith("extensions.") ? extensions : settings).has(method));

  assert.deepEqual(blocked, [], "add these to the bridge allowlist in src-tauri/src/main.rs");
});

// The other direction: a page redrawn without one of its controls still passes
// every test of what remains. Each thing the bridge can do for the Extensions
// pages must still be asked for by some page.
test("no extension action the bridge offers has lost its control", async () => {
  const bridge = await readFile(new URL("../src-tauri/src/main.rs", import.meta.url), "utf8");
  const files = (await readdir(src)).filter((name) => /\.tsx?$/.test(name));
  const code = (await Promise.all(files.map((name) => readFile(new URL(name, src), "utf8")))).join("\n");

  const orphaned = [...allowlist(bridge, "extension_request")].filter((method) => !code.includes(`"${method}"`)).sort();

  assert.deepEqual(orphaned, [], "these can no longer be reached from any page");
});
