import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";
import { JSDOM } from "jsdom";
import React, { act } from "react";

const dom = new JSDOM('<!doctype html><div id="root"></div>', { url: "http://localhost/" });
for (const name of ["window", "document", "Node", "Element", "HTMLElement"]) globalThis[name] = dom.window[name];
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const { createRoot } = await import("react-dom/client");
const source = await readFile(new URL("../src/windowGlass.ts", import.meta.url), "utf8");
let compiled = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext } }).outputText;
compiled = compiled.replaceAll(JSON.stringify("react"), JSON.stringify(import.meta.resolve("react")));
const { useWindowGlass } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);

async function glassWith(mac, ask) {
  let shown;
  function Harness() {
    shown = useWindowGlass(mac, ask);
    return null;
  }
  const root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(Harness)));
  await act(async () => await new Promise((done) => setTimeout(done, 0)));
  await act(() => root.unmount());
  return shown;
}

test("the window is glass on macOS, and elsewhere only where the system says it applied the material", async () => {
  let asked = 0;
  const count = (answer) => () => {
    asked += 1;
    return answer;
  };
  assert.equal(await glassWith(true, count(Promise.resolve(false))), true, "macOS lost its glass");
  assert.equal(asked, 0, "macOS asked the shell");
  assert.equal(await glassWith(false, count(Promise.resolve(true))), true, "an applied material was not used");
  assert.equal(await glassWith(false, count(Promise.resolve(false))), false, "glass without the material");
  assert.equal(await glassWith(false, count(Promise.reject(new Error("no window")))), false, "a failed check gave glass");
  assert.equal(asked, 3);
});
