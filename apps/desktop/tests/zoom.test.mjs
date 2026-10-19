import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";
import { JSDOM } from "jsdom";
import React, { act } from "react";

const dom = new JSDOM('<!doctype html><div id="root"></div><textarea></textarea>', { url: "http://localhost/" });
for (const name of ["window", "document", "Node", "Element", "HTMLElement", "KeyboardEvent", "WheelEvent", "MouseEvent"]) globalThis[name] = dom.window[name];
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const { createRoot } = await import("react-dom/client");
const source = await readFile(new URL("../src/OutputZoom.ts", import.meta.url), "utf8");
let compiled = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext } }).outputText;
compiled = compiled.replaceAll(JSON.stringify("react"), JSON.stringify(import.meta.resolve("react")));
const { useZoomGestures, stepZoom } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);

test("a pinch zooms the preview itself, and the keyboard never steals typing", async () => {
  let shown;
  function Harness() {
    const canvas = React.useRef(null);
    const [zoom, setZoom] = React.useState();
    shown = zoom;
    useZoomGestures(canvas, true, zoom, setZoom, false);
    return React.createElement("div", { ref: canvas, id: "canvas" });
  }
  const root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(Harness)));
  const canvas = document.getElementById("canvas");
  await act(async () => canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, ctrlKey: true, bubbles: true, cancelable: true })));
  assert.ok(shown > 2 && shown <= 8, `pinch zoomed to ${shown}`);
  await act(async () => canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, bubbles: true, cancelable: true })));
  assert.ok(shown > 2 && shown <= 8, "a plain scroll scrolls, it does not zoom");
  const typed = document.querySelector("textarea");
  await act(async () => typed.dispatchEvent(new KeyboardEvent("keydown", { key: "0", bubbles: true })));
  assert.notEqual(shown, undefined, "typing 0 in the composer kept the zoom");
  await act(async () => document.body.dispatchEvent(new KeyboardEvent("keydown", { key: "0", bubbles: true })));
  assert.equal(shown, undefined, "0 returns to Fit");
  await act(async () => canvas.dispatchEvent(new MouseEvent("dblclick", { clientX: 10, clientY: 10, bubbles: true })));
  assert.equal(shown, 2, "a double click zooms in where it lands");
  assert.equal(stepZoom(2.7, 1), 3, "buttons step on from wherever a pinch stopped");
  assert.equal(stepZoom(8, 1), 8);
  await act(() => root.unmount());
});
