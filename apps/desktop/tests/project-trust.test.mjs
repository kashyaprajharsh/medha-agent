import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";
import { JSDOM } from "jsdom";
import React, { act } from "react";

const source = await readFile(new URL("../src/live.ts", import.meta.url), "utf8");
const { outputText } = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext } });
const { reduceLive, startingLive } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`);

test("a trust notice never changes running work into an approval or a question", () => {
  const review = { root: "/checkout", folder: "/checkout/src", trusted: false, fingerprint: "exact-values", settings: [{ key: "memory.write_approval", value: "none", effect: "personal writes" }] };
  let state = reduceLive(startingLive(), { method: "event", params: { kind: "turn.started", turn: 1 } });
  state = reduceLive(state, { method: "event", params: { kind: "project.trust", review } });
  assert.equal(state.status, "running");
  assert.deepEqual(state.projectTrust, review);
  assert.deepEqual(state.approvals, []);
  assert.deepEqual(state.questions, []);
  state = reduceLive(state, { method: "event", params: { kind: "turn.done" } });
  assert.deepEqual(state.projectTrust, review, "the action remains available after the request finishes");
  const restored = reduceLive(startingLive(), { method: "session.presentation", params: { session: "chat", frames: [{ method: "event", params: { kind: "project.trust", review } }], running: false } });
  assert.deepEqual(restored.projectTrust, review);
  assert.deepEqual(restored.approvals, []);
});

const dom = new JSDOM('<!doctype html><div id="root"></div>', { url: "http://localhost/" });
for (const name of ["window", "document", "Node", "Element", "HTMLElement", "MouseEvent", "localStorage"]) globalThis[name] = dom.window[name];
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const { createRoot } = await import("react-dom/client");
const workspace = "data:text/javascript," + encodeURIComponent("export function useWorkspaceApi(){return globalThis.__trustApi}");
const component = await readFile(new URL("../src/ProjectTrustNotice.tsx", import.meta.url), "utf8");
let compiled = ts.transpileModule(component, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, jsx: ts.JsxEmit.ReactJSX } }).outputText;
for (const dependency of ["react", "react/jsx-runtime", "./Workspace"]) compiled = compiled.replaceAll(JSON.stringify(dependency), JSON.stringify(dependency === "./Workspace" ? workspace : import.meta.resolve(dependency)));
const { ProjectTrustNotice } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);

test("one quiet button accepts the shown scope and values, and an answered notice stays dismissed", async () => {
  localStorage.clear();
  const review = { root: "/checkout", folder: "/checkout/src", trusted: false, fingerprint: "exact-values", settings: [{ key: "memory.write_approval", value: "none", effect: "personal writes" }], ignored: ["memory.write_approval"] };
  const actions = [];
  globalThis.__trustApi = { settings: async (method, payload) => { if (method.endsWith(".change")) actions.push(payload); return { data: review }; } };
  let root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(ProjectTrustNotice, { review, session: "chat" })));
  assert.equal(document.querySelectorAll('[role="dialog"]').length, 0);
  assert.match(document.querySelector('[role="status"]').textContent, /memory.write_approval = "none"/);
  await act(async () => document.querySelector("button").click());
  assert.deepEqual(actions, [{ action: "trust_project", data: { root: review.root, folder: review.folder, fingerprint: review.fingerprint } }]);
  assert.equal(document.querySelector('[role="status"]'), null);
  await act(() => root.unmount());
  root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(ProjectTrustNotice, { review, session: "chat" })));
  assert.equal(document.querySelector('[role="status"]'), null);
  assert.equal(actions.length, 1);
  await act(() => root.unmount());
});

test("dismiss makes no trust change and persists for this folder and chat", async () => {
  localStorage.clear();
  const review = { root: "/untrusted", folder: "/untrusted", trusted: false, fingerprint: "values", settings: [], ignored: [] };
  let changes = 0;
  globalThis.__trustApi = { settings: async (method) => { if (method.endsWith(".change")) changes++; return { data: review }; } };
  let root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(ProjectTrustNotice, { review, session: "chat" })));
  await act(async () => [...document.querySelectorAll("button")].find(button => button.textContent === "Dismiss").click());
  await act(() => root.unmount());
  root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(ProjectTrustNotice, { review, session: "chat" })));
  assert.equal(document.querySelector('[role="status"]'), null);
  assert.equal(changes, 0);
  await act(() => root.unmount());
});
