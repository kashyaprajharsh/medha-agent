import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";
import { JSDOM } from "jsdom";
import React, { act } from "react";

const dom = new JSDOM('<!doctype html><div id="root"></div>', { url: "http://localhost/" });
for (const name of ["window", "document", "Node", "Element", "HTMLElement", "MouseEvent"]) globalThis[name] = dom.window[name];
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const { createRoot } = await import("react-dom/client");
const source = await readFile(new URL("../src/ErrorBoundary.tsx", import.meta.url), "utf8");
let compiled = ts.transpileModule(source, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
for (const name of ["react/jsx-runtime", "react"])
  compiled = compiled.replaceAll(JSON.stringify(name), JSON.stringify(import.meta.resolve(name)));
const { Boundary, reloadable } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
const h = React.createElement;

async function settle() {
  for (let turn = 0; turn < 5; turn++) await act(async () => await new Promise((done) => setTimeout(done, 0)));
}

test("a part that fails to load says so in its own place, the rest stays, and Try again loads it", async () => {
  let loads = 0;
  const Part = reloadable(() => {
    loads += 1;
    return loads === 1
      ? Promise.reject(new Error("the chunk did not load"))
      : Promise.resolve({ default: () => h("p", { id: "part" }, "terminal") });
  });
  const quiet = console.error;
  console.error = () => {};
  const root = createRoot(document.getElementById("root"));
  try {
    await act(async () =>
      root.render(
        h("div", null, h("p", { id: "rest" }, "chat"), h(Boundary, { what: "the terminal" }, h(React.Suspense, { fallback: null }, h(Part)))),
      ),
    );
    await settle();
    const alert = document.querySelector("[role=alert]");
    assert.ok(document.getElementById("rest"), "the rest of the window went with the failed part");
    assert.match(alert?.textContent ?? "", /Couldn't open the terminal/);
    assert.match(alert.textContent, /the chunk did not load/, "the reason is not shown");
    const again = [...alert.querySelectorAll("button")].find((button) => button.textContent === "Try again");
    await act(async () => again.click());
    await settle();
    assert.equal(loads, 2, "Try again did not load the part again");
    assert.ok(document.getElementById("part"), "the part did not appear after Try again");
    assert.equal(document.querySelector("[role=alert]"), null);
  } finally {
    console.error = quiet;
    await act(() => root.unmount());
  }
});
