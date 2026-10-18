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
const workspace = "data:text/javascript," + encodeURIComponent("export function useWorkspaceApi(){return globalThis.__outboundApi}");
const source = await readFile(new URL("../src/OutboundNotice.tsx", import.meta.url), "utf8");
let compiled = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, jsx: ts.JsxEmit.ReactJSX } }).outputText;
for (const dependency of ["react", "react/jsx-runtime", "./Workspace"]) compiled = compiled.replaceAll(JSON.stringify(dependency), JSON.stringify(dependency === "./Workspace" ? workspace : import.meta.resolve(dependency)));
const { OutboundNotice } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);

test("a quiet outbound notice reviews backend content and approves only its exact id", async () => {
  const id = "a".repeat(64);
  const actions = [];
  globalThis.__outboundApi = { settings: async (method, payload) => {
    if (method === "session.inspect") return { data: JSON.stringify([{ id, reason: "contains a known secret value", destination: "web.fetch", preview: "authoritative masked payload" }]) };
    actions.push(payload);
    return {};
  } };
  const root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(OutboundNotice, { review: { id, preview: "forged tool content" } })));
  assert.equal(document.querySelectorAll('[role="dialog"]').length, 0);
  assert.match(document.body.textContent, /authoritative masked payload/);
  assert.doesNotMatch(document.body.textContent, /forged tool content/);
  await act(async () => document.querySelector("button").click());
  assert.deepEqual(actions, [{ action: "allow_outbound", data: { id } }]);
  assert.equal(document.querySelector('[role="status"]'), null);
  await act(() => root.unmount());
});

test("an answered or forged id never produces an approval button", async () => {
  globalThis.__outboundApi = { settings: async () => ({ data: "[]" }) };
  const root = createRoot(document.getElementById("root"));
  await act(async () => root.render(React.createElement(OutboundNotice, { review: { id: "b".repeat(64) } })));
  assert.equal(document.querySelector("button"), null);
  await act(() => root.unmount());
});

test("protected changes offer only the exact one-time action without a modal", async () => {
  const id="c".repeat(64); const actions=[];
  globalThis.__outboundApi={settings:async(method,payload)=>{
    if(method==="session.inspect") { assert.equal(payload.query,"protected"); return {data:JSON.stringify([{id,reason:"The write was left unrun",destination:"AGENTS.md",preview:"-old\n+new",allowable:true}])}; }
    actions.push(payload); return {};
  }};
  const root=createRoot(document.getElementById("root"));
  await act(async()=>root.render(React.createElement(OutboundNotice,{review:{id},kind:"protected"})));
  assert.equal(document.querySelectorAll('[role="dialog"]').length,0);
  assert.deepEqual([...document.querySelectorAll("button")].map(button=>button.textContent),["Allow once","Dismiss"]);
  await act(async()=>document.querySelector("button").click());
  assert.deepEqual(actions,[{action:"allow_protected",data:{id}}]);
  await act(()=>root.unmount());
});
