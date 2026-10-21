import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import ts from "typescript";
import { JSDOM } from "jsdom";
import React, { act } from "react";
import JSZip from "jszip";

const dom = new JSDOM('<!doctype html><div id="root"></div>', {
  pretendToBeVisual: true,
  url: "http://localhost/",
});
for (const name of [
  "window",
  "document",
  "Node",
  "Element",
  "HTMLElement",
  "MouseEvent",
  "KeyboardEvent",
  "Event",
  "CustomEvent",
  "localStorage",
])
  globalThis[name] = dom.window[name];
const { createRoot } = await import("react-dom/client");
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
globalThis.matchMedia = () => ({ matches: true });
globalThis.ResizeObserver = class {
  observe() {}
  disconnect() {}
};
dom.window.HTMLElement.prototype.scrollIntoView = function () {};
globalThis.__medhaControlsTest = { active: true, api: {} };
const modules = new Map([
  [
    "Workspace",
    "data:text/javascript," +
      encodeURIComponent(
        "const scope={current:{chatKey:null,sessionId:null}}; export function useWorkspace(){return {scope,...globalThis.__medhaControlsTest}} export function useWorkspaceApi(){return globalThis.__medhaControlsTest.api}",
      ),
  ],
  [
    "api",
    "data:text/javascript," + encodeURIComponent("export const desktop = {chooseExtensionSource: (kind) => globalThis.__medhaControlsTest.chooseExtensionSource(kind), chooseAttachments: (kind) => globalThis.__medhaControlsTest.chooseAttachments(kind), admitImage: (bytes) => globalThis.__medhaControlsTest.admitImage(bytes)}"),
  ],
  [
    // The diagram engine needs a real browser; a whole diagram here is one that ends in a node.
    "outputRender",
    "data:text/javascript," +
      encodeURIComponent(
        "export async function drawDiagram(source){return /--> *\\w/.test(source.split('\\n').at(-1))?{svg:'<svg data-lines=\"'+source.split('\\n').length+'\"></svg>'}:{problem:'Its last line is unfinished.'}} export function drawingUrl(){return 'blob:drawing'}",
      ),
  ],
]);
async function moduleUrl(name, cache = modules) {
  if (cache.has(name)) return cache.get(name);
  const source = await readFile(
    new URL(`../src/${name}.tsx`, import.meta.url),
    "utf8",
  ).catch(() =>
    readFile(new URL(`../src/${name}.ts`, import.meta.url), "utf8"),
  );
  let { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.ESNext,
      jsx: ts.JsxEmit.ReactJSX,
    },
  });
  outputText = outputText.replaceAll(
    "import.meta.url",
    JSON.stringify(new URL(`../src/${name}.tsx`, import.meta.url).href),
  );
  for (const match of [...outputText.matchAll(/^import\s.*?\sfrom\s["']([^"'\r\n]+)["']/gm)]) {
    const dependency = match[1];
    const url = dependency.startsWith("./")
      ? await moduleUrl(dependency.slice(2), cache)
      : cache.get(dependency) ?? import.meta.resolve(dependency);
    outputText = outputText
      .replaceAll(`"${dependency}"`, JSON.stringify(url))
      .replaceAll(`'${dependency}'`, JSON.stringify(url));
  }
  const url = `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`;
  cache.set(name, url);
  return url;
}
const { Select } = await import(await moduleUrl("Select"));
const { Composer } = await import(await moduleUrl("Composer"));
const { useLive } = await import(await moduleUrl("useLive"));
const h = React.createElement;
let root;
test.beforeEach(() => {
  document.body.innerHTML = '<div id="root"></div>';
  root = createRoot(document.getElementById("root"));
  globalThis.__medhaControlsTest = { active: true, api: {} };
});
test.afterEach(async () => {
  await act(() => root.unmount());
});
async function click(element) {
  assert.ok(element, "click target exists");
  await act(async () => {
    element.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
    element.click();
  });
}
async function key(element, value) {
  await act(() =>
    element.dispatchEvent(
      new KeyboardEvent("keydown", { key: value, bubbles: true }),
    ),
  );
}
function option(label) {
  return [...document.querySelectorAll('[role="option"]')].find(
    (node) => node.textContent.trim() === label,
  );
}
const settings = {
  profile: "test",
  profiles: [],
  model: "test",
  mode: "careful",
  reasoning: "auto",
  effort: "auto",
  efforts: ["none", "minimal", "low", "medium", "high", "xhigh"],
  reasoning_support: "effort",
  streaming: true,
  context_limit: 32768,
};

test("click High applies the effort, switches Auto to On and keeps Thinking open", async () => {
  const changes = [];
  let sends = 0;
  function Harness() {
    const [current, setSettings] = React.useState(settings);
    const [control, setControl] = React.useState("reasoning");
    return h(Composer, {
      value: "",
      onChange() {},
      onSend() {
        sends++;
      },
      onStop() {},
      running: false,
      disabled: false,
      placeholder: "Task",
      settings: current,
      settingsLocked: false,
      control,
      onControl: setControl,
      onLoadSettings: async () => {},
      onConfigure: async (change) => {
        changes.push(change);
        setSettings({
          ...current,
          effort: change.effort,
          reasoning: change.effort === "auto" ? "auto" : "on",
        });
      },
      showReasoning: true,
      onShowReasoning() {},
      attachments: [],
      onAttachments() {},
      onSurface() {},
      onNew() {},
      onRewind() {},
      onSettings() {},
      onExtensions() {},
    });
  }
  await act(() => root.render(h(Harness)));
  await click(document.getElementById("effort"));
  await click(option("High"));
  assert.deepEqual(changes, [{ effort: "high" }]);
  assert.equal(document.getElementById("effort").textContent.trim(), "High");
  assert.ok(document.querySelector('[aria-label="reasoning settings"]'));
  const on = [...document.querySelectorAll(".segmented button")].find(
    (node) => node.textContent === "On",
  );
  assert.equal(on.getAttribute("aria-pressed"), "true");
  assert.equal(document.querySelector('[role="listbox"]'), null);
  assert.equal(sends, 0, "picking an effort must not submit the chat form");

  await click(document.getElementById("effort"));
  await key(document.getElementById("effort"), "Escape");
  assert.equal(document.querySelector('[role="listbox"]'), null);
  assert.ok(
    document.querySelector('[aria-label="reasoning settings"]'),
    "first Escape closes only the child picker",
  );
});

test("all shared pickers select by mouse and keyboard without submitting their form", async () => {
  let submitted = 0;
  function Harness() {
    const [value, setValue] = React.useState("a");
    return h(
      "form",
      {
        onSubmit(event) {
          event.preventDefault();
          submitted++;
        },
      },
      h(
        Select,
        {
          id: "picker",
          value,
          onChange: (event) => setValue(event.target.value),
        },
        h("option", { value: "a" }, "One"),
        h("option", { value: "b" }, "Two"),
        h("option", { value: "c", disabled: true }, "Unavailable"),
      ),
    );
  }
  await act(() => root.render(h(Harness)));
  await click(document.getElementById("picker"));
  await click(option("Two"));
  assert.equal(document.getElementById("picker").textContent, "Two");
  await key(document.getElementById("picker"), "ArrowDown");
  await key(document.getElementById("picker"), "Home");
  await key(document.getElementById("picker"), "Enter");
  assert.equal(document.getElementById("picker").textContent, "One");
  assert.equal(submitted, 0);
});

test("hover does not scroll the page and a picker near the bottom opens above", async () => {
  await act(() =>
    root.render(
      h(
        Select,
        { id: "picker", value: "a", onChange() {} },
        h("option", { value: "a" }, "One"),
        h("option", { value: "b" }, "Two"),
      ),
    ),
  );
  const trigger = document.getElementById("picker");
  trigger.getBoundingClientRect = () => ({
    left: 30,
    right: 230,
    top: window.innerHeight - 60,
    bottom: window.innerHeight - 20,
    width: 200,
    height: 40,
  });
  await click(trigger);
  const menu = document.querySelector('[role="listbox"]');
  assert.ok(menu.style.bottom);
  assert.equal(menu.style.top, "");
  menu.scrollTop = 15;
  const originalScroll = HTMLElement.prototype.scrollIntoView;
  let ancestorScrolls = 0;
  HTMLElement.prototype.scrollIntoView = () => {
    ancestorScrolls++;
  };
  try {
    await act(() =>
      option("Two").dispatchEvent(
        new MouseEvent("pointermove", { bubbles: true }),
      ),
    );
    assert.equal(menu.scrollTop, 15);
    assert.equal(ancestorScrolls, 0);
    await click(document.body);
    assert.equal(document.querySelector('[role="listbox"]'), null);
  } finally {
    HTMLElement.prototype.scrollIntoView = originalScroll;
  }
});

test("session settings and effort changes update from the acknowledgement even without a notification", async () => {
  const calls = [];
  globalThis.__medhaControlsTest.api = {
    onLive: async () => () => {},
    liveOpen: async () => {},
    liveCall: async (_key, method, change) => {
      calls.push({ method, change });
      return method === "session.configure"
        ? { ...settings, reasoning: "on", effort: change.effort }
        : settings;
    },
  };
  let live;
  function Harness() {
    live = useLive();
    return null;
  }
  await act(() => root.render(h(Harness)));
  await act(() => live.ensureOpen("draft", null));
  assert.equal(live.live.draft.settings.reasoning, "auto");
  await act(() => live.configure("draft", null, { effort: "high" }));
  assert.equal(live.live.draft.settings.effort, "high");
  assert.equal(live.live.draft.settings.reasoning, "on");
  assert.equal(calls.length, 2);
});

test("a remounted renderer recovers its native chat and sends Stop to the same handle", async () => {
  const cache = new Map(modules);
  cache.delete("api");
  cache.delete("useLive");
  const stub = (name, source) => cache.set(name, "data:text/javascript," + encodeURIComponent(source));
  stub("@tauri-apps/api/core", "export const invoke=(...args)=>globalThis.__medhaNative.invoke(...args); export class Channel {}");
  stub("@tauri-apps/api/event", "export const listen=(...args)=>globalThis.__medhaNative.listen(...args)");
  const { createApi } = await import(await moduleUrl("api", cache));
  const { useLive: recoveredLive } = await import(await moduleUrl("useLive", cache));
  const listeners = new Set(), requests = [];
  const session = "01M4HZ00C8NHK59J3NRBZKMQS4";
  const nativeKey = "personal-draft-original";
  let next = 0, running = true;
  const emit = (frame) => {
    for (const callback of listeners) callback({ payload: { key: nativeKey, frame } });
  };
  globalThis.__medhaNative = {
    listen: async (name, callback) => {
      assert.equal(name, "medha-live");
      listeners.add(callback);
      return () => listeners.delete(callback);
    },
    invoke: async (name, args) => {
      if (name === "live_open") return { key: nativeKey, reused: true };
      if (name === "live_focus") { assert.equal(args.key, nativeKey); return; }
      assert.equal(name, "live_request");
      assert.equal(args.key, nativeKey, "resume, send and Stop must address the original native viewer");
      requests.push(args.method);
      const id = ++next;
      if (args.method === "cancel") running = false;
      const result = args.method === "session.presentation"
        ? { session, running, settings, approvals: [], questions: [], agents: [], frames: running ? [
            { method: "event", params: { kind: "model.text", delta: "Already streamed", html: "<p>Already streamed</p>" } },
          ] : [] }
        : {};
      emit({ id, result, ...(args.method === "session.presentation" ? { method: "session.presentation" } : {}) });
      if (args.method === "session.presentation" && running) {
        emit({ method: "event", params: { kind: "model.text", delta: " after snapshot", html: "<p>Already streamed after snapshot</p>" } });
        emit({ method: "event", params: { kind: "model.waiting" } });
      }
      if (args.method === "message.send") emit({ method: "event", params: { kind: "message.accepted", content: args.params.content } });
      if (args.method === "cancel") emit({ method: "event", params: { kind: "turn.cancelled" } });
      return id;
    },
  };
  const api = createApi("personal", () => ({ chatKey: session, sessionId: session }));
  globalThis.__medhaControlsTest.api = api;
  let live;
  function Harness() { live = recoveredLive(); return null; }
  await act(() => root.render(h(Harness)));
  await act(() => live.ensureOpen(session, session));
  assert.equal(live.live[session].sessionId, session);
  assert.equal(live.live[session].status, "running");
  assert.equal(live.live[session].items[0].text, "Already streamed after snapshot", "recovery cannot overwrite frames arriving after its snapshot");
  await act(() => live.send(session, session, "Follow up"));
  await act(() => live.cancel(session));
  assert.equal(live.live[session].status, "idle");
  assert.deepEqual(requests, ["session.presentation", "message.send", "cancel"]);
  // A first send after remount must survive the idle snapshot without being
  // left as a second, unpositioned optimistic message.
  await act(() => root.render(h(Harness, { key: "remounted" })));
  await act(() => live.send(session, session, "Send after remount"));
  const user = live.live[session].items.filter((item) => item.kind === "user");
  assert.equal(user.length, 1);
  assert.equal(user[0].id, live.live[session].sent[0].id);
  await act(() => emit({ method: "exit", params: { stderr: "" } }));
  assert.equal(live.live[session].status, "exited", "ended frames must use the recovered alias too");
});

test("stream bursts paint once without losing text; controls flush immediately", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let receive, live;
  let renders = 0;
  globalThis.__medhaControlsTest.api = {
    onLive: async (callback) => {
      receive = callback;
      return () => {};
    },
  };
  function Harness() {
    renders++;
    live = useLive();
    return null;
  }
  await act(() => root.render(h(Harness)));
  const initial = renders;
  await act(() => {
    for (let i = 0; i < 1000; i++)
      receive("chat", {
        method: "event",
        params: { kind: "model.text", delta: `${i},`, html: `<p>${i}</p>` },
      });
  });
  assert.equal(renders, initial);
  await act(() => t.mock.timers.tick(50));
  assert.equal(renders, initial + 1);
  assert.equal(
    live.live.chat.items[0].text,
    Array.from({ length: 1000 }, (_, i) => `${i},`).join(""),
  );
  assert.equal(live.live.chat.items[0].html, "<p>999</p>");
  await act(() => {
    receive("chat", {
      method: "event",
      params: { kind: "model.text", delta: "tail" },
    });
    receive("chat", {
      method: "approval",
      params: { gate_id: 1, action: "write" },
    });
  });
  assert.ok(live.live.chat.items[0].text.endsWith("tail"));
  assert.equal(live.live.chat.approvals.length, 1);
  await act(() => {
    receive("chat", {
      method: "event",
      params: { kind: "model.text", delta: "final" },
    });
    receive("chat", { method: "event", params: { kind: "turn.done" } });
  });
  assert.ok(live.live.chat.items[0].text.endsWith("tailfinal"));
  assert.equal(live.live.chat.status, "idle");
  const complete = renders;
  await act(() => t.mock.timers.tick(100));
  assert.equal(renders, complete);
});

test("typing remains immediate with pending streams and separate sessions", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let receive, live, type;
  let unlistened = false;
  globalThis.__medhaControlsTest.api = {
    onLive: async (callback) => {
      receive = callback;
      return () => {
        unlistened = true;
      };
    },
  };
  function Harness() {
    live = useLive();
    const [value, setValue] = React.useState("");
    type = setValue;
    return h("textarea", {
      value,
      onChange: (event) => setValue(event.target.value),
    });
  }
  await act(() => root.render(h(Harness)));
  for (const word of ["what", " is", " going", " on"])
    await act(() => {
      receive("a", {
        method: "event",
        params: { kind: "model.text", delta: "A" },
      });
      receive("b", {
        method: "event",
        params: { kind: "model.reasoning", delta: "B" },
      });
      type((value) => value + word);
    });
  assert.equal(document.querySelector("textarea").value, "what is going on");
  assert.equal(live.live.a, undefined);
  await act(() => t.mock.timers.tick(50));
  assert.equal(live.live.a.items[0].text, "AAAA");
  assert.equal(live.live.b.items[0].text, "BBBB");
  const snapshot = live.live;
  await act(() => receive("a", { id: 1, result: {} }));
  assert.equal(live.live, snapshot);
  await act(() =>
    receive("a", {
      method: "event",
      params: { kind: "model.text", delta: "pending" },
    }),
  );
  await act(() => root.unmount());
  root = { unmount() {} };
  assert.equal(unlistened, true);
  await act(() => t.mock.timers.tick(100));
  assert.equal(live.live, snapshot);
});

test("unchanged Markdown and file code reuse highlighting during parent updates", async () => {
  const { Markdown, SourceCode } = await import(await moduleUrl("Markdown"));
  const { default: hljs } = await import("highlight.js/lib/common");
  let highlights = 0;
  const counter = { "before:highlight": () => highlights++ };
  hljs.addPlugin(counter);
  const code = "const perfRegressionMarker = 123;";
  const html = `<pre data-lang="javascript"><code>${code}</code></pre>`;
  function Harness({ tick, content = html }) {
    return h(
      "div",
      null,
      h("span", null, tick),
      h(Markdown, { html: content }),
      h(SourceCode, { text: code, language: "javascript" }),
    );
  }
  try {
    await act(() => root.render(h(Harness, { tick: 0 })));
    assert.equal(highlights, 1);
    for (let tick = 1; tick <= 10; tick++)
      await act(() => root.render(h(Harness, { tick })));
    assert.equal(highlights, 1);
    await act(() =>
      root.render(h(Harness, { tick: 11, content: html + "<p>New text</p>" })),
    );
    assert.equal(highlights, 1);
    assert.ok(document.querySelector("code .hljs-keyword"));
    assert.equal(document.querySelectorAll(".code-copy").length, 2);
  } finally {
    hljs.removePlugin(counter);
  }
});

test("large closed tool output and reasoning stay unmounted until opened", async () => {
  const { StepGroup, Reasoning } = await import(await moduleUrl("Steps"));
  const { default: hljs } = await import("highlight.js/lib/common");
  let highlights = 0;
  const counter = { "before:highlight": () => highlights++ };
  hljs.addPlugin(counter);
  const steps = Array.from({ length: 100 }, (_, i) => ({
    id: String(i),
    tool: "read",
    verb: "Read",
    target: `file${i}.py`,
    filePath: `file${i}.py`,
    status: "ok",
    output: `print(${i})\n`.repeat(2000),
  }));
  function Harness({ text }) {
    return h(
      "div",
      null,
      h(StepGroup, { steps, live: true }),
      h(Reasoning, { text, durationMs: 1000 }),
    );
  }
  try {
    await act(() =>
      root.render(h(Harness, { text: "long reasoning ".repeat(30000) })),
    );
    assert.equal(
      highlights,
      0,
      "closed step details do not run syntax highlighting",
    );
    assert.equal(document.querySelectorAll(".source-code").length, 0);
    assert.equal(document.querySelector(".thought-text"), null);
    await click(document.querySelector(".thought-head"));
    assert.ok(
      document.querySelector(".thought-text").textContent.length > 300000,
    );
    await click(document.querySelector(".thought-head"));
    const frozen = document.querySelector(".thought-text").textContent;
    await act(() => root.render(h(Harness, { text: "new reasoning" })));
    assert.equal(document.querySelector(".thought-text").textContent, frozen);
    await click(document.querySelector(".thought-head"));
    assert.equal(
      document.querySelector(".thought-text").textContent,
      "new reasoning",
    );
    await click(document.querySelector(".step-main"));
    assert.equal(highlights, 1);
    assert.equal(document.querySelectorAll(".source-code").length, 1);
  } finally {
    hljs.removePlugin(counter);
  }
});

test("the actual composer types locally during streaming, sends the current draft and resets", async () => {
  let draft = "",
    sends = 0,
    parentRenders = 0;
  let rerender, renderStale;
  function Harness() {
    parentRenders++;
    const [revision, setRevision] = React.useState(0);
    rerender = setRevision;
    const [stale, setStale] = React.useState(false);
    renderStale = setStale;
    return h(Composer, {
      value: stale ? "" : draft,
      resetRevision: revision,
      onChange(value) {
        draft = value;
      },
      onSend() {
        sends++;
        assert.equal(draft, "what is going on");
        draft = "";
        setRevision((v) => v + 1);
      },
      onStop() {},
      running: true,
      disabled: false,
      placeholder: "Task",
      settings,
      settingsLocked: true,
      control: null,
      onControl() {},
      onLoadSettings: async () => {},
      onConfigure: async () => {},
      showReasoning: false,
      onShowReasoning() {},
      attachments: [],
      onAttachments() {},
      onSurface() {},
      onNew() {},
      onRewind() {},
      onSettings() {},
      onExtensions() {},
    });
  }
  await act(() => root.render(h(Harness)));
  const initial = parentRenders;
  const textarea = document.querySelector("textarea");
  const nativeSetter = Object.getOwnPropertyDescriptor(
    window.HTMLTextAreaElement.prototype,
    "value",
  ).set;
  for (const value of [
    "what",
    "what is",
    "what is going",
    "what is going on",
  ]) {
    await act(() => {
      nativeSetter.call(textarea, value);
      textarea.dispatchEvent(new Event("input", { bubbles: true }));
    });
    assert.equal(draft, value);
    assert.equal(textarea.value, value);
  }
  assert.equal(parentRenders, initial, "keystrokes do not render the app");
  await act(() => rerender(1));
  assert.equal(textarea.value, draft, "background updates preserve the draft");
  await act(() => renderStale(true));
  assert.equal(
    textarea.value,
    draft,
    "a stale background render cannot overwrite typing",
  );
  await key(textarea, "Enter");
  assert.equal(sends, 1);
  assert.equal(
    textarea.value,
    "",
    "send clears even an initially empty parent value",
  );
});

test("session date groups start with Today open and older groups collapsed", async (t) => {
  t.mock.timers.enable({
    apis: ["Date"],
    now: new Date(2026, 8, 26, 12).getTime(),
  });
  const { Sidebar } = await import(await moduleUrl("Sidebar"));
  const session = (id, day, month = 8) => ({
    id,
    title: id,
    parent_id: null,
    events: 1,
    started_ts: new Date(2026, month, day, 10).getTime() / 1000,
    last_ts: new Date(2026, month, day, 10).getTime() / 1000,
  });
  const selected = [];
  const props = {
    sessions: [
      session("today", 26),
      session("week", 21),
      session("month", 5),
      session("older", 14, 7),
      session("running", 14, 7),
      session("needs", 14, 7),
      session("pinned", 14, 7),
    ],
    running: new Set(["running"]),
    needs: new Set(["needs"]),
    pinned: new Set(["pinned"]),
    subagentCounts: new Map(),
    selected: null,
    onSelect: (id) => selected.push(id),
    onTogglePin() {},
    onRefresh() {},
    onNew() {},
    onPalette() {},
    onExtensions() {},
    onSettings() {},
    onPointerEnter() {},
    onPointerLeave() {},
    onToggleTheme() {},
    page: "chat",
    workspace: "/test/workspace",
    workspaceName: "workspace",
    loading: false,
    drafting: false,
    theme: "dark",
    visible: true,
  };
  await act(() => root.render(h(Sidebar, props)));
  const toggle = (label) =>
    [...document.querySelectorAll(".group-toggle")].find(
      (node) => node.querySelector("span").textContent === label,
    );
  assert.equal(toggle("Today").getAttribute("aria-expanded"), "true");
  for (const label of ["This week", "This month", "Older"])
    assert.equal(toggle(label).getAttribute("aria-expanded"), "false");
  const titles = () =>
    [...document.querySelectorAll(".sess-title")].map(
      (node) => node.textContent,
    );
  assert.deepEqual(titles(), ["needs", "running", "pinned", "today"]);
  await click(toggle("This week"));
  assert.ok(titles().includes("week"));
  await click(
    [...document.querySelectorAll(".sess")].find(
      (node) => node.querySelector(".sess-title").textContent === "week",
    ),
  );
  assert.deepEqual(selected, ["week"]);
  await act(() => root.render(h(Sidebar, { ...props, selected: "week" })));
  assert.equal(
    toggle("This week").getAttribute("aria-expanded"),
    "true",
    "refresh preserves expansion",
  );
  await click(toggle("This week"));
  await click(toggle("Today"));
  assert.deepEqual(titles(), ["needs", "running", "pinned"]);
});

test("calendar groups do not put last Sunday's session into this week's Monday", async (t) => {
  t.mock.timers.enable({
    apis: ["Date"],
    now: new Date(2026, 1, 2, 12).getTime(),
  });
  const { Sidebar } = await import(await moduleUrl("Sidebar"));
  const props = {
    sessions: [1, 2].map((day) => ({
      id: String(day),
      title: day === 1 ? "Sunday" : "Monday",
      parent_id: null,
      events: 1,
      started_ts: 0,
      last_ts: new Date(2026, 1, day, 10).getTime() / 1000,
    })),
    running: new Set(),
    needs: new Set(),
    pinned: new Set(),
    subagentCounts: new Map(),
    selected: null,
    onSelect() {},
    onTogglePin() {},
    onRefresh() {},
    onNew() {},
    onPalette() {},
    onExtensions() {},
    onSettings() {},
    onPointerEnter() {},
    onPointerLeave() {},
    onToggleTheme() {},
    page: "chat",
    workspace: "/test",
    workspaceName: "test",
    loading: false,
    drafting: false,
    theme: "dark",
    visible: true,
  };
  await act(() => root.render(h(Sidebar, props)));
  assert.equal(
    [...document.querySelectorAll(".group-toggle")].some((node) =>
      node.textContent.includes("This week"),
    ),
    false,
  );
  assert.deepEqual(
    [...document.querySelectorAll(".sess-title")].map(
      (node) => node.textContent,
    ),
    ["Monday"],
  );
  await click(
    [...document.querySelectorAll(".group-toggle")].find((node) =>
      node.textContent.includes("This month"),
    ),
  );
  assert.ok(
    [...document.querySelectorAll(".sess-title")].some(
      (node) => node.textContent === "Sunday",
    ),
  );
});

test("a path approval offers each answer the backend sends, and the folder says which folder", async () => {
  const { LiveTail } = await import(await moduleUrl("LiveTail"));
  const { reduceLive, startingLive } = await import(await moduleUrl("live"));
  let state = reduceLive(startingLive(), { method: "ready", params: { session: "s" } });
  state = reduceLive(state, {
    method: "approval",
    params: {
      gate_id: 4,
      action: "agent 'docs' · Read access to /work/proj/src/a.txt",
      detail: "This path is outside the workspace: /work/proj/src/a.txt",
      escalated: false,
      kind: "path",
      choices: ["once", "always", "folder", "deny"],
      folder: "/work/proj/src",
      path: {
        path: "/work/proj/src/a.txt",
        kind: "file",
        access: "read",
      },
    },
  });
  const answered = [];
  await act(() =>
    root.render(
      h(LiveTail, {
        state,
        showReasoning: false,
        onAnswer() {},
        onApprove: (approval, decision) => answered.push([approval.gateId, decision]),
        onOpenAgent() {},
      }),
    ),
  );
  const card = document.querySelector(".ask");
  assert.equal(card.querySelector(".ask-top b").textContent, "agent 'docs' wants read access to /work/proj/src/a.txt");
  const buttons = [...card.querySelectorAll(".ask-actions button")];
  assert.deepEqual(
    buttons.map((button) => button.textContent),
    ["Allow", "Always allow this file", "Always allow this folder", "Deny"],
  );
  assert.match(card.textContent, /Its folder: \/work\/proj\/src/);
  await click(buttons[2]);
  assert.deepEqual(answered, [[4, "folder"]]);
});

test("directory and unknown targets never claim an exact file grant", async () => {
  const { LiveTail } = await import(await moduleUrl("LiveTail"));
  const { reduceLive, startingLive } = await import(await moduleUrl("live"));
  for (const [kind, label] of [
    ["directory", "Always allow this folder and its contents"],
    ["unknown", "Always allow this path"],
  ]) {
    let state = reduceLive(startingLive(), { method: "ready", params: { session: "s" } });
    state = reduceLive(state, { method: "approval", params: {
      gate_id: 9, action: "Write access to /work/other/target", escalated: false,
      kind: "path", choices: ["once", "always", "deny"],
      path: { path: "/work/other/target", kind, access: "write" },
    }});
    await act(() => root.render(h(LiveTail, { state, onAnswer() {}, onApprove() {}, onOpenAgent() {} })));
    const labels = [...document.querySelectorAll(".ask-actions button")].map(button => button.textContent);
    assert.deepEqual(labels, ["Allow", label, "Deny"]);
    assert.equal(labels.includes("Always allow this file"), false);
  }
});

test("a tool call still being written says what Medha is doing, where a caret alone looked stuck", async () => {
  const { LiveTail } = await import(await moduleUrl("LiveTail"));
  const { reduceLive, startingLive } = await import(await moduleUrl("live"));
  const event = (kind, params = {}) => ({ method: "event", params: { kind, ...params } });
  const status = () => document.querySelector(".live-status > span:last-child")?.textContent;
  let state = reduceLive(startingLive(), { method: "ready", params: { session: "s" } });
  const then = async (kind, params) => {
    state = reduceLive(state, event(kind, params));
    await act(() => root.render(h(LiveTail, { state, onAnswer() {}, onApprove() {}, onOpenAgent() {} })));
  };
  await then("message.accepted", { content: "make a page" });
  await then("turn.started", { turn: 1 });
  await then("model.text", { delta: "Building" });
  assert.ok(document.querySelector(".prose.streaming"), "text that is still arriving keeps its caret");
  assert.equal(status(), undefined);

  // A large file takes minutes to arrive as one call. Nothing else is sent meanwhile.
  await then("tool.started", { tool: "write" });
  assert.equal(document.querySelector(".prose.streaming"), null, "the text is finished, so its caret goes");
  assert.equal(status(), "Writing a file");
  await then("tool.started", { tool: "write", target: "/work/site/react-agent.html" });
  assert.equal(status(), "Writing react-agent.html");
  assert.equal(state.activity.started > 0 && state.activity.tool, "write");

  await then("tool.call", { id: "c1", tool: "write", verb: "Write", target: "react-agent.html" });
  assert.equal(state.activity, undefined, "the call itself takes over as a running step");
  assert.equal(status(), undefined);

  // Any other tool keeps the plain line, never a raw tool name.
  await then("tool.started", { tool: "mcp__drawing__create_view" });
  assert.equal(status(), "Working");
  await then("model.restarted");
  assert.equal(state.activity, undefined, "a retry starts the reply again");
});

test("action approvals keep their action label", async () => {
  const { LiveTail } = await import(await moduleUrl("LiveTail"));
  const { reduceLive, startingLive } = await import(await moduleUrl("live"));
  let state = reduceLive(startingLive(), { method: "approval", params: {
    gate_id: 2, action: "shell command", escalated: false,
    kind: "action", choices: ["once", "always", "deny"],
  }});
  await act(() => root.render(h(LiveTail, { state, onAnswer() {}, onApprove() {}, onOpenAgent() {} })));
  assert.deepEqual([...document.querySelectorAll(".ask-actions button")].map(button => button.textContent), ["Allow", "Always allow", "Deny"]);
});

test("a long compaction says so instead of looking stuck on Working", async () => {
  const { LiveTail } = await import(await moduleUrl("LiveTail"));
  const { reduceLive, startingLive, recordSent } = await import(await moduleUrl("live"));
  const event = (kind, params = {}) => ({ method: "event", params: { kind, ...params } });
  let state = recordSent(startingLive(), { kind: "user", id: "u", text: "forget it", ts: 1 });
  state = reduceLive(state, { method: "ready", params: { session: "s" } });
  const status = () => document.querySelector(".live-status > span:last-child")?.textContent;
  const render = () =>
    root.render(h(LiveTail, { state, showReasoning: false, onAnswer() {}, onApprove() {}, onOpenAgent() {} }));
  state = reduceLive(state, event("compacting", { active: true }));
  await act(render);
  assert.match(status(), /Condensing earlier messages/);
  state = reduceLive(state, event("compacting", { active: false }));
  await act(render);
  assert.equal(status(), "Working");
});

test("live follow-ups render between agent responses and pending ones stay at the bottom", async () => {
  const { LiveTail } = await import(await moduleUrl("LiveTail"));
  const { reduceLive, startingLive, recordSent } = await import(
    await moduleUrl("live")
  );
  const event = (kind, params = {}) => ({
    method: "event",
    params: { kind, ...params },
  });
  let state = recordSent(startingLive(), {
    kind: "user",
    id: "initial",
    text: "Initial task",
    ts: 1,
  });
  state = reduceLive(
    state,
    event("model.text", { delta: "Earlier agent work" }),
  );
  state = recordSent(state, {
    kind: "user",
    id: "followup",
    text: "Follow-up instruction",
    ts: 2,
  });
  const render = () =>
    root.render(
      h(LiveTail, {
        state,
        showReasoning: false,
        onAnswer() {},
        onApprove() {},
        onOpenAgent() {},
      }),
    );
  const rows = () =>
    [
      ...document.querySelectorAll(".live-tail > .you, .live-tail > .prose"),
    ].map((node) => node.querySelector("p")?.textContent ?? node.textContent);
  await act(render);
  assert.deepEqual(rows(), [
    "Initial task",
    "Earlier agent work",
    "Follow-up instruction",
  ]);
  state = reduceLive(
    state,
    event("message.steered", { content: "Follow-up instruction" }),
  );
  state = reduceLive(
    state,
    event("model.text", { delta: "Work after steering" }),
  );
  await act(render);
  assert.deepEqual(rows(), [
    "Initial task",
    "Earlier agent work",
    "Follow-up instruction",
    "Work after steering",
  ]);
  assert.equal(document.querySelectorAll(".live-tail > .turn-head").length, 2);
  state = recordSent(state, {
    kind: "user",
    id: "pending",
    text: "Unread follow-up",
    ts: 3,
  });
  await act(render);
  assert.equal(rows().at(-1), "Unread follow-up");
  state = reduceLive(state, event("turn.cancelled"));
  await act(render);
  assert.equal(rows().at(-1), "Unread follow-up");
});

test("sending through useLive preserves the initial prompt and mid-turn follow-up positions", async () => {
  let receive, live;
  globalThis.__medhaControlsTest.api = {
    onLive: async (callback) => {
      receive = callback;
      return () => {};
    },
    liveOpen: async () => {},
    liveCall: async (_key, method) =>
      method === "session.settings" ? settings : { accepted: true },
  };
  function Harness() {
    live = useLive();
    return null;
  }
  await act(() => root.render(h(Harness)));
  await act(() => live.send("chat", null, "Original request"));
  assert.equal(live.live.chat.items[0].kind, "user");
  assert.equal(live.live.chat.items[0].text, "Original request");
  await act(() => {
    receive("chat", {
      method: "event",
      params: { kind: "model.text", delta: "Earlier work" },
    });
    receive("chat", {
      method: "event",
      params: { kind: "tool.call", id: "tool", tool: "read" },
    });
  });
  await act(() => live.send("chat", null, "Please change direction"));
  assert.deepEqual(
    live.live.chat.items.map((item) => item.kind),
    ["user", "text", "tools"],
  );
  await act(() =>
    receive("chat", {
      method: "event",
      params: { kind: "message.steered", content: "Please change direction" },
    }),
  );
  assert.deepEqual(
    live.live.chat.items.map((item) => item.kind),
    ["user", "text", "tools", "user"],
  );
  assert.equal(live.live.chat.items.at(-1).text, "Please change direction");
});

test("closed work groups update completed steps and release their spinners", async () => {
  const { StepGroup } = await import(await moduleUrl("Steps"));
  const running = {
    id: "long-tool",
    tool: "shell.exec",
    verb: "Ran",
    status: "running",
    started: 1,
  };
  await act(() => root.render(h(StepGroup, { steps: [running], live: true })));
  assert.equal(document.querySelectorAll(".step.running .spinner").length, 1);
  await click(document.querySelector(".work-head"));
  assert.equal(
    document.querySelector(".work-head").getAttribute("aria-expanded"),
    "false",
  );
  await act(() =>
    root.render(
      h(StepGroup, {
        steps: [{ ...running, status: "ok", ended: 2 }],
        live: true,
      }),
    ),
  );
  assert.equal(document.querySelectorAll(".step.running .spinner").length, 0);
  assert.equal(document.querySelectorAll(".step.running").length, 0);
});

test("a long message folds behind Show more only when it overflows", async () => {
  const { UserText } = await import(await moduleUrl("UserText"));
  const proto = dom.window.HTMLElement.prototype;
  const tall = (height) =>
    Object.defineProperty(proto, "scrollHeight", { configurable: true, get: () => height });
  Object.defineProperty(proto, "clientHeight", { configurable: true, get: () => 150 });
  try {
    tall(150);
    await act(() => root.render(h(UserText, { text: "short" })));
    assert.equal(document.querySelector(".you-toggle"), null, "nothing hidden, no toggle");

    tall(900);
    await act(() => root.render(h(UserText, { text: "long\n".repeat(12) })));
    const toggle = document.querySelector(".you-toggle");
    assert.equal(toggle.textContent, "Show more");
    assert.match(document.querySelector(".you-text p").className, /folded faded/);
    await act(() => toggle.click());
    assert.equal(toggle.getAttribute("aria-expanded"), "true");
    assert.equal(toggle.textContent, "Show less");
    assert.equal(document.querySelector(".you-text p").className, "");
  } finally {
    delete proto.scrollHeight;
    delete proto.clientHeight;
  }
});

test("a paste past the limit is a card whose sheet shows all of it and closes on Escape", async () => {
  const { UserText } = await import(await moduleUrl("UserText"));
  const text = Array.from({ length: 40 }, (_, i) => `line ${i + 1}`).join("\n");
  await act(() => root.render(h(UserText, { text })));
  const card = document.querySelector(".pasted");
  assert.equal(document.querySelector(".you-text p"), null, "no inline wall of text");
  assert.match(card.textContent, /Pasted content/);
  assert.match(card.querySelector(".pasted-size").textContent, /^40 lines/);
  assert.equal(card.querySelector(".pasted-preview").textContent, "line 1\nline 2");

  await act(() => card.click());
  const sheet = document.body.querySelector(".pasted-sheet");
  assert.equal(sheet.getAttribute("role"), "dialog");
  assert.equal(sheet.querySelector(".pasted-body").textContent, text);
  let reachedApp = false;
  const app = () => (reachedApp = true);
  window.addEventListener("keydown", app);
  await act(() =>
    window.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "Escape" })),
  );
  window.removeEventListener("keydown", app);
  assert.equal(document.body.querySelector(".pasted-sheet"), null);
  assert.equal(reachedApp, false, "Escape closes the sheet only");

  await act(() => root.render(h(UserText, { text: "a short question" })));
  assert.equal(document.querySelector(".pasted"), null);
});

test("thinking stays in the chat as a collapsed row that opens on click", async () => {
  const { Transcript } = await import(await moduleUrl("Transcript"));
  const blocks = [
    { kind: "user", id: "u", text: "hii", ts: 1 },
    { kind: "reasoning", id: "r", text: "A greeting.", durationMs: 1200, ts: 2 },
    { kind: "assistant", id: "a", text: "Hey!", ts: 3 },
  ];
  const head = () => document.querySelector(".thought-head");
  await act(() => root.render(h(Transcript, { blocks, showReasoning: false })));
  assert.equal(head().textContent, "Thought for 1.2s");
  assert.equal(head().getAttribute("aria-expanded"), "false");
  await click(head());
  assert.equal(head().getAttribute("aria-expanded"), "true");
  await act(() => root.render(h(Transcript, { blocks, showReasoning: true })));
  assert.equal(head().getAttribute("aria-expanded"), "true");
  assert.equal(document.querySelectorAll(".turn-head").length, 1);
});

test("Markdown adds only one copy control under StrictMode effect replay", async () => {
  const { Markdown } = await import(await moduleUrl("Markdown"));
  await act(() =>
    root.render(
      h(
        React.StrictMode,
        null,
        h(Markdown, {
          html: '<pre data-lang="python"><code>print(123)</code></pre>',
        }),
      ),
    ),
  );
  assert.equal(document.querySelectorAll(".code-copy").length, 1);
});

test("a diagram grows while it is written, keeps its last good drawing, and opens only when whole", async () => {
  const { Markdown } = await import(await moduleUrl("Markdown"));
  const { OutputsProvider } = await import(await moduleUrl("outputContext"));
  const shown = [];
  const outputs = { open: null, all: [], viewing: false, stopped: [], online: [], show: (output) => shown.push(output) };
  const reply = async (body, streaming) => {
    await act(() =>
      root.render(
        h(
          OutputsProvider,
          { value: outputs },
          h(Markdown, {
            html: `<p>Here.</p><pre data-lang="mermaid"><code>${body}</code></pre>`,
            scope: "live-0",
            streaming,
          }),
        ),
      ),
    );
    // Effects start once the render settles; while writing it redraws on a beat.
    await act(() => new Promise((resolve) => setTimeout(resolve, 300)));
  };
  const plate = () => document.querySelector(".out-plate");

  await reply("flowchart LR\n  A --&gt;", true);
  assert.equal(document.querySelector(".prose").textContent, "Here.");
  assert.equal(plate().querySelector(".out-source").textContent, "flowchart LR\n  A -->");
  assert.equal(plate().querySelector(".out-status").textContent, "Writing");
  await click(plate().querySelector(".out-stage"));
  assert.equal(shown.length, 0, "a half-written output does not open");

  await reply("flowchart LR\n  A --&gt; B", true);
  assert.equal(plate().querySelector(".out-diagram svg").dataset.lines, "2");
  await reply("flowchart LR\n  A --&gt; B\n  B --&gt;", true);
  assert.equal(plate().querySelector(".out-diagram svg").dataset.lines, "2", "the last good drawing stays");
  assert.equal(plate().querySelector(".out-tail").textContent, "B -->");
  assert.equal(plate().querySelector(".out-problem"), null, "no error while it is still being written");

  await reply("---\ntitle: Read path\n---\nflowchart LR\n  A --&gt; B\n  B --&gt; C", false);
  assert.equal(plate().querySelector(".out-status"), null);
  await click(plate().querySelector("button.out-name"));
  assert.deepEqual(shown.map(({ anchor, kind, source, path, screen, name }) => ({ anchor, kind, source, path, screen, name })), [
    {
      anchor: "live-0:1",
      kind: "diagram",
      source: "---\ntitle: Read path\n---\nflowchart LR\n  A --> B\n  B --> C",
      path: undefined,
      screen: undefined,
      name: "Read path",
    },
  ]);
  assert.equal(shown[0].sourceIndex, 0);
  assert.equal(shown[0].sourceEvent, undefined, "unfinished live output has no invented durable identity");

  await reply("flowchart LR\n  A --&gt;", false);
  assert.match(plate().querySelector(".out-problem").textContent, /couldn't be drawn.*unfinished/);
  assert.equal(plate().querySelector("button.out-name"), null);
});

test("a diagram grows while text keeps arriving, without waiting for a pause", async () => {
  const { Markdown } = await import(await moduleUrl("Markdown"));
  const { OutputsProvider } = await import(await moduleUrl("outputContext"));
  const outputs = { open: null, all: [], viewing: false, stopped: [], online: [] };
  let body = "flowchart LR";
  const seen = [];
  // A new piece every 30 ms for a second: faster than any pause the drawing could wait for.
  for (let piece = 0; piece < 34; piece++) {
    body += piece % 3 === 0 ? `\n  N${piece} --&gt; ` : `M${piece}`;
    await act(() =>
      root.render(
        h(
          OutputsProvider,
          { value: outputs },
          h(Markdown, { html: `<pre data-lang="mermaid"><code>${body}</code></pre>`, scope: "live-0", streaming: true }),
        ),
      ),
    );
    await act(() => new Promise((resolve) => setTimeout(resolve, 30)));
    seen.push(Number(document.querySelector(".out-diagram svg")?.dataset.lines ?? 0));
  }
  assert.ok(seen[12] > 0, "it is drawn well before the text stops");
  assert.ok(new Set(seen).size > 3, `it keeps growing as lines arrive: ${seen}`);
  assert.equal(document.querySelector(".out-status").textContent, "Writing");
});

test("a deck saved as PowerPoint reads back as the same slides, and nothing in a file is run", async () => {
  globalThis.DOMParser = dom.window.DOMParser;
  const { writeDeck } = await import(await moduleUrl("outputDeckWrite"));
  const { readDeck } = await import(await moduleUrl("outputDeckRead"));
  const hostile = '<img src=x onerror=alert(1)><script>alert(2)</script>';
  const deck = {
    head: "",
    body: "",
    slides: [
      "<section><h1>Reads no longer wait</h1><p>One writer &amp; four readers.</p></section>",
      `<section><h2>Checked</h2><ul><li>42 tests</li><li>${hostile.replaceAll("<", "&lt;")}</li></ul></section>`,
    ],
  };
  const read = await readDeck(await writeDeck(deck, "Reads"));
  const said = read.slides.map((slide) => slide.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim());
  assert.deepEqual(said, ["Reads no longer wait One writer &amp; four readers.", `Checked 42 tests ${hostile.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;")}`]);
  for (const slide of read.slides) assert.doesNotMatch(slide, /<script|<img src=x|onerror=alert\(1\)>/);
  await assert.rejects(readDeck(new TextEncoder().encode("not a zip")));

  // A chart is drawn from its stored numbers; a label is only ever text.
  const { chartMarkup } = await import(await moduleUrl("outputDeckChart"));
  const point = (value, at = 0) => `<c:pt idx="${at}"><c:v>${value}</c:v></c:pt>`;
  const chart = (kind) =>
    new DOMParser().parseFromString(
      `<c:chartSpace xmlns:c="urn:c"><c:plotArea><c:${kind}><c:ser><c:tx>${point("Before")}</c:tx>` +
        `<c:cat>${point("&lt;script&gt;alert(1)&lt;/script&gt;")}${point("8 writers", 1)}</c:cat>` +
        `<c:val>${point(41)}${point(182, 1)}</c:val></c:ser></c:${kind}></c:plotArea></c:chartSpace>`,
      "application/xml",
    );
  const bars = chartMarkup(chart("barChart"), ["#123456"]);
  assert.equal(bars.match(/<rect [^>]*rx="3"/g).length, 2, "one bar for each number");
  assert.match(bars, /&lt;script&gt;alert\(1\)/);
  assert.doesNotMatch(bars, /<script/);
  assert.match(chartMarkup(chart("pieChart"), ["#123456", "#abcdef"]), /<path d="M/);
  assert.equal(chartMarkup(chart("radarChart"), []), undefined, "a kind it cannot draw is not drawn wrong");
});

test("a screen is told nothing until it is ready, and gets only what the person allows", async () => {
  const { bridge } = await import(await moduleUrl("outputBridge"));
  const sent = [];
  const asked = [];
  let allow = false;
  const gate = (what) => (asked.push(what), allow ? Promise.resolve() : Promise.reject(new Error("The person did not allow this.")));
  const link = bridge(
    {
      context: () => ({ theme: "dark", displayMode: "fullscreen" }),
      callTool: async (name, args) => (await gate(`call ${name}`), { content: [{ type: "text", text: JSON.stringify(args) }] }),
      openLink: (url) => gate(`open ${url}`),
      say: (text) => gate(`say ${text}`),
      readPage: async (uri) => ({ uri }),
    },
    (message) => sent.push(message),
    5,
  );
  link.feed({ input: { elements: 3 }, result: { content: [], structuredContent: { ok: true } } });
  const settle = (after = 0) => new Promise((resolve) => setTimeout(resolve, after));
  const rpc = (id, method, params) => link.receive({ jsonrpc: "2.0", id, method, params });

  // Not JSON-RPC, or not yet ready: nothing is sent back unprompted.
  for (const junk of [null, "hi", { method: "tools/call" }, { jsonrpc: "2.0" }]) link.receive(junk);
  link.changed({ theme: "light" });
  assert.deepEqual(sent, []);

  rpc(1, "ui/initialize", { protocolVersion: "2026-01-26" });
  await settle();
  assert.equal(sent[0].result.hostContext.theme, "dark");
  assert.equal(sent.length, 1, "the result waits for the screen to say it is ready");
  link.receive({ jsonrpc: "2.0", method: "ui/notifications/initialized" });
  link.receive({ jsonrpc: "2.0", method: "ui/notifications/initialized" });
  // A page that has only just said so is not listening yet; what is sent now would be lost.
  assert.equal(sent.length, 1, "the drawing is not sent in the same moment the screen says it is ready");
  await settle(20);
  assert.deepEqual(sent.slice(1).map((message) => message.method), [
    "ui/notifications/tool-input",
    "ui/notifications/tool-result",
  ]);
  assert.deepEqual(sent[1].params, { arguments: { elements: 3 } });

  // Refused by the person: the screen is told so, and nothing happened.
  rpc(2, "tools/call", { name: "save_scene", arguments: { id: 7 } });
  rpc(3, "ui/open-link", { url: "javascript:alert(1)" });
  rpc(4, "ui/open-link", { url: "https://excalidraw.com/#room" });
  rpc(5, "sampling/createMessage", {});
  await settle();
  const reply = (id) => sent.find((message) => message.id === id);
  assert.match(reply(2).error.message, /did not allow/);
  assert.match(reply(3).error.message, /Only a web link/);
  assert.ok(reply(4).error && reply(5).error.code === -32601);
  assert.deepEqual(asked, ["call save_scene", "open https://excalidraw.com/#room"], "a bad link never reaches the person");

  allow = true;
  rpc(6, "tools/call", { name: "save_scene", arguments: { id: 7 } });
  rpc(7, "ui/message", { role: "user", content: { type: "text", text: "Make the pool blue" } });
  await settle();
  assert.equal(reply(6).result.content[0].text, '{"id":7}');
  assert.deepEqual(reply(7).result, {});
  assert.equal(asked.at(-1), "say Make the pool blue");
});

test("a list written where the tool takes text is drawn live as the finished call will run", async () => {
  const { asItWillRun, halfWritten } = await import(await moduleUrl("outputBridge"));
  // One model writes the elements as JSON text, another as the list itself.
  const asText = halfWritten('{"elements":"[{\\"type\\":\\"rect\\"},{\\"type\\":\\"te');
  const asList = halfWritten('{"elements":[{"type":"rect"},{"type":"te');
  assert.deepEqual(asItWillRun(asList, ["elements"]), { elements: '[{"type":"rect"},{"type":"te"}]' });
  assert.equal(asItWillRun(asText, ["elements"]), asText, "text is passed on untouched");
  // Only fields the schema reported as text change; nothing without a report does.
  const both = { elements: [1], points: [2] };
  assert.deepEqual(asItWillRun(both, ["elements"]), { elements: "[1]", points: [2] });
  assert.equal(asItWillRun(both, undefined), both);
  assert.equal(asItWillRun(both, "elements"), both);
  assert.equal(asItWillRun(undefined, ["elements"]), undefined);
});

test("a screen draws a call as the model writes it, and is told everything again after a reload", async () => {
  const { bridge, halfWritten } = await import(await moduleUrl("outputBridge"));
  // Arguments cut off anywhere still read as the part that is whole.
  const whole = '{"elements":"[{\\"type\\":\\"rect\\",\\"x\\":10}]","title":"Read \\u0041 path","n":[1,2.5,{"a":null}]}';
  for (let cut = 1; cut <= whole.length; cut++) {
    const read = halfWritten(whole.slice(0, cut));
    assert.ok(read === undefined || typeof read === "object", `cut at ${cut}: ${whole.slice(0, cut)}`);
  }
  assert.deepEqual(halfWritten(whole), JSON.parse(whole));
  assert.deepEqual(halfWritten('{"elements":"[{\\"type\\":\\"re'), { elements: '[{"type":"re' });
  assert.deepEqual(halfWritten('{"a":[1,2,{"b":"x"},{"c":'), { a: [1, 2, { b: "x" }] });
  assert.deepEqual(halfWritten('{"a":1,"b'), { a: 1 });
  assert.deepEqual(halfWritten('{"a":"tail\\'), { a: "tail" });
  assert.equal(halfWritten(""), undefined);

  const sent = [];
  const link = bridge({ context: () => ({}) }, (message) => sent.push(message), 5);
  const said = () => sent.filter((message) => message.method).map((message) => [message.method.split("/").at(-1), message.params]);
  const start = async () => {
    link.receive({ jsonrpc: "2.0", id: 1, method: "ui/initialize", params: {} });
    await new Promise((resolve) => setTimeout(resolve, 0));
    link.receive({ jsonrpc: "2.0", method: "ui/notifications/initialized" });
    await new Promise((resolve) => setTimeout(resolve, 20));
  };

  link.feed({ partial: { boxes: ["Writer"] } });
  assert.deepEqual(said(), [], "nothing is said before the screen is ready");
  await start();
  link.feed({ partial: { boxes: ["Writer", "WAL"] } });
  link.feed({ partial: { boxes: ["Writer", "WAL"] }, input: { boxes: ["Writer", "WAL", "SQLite"] } });
  link.feed({ partial: { boxes: ["late"] }, input: { boxes: ["Writer", "WAL", "SQLite"] } });
  link.feed({ input: { boxes: ["Writer", "WAL", "SQLite"] }, result: { content: [] } });
  assert.deepEqual(said(), [
    ["tool-input-partial", { arguments: { boxes: ["Writer"] } }],
    ["tool-input-partial", { arguments: { boxes: ["Writer", "WAL"] } }],
    ["tool-input", { arguments: { boxes: ["Writer", "WAL", "SQLite"] } }],
    ["tool-result", { content: [] }],
  ]);

  // The frame was unloaded and came back: the new page is given the finished call and its result.
  sent.length = 0;
  await start();
  assert.deepEqual(said(), [
    ["tool-input", { arguments: { boxes: ["Writer", "WAL", "SQLite"] } }],
    ["tool-result", { content: [] }],
  ]);
});

test("a revision takes the viewer with it, but never away from a version the person chose", async () => {
  const { Markdown } = await import(await moduleUrl("Markdown"));
  const { OutputScope } = await import(await moduleUrl("OutputScope"));
  const { useOutputs } = await import(await moduleUrl("outputContext"));
  let outputs;
  let writing = -1;
  let free = true;
  let shown = [];
  let stamp = 0;
  const Probe = () => ((outputs = useOutputs()), null);
  const version = (body) =>
    `<pre data-lang="mermaid"><code>---\ntitle: Read path\n---\nflowchart LR\n  A --&gt; ${body}</code></pre>`;
  const other = '<pre data-lang="mermaid"><code>flowchart LR\n  X --&gt; Y</code></pre>';
  // Which replies are versions of one output is the saved catalogue's word alone.
  globalThis.__medhaControlsTest.api = {
    artifacts: async () => ({
      revisions: shown.flatMap((html, index) => index === writing ? [] : [{
        artifact_id: html === other ? `separate-${index}` : "read-path", revision_id: `r${index}:code:0`, predecessor: null,
        source_event: `r${index}`, source_index: 0, ts: index + 1, kind: "diagram", name: "Read path", inline: true,
      }]),
      next_cursor: null, omitted: 0,
    }),
  };
  const chat = async (...replies) => {
    shown = replies;
    await act(() =>
      root.render(
        h(
          OutputScope,
          { chat: "c", sessionId: "saved", stamp: String(++stamp), viewing: true, onShow() {}, onMade: () => free, onAsk() {} },
          h(Probe),
          replies.map((html, index) =>
            h(Markdown, { key: index, html, scope: `r${index}`, at: index, streaming: index === writing }),
          ),
        ),
      ),
    );
    await act(() => new Promise((resolve) => setTimeout(resolve, 150)));
  };

  await chat(version("B"));
  await act(() => outputs.show(outputs.all[0]));
  assert.equal(outputs.open.anchor, "r0:code:0");

  await chat(version("B"), version("C"));
  assert.equal(outputs.open.anchor, "r1:code:0", "the viewer follows the revision");
  assert.equal(document.querySelectorAll(".out-plate").length, 1, "the first version keeps its plate");
  assert.match(document.querySelector(".out-revision.current").textContent, /Read path.*v2/);

  await act(() => outputs.follow(outputs.all[0]));
  await chat(version("B"), version("C"), version("D"));
  assert.equal(outputs.open.anchor, "r0:code:0", "a chosen older version stays put");
  assert.deepEqual(outputs.all.map((output) => output.anchor), ["r0:code:0", "r1:code:0", "r2:code:0"]);

  await chat(version("B"));
  assert.deepEqual(outputs.all.map((output) => output.anchor), ["r0:code:0"], "a reply that leaves takes its outputs");

  // What Medha finishes writing opens by itself, unless the person is busy elsewhere.
  await act(() => outputs.browse());
  writing = 1;
  await chat(version("B"), other);
  assert.equal(outputs.open, null, "nothing opens while it is still being written");
  writing = -1;
  await chat(version("B"), other);
  assert.equal(outputs.open.anchor, "r1:code:0");
  assert.equal(document.querySelectorAll(".out-plate").length, 2, "a reply the catalogue keeps apart is its own output");
  await act(() => outputs.browse());
  free = false;
  writing = 2;
  await chat(version("B"), other, other);
  writing = -1;
  await chat(version("B"), other, other);
  assert.equal(outputs.open, null, "it stays out of the way of another panel");
});

test("reading or mentioning an input file cannot publish it or open Preview", async () => {
  const { Markdown } = await import(await moduleUrl("Markdown"));
  const { OutputsProvider, SHOWN_ONLY } = await import(await moduleUrl("outputContext"));
  const published = [], opened = [];
  globalThis.__medhaControlsTest.api = { files() { throw new Error("file existence must not infer publication"); } };
  await act(() => root.render(h(OutputsProvider, { value: { ...SHOWN_ONLY, enter: (...args) => { published.push(args); return () => {}; }, made: (output) => opened.push(output) } }, h(Markdown, { html: '<p>I read <code>existing.html</code> and <a href="other.html">the other file</a>.</p><p><span class="md-image" data-src="photo.png">Photo</span></p>', scope: "input", fresh: true }))));
  assert.equal(document.querySelectorAll(".out-plate").length, 0);
  assert.equal(document.querySelectorAll(".prose a").length, 3, "references retain explicit preview actions");
  assert.deepEqual([published, opened], [[], []]);
});

test("a drawing tool is recognised for live drawing even when another call starts before the screen list arrives", async () => {
  const { useOffered, forgetScreens } = await import(await moduleUrl("OutputApp"));
  const tool = "mcp__excalidraw__create_view";
  const screens = { tools: [{ name: tool, server: "excalidraw", tool: "create_view", resource: "ui://excalidraw/view" }] };
  const answers = [];
  globalThis.__medhaControlsTest.api = { liveCall: () => new Promise((done) => answers.push(done)) };
  let offered;
  const Probe = ({ writing }) => ((offered = useOffered(writing)), null);
  const draw = (count) => act(() => root.render(h(Probe, { writing: Array.from({ length: count }, () => ({ tool })) })));

  globalThis.__medhaControlsTest.scope = { current: { chatKey: "racing" } };
  await draw(1);
  await draw(2);
  assert.equal(answers.length, 1, "the tool is asked about once");
  await act(async () => answers[0](screens));
  assert.ok(offered(tool), "the answer that arrived late still switches live drawing on");

  // An answer for a chat that ended meanwhile does not bring its cache back.
  globalThis.__medhaControlsTest.scope = { current: { chatKey: "ended" } };
  await draw(1);
  forgetScreens("ended");
  await act(async () => answers[1](screens));
  assert.equal(offered(tool), undefined);
  delete globalThis.__medhaControlsTest.scope;
});

test("failed MCP screens remain compact and do not replace a good revision", async () => {
  const { Screens } = await import(await moduleUrl("OutputPlate"));
  const { OutputsProvider, SHOWN_ONLY } = await import(await moduleUrl("outputContext"));
  const entered = [], made = [], inspected = [];
  const screen = { server: "drawing", tool: "create_view", resource: "ui://drawing", input: {}, result: { isError: true } };
  await act(() => root.render(h(OutputsProvider, { value: { ...SHOWN_ONLY, enter: (output) => { entered.push(output); return () => {}; }, made: (output) => made.push(output), show: (output) => inspected.push(output) } }, h(Screens, { at: 0, fresh: true, steps: [{ id: "bad", status: "failed", screen, summary: "Invalid drawing" }, { id: "denied", status: "denied", screen, summary: "Denied" }] }))));
  assert.equal(document.querySelectorAll(".out-failed").length, 2);
  assert.equal(document.querySelectorAll(".out-plate").length, 0);
  assert.deepEqual([entered, made], [[], []]);
  await click([...document.querySelectorAll("button")].find((button) => button.textContent === "Inspect error screen"));
  assert.equal(inspected[0].anchor, "error-screen:bad");
});

test("saved artifact identities and all revisions survive catalogue reloads", async () => {
  const { OutputScope } = await import(await moduleUrl("OutputScope"));
  const { useOutputs } = await import(await moduleUrl("outputContext"));
  const records = [1, 2].map((n) => ({ artifact_id: "inline:flow:diagram", revision_id: `event-${n}:code:0`, predecessor: n > 1 ? `event-${n - 1}:code:0` : null, source_event: `event-${n}`, source_index: 0, ts: n, kind: "diagram", name: "Same title", inline: true, source: `flowchart LR\n A-->Node${n}` }));
  globalThis.__medhaControlsTest.api = { artifacts: async () => ({ revisions: records, next_cursor: null, omitted: 0 }) };
  let outputs;
  const Probe = () => ((outputs = useOutputs()), null);
  const mount = async (key) => { await act(() => root.render(h(OutputScope, { key, chat: "live", sessionId: "saved", viewing: true, onShow() {}, onMade: () => true, onAsk() {} }, h(Probe)))); await act(() => new Promise((done) => setTimeout(done, 80))); };
  await mount("first");
  assert.equal(outputs.all.length, 2);
  await act(() => outputs.show(outputs.all[0]));
  assert.equal(outputs.open.source, records[0].source);
  await mount("reloaded");
  assert.deepEqual(outputs.all.map((output) => output.anchor), records.map((record) => record.revision_id));
  assert.deepEqual(outputs.all.map((output) => output.predecessor), [null, "event-1:code:0"]);
});

test("identical live outputs bind by source event and kind instead of titles, content or call IDs", async () => {
  const { OutputScope } = await import(await moduleUrl("OutputScope"));
  const { useOutputs } = await import(await moduleUrl("outputContext"));
  const source = "flowchart LR\n A-->B";
  const records = ["first", "second"].map((id, n) => ({ artifact_id: `independent-${id}`, revision_id: `${id}:code:0`, predecessor: null, source_event: id, source_index: 0, ts: n + 1, kind: "diagram", name: "Same title", inline: true, source }));
  records.push({ ...records[1], kind: "app", artifact_id: "drawing", revision_id: "second:screen", inline: false, source: undefined, tool_call: "reused", screen: { server: "draw", tool: "view", resource: "ui://draw" } });
  globalThis.__medhaControlsTest.api = { artifacts: async () => ({ revisions: records, next_cursor: null, omitted: 0 }) };
  let outputs;
  const Probe = () => ((outputs = useOutputs()), null);
  await act(() => root.render(h(OutputScope, { chat: "c", sessionId: "saved", viewing: true, onShow() {}, onMade: () => true, onAsk() {} }, h(Probe))));
  await act(() => new Promise(done => setTimeout(done, 80)));
  const live = id => ({ anchor: `live-${id}:0`, kind: "diagram", name: "Same title", source, sourceEvent: id, sourceIndex: 0 });
  assert.equal(outputs.resolve(live("first")).revisionId, "first:code:0");
  assert.equal(outputs.resolve(live("second")).revisionId, "second:code:0");
  assert.equal(outputs.resolve({ ...live("second"), kind: "app", toolCall: "reused" }).revisionId, "second:screen");
  assert.equal(outputs.resolve({ ...live("unknown"), toolCall: "reused" }).revisionId, undefined);
  assert.equal(outputs.resolve({ ...live("unknown"), sourceEvent: undefined, toolCall: "reused" }).revisionId, undefined);
  assert.equal(outputs.all.length, 3);
});

test("oversized inline source is copied from its saved revision and action failures stay visible", async () => {
  const { Preview } = await import(await moduleUrl("OutputPreview"));
  const { OutputsProvider, SHOWN_ONLY } = await import(await moduleUrl("outputContext"));
  const source = "flowchart LR\n A-->B\n";
  const copied = [], reads = [], released = [], saves = [];
  Object.defineProperty(globalThis.navigator, "clipboard", { configurable: true, value: { writeText: async (text) => copied.push(text) } });
  const open = { anchor: "revision", revisionId: "revision", sessionId: "chat", kind: "diagram", name: "Diagram", source: "", storedSource: true };
  globalThis.__medhaControlsTest.api = {
    artifactPreview: async (...args) => { reads.push(args); return { text: source, size: source.length, bundle: "owned" }; },
    artifactRelease: async (bundle) => released.push(bundle),
    saveOutput: async (...args) => { saves.push(args); throw new Error("Disk is full"); },
  };
  await act(() => root.render(h(OutputsProvider, { value: { ...SHOWN_ONLY, open, all: [open] } }, h(Preview))));
  await click(document.querySelector('[aria-label="Copy"]'));
  assert.deepEqual(copied, [source]);
  assert.ok(reads.every((args) => args[0] === "chat" && args[1] === "revision"));
  assert.ok(released.includes("owned"));
  await click(document.querySelector('[aria-label="Save a copy"]'));
  assert.match(document.querySelector('[role="alert"]').textContent, /Disk is full.*Try the action again/);
  assert.deepEqual(saves[0][1], { revisionId: "revision", sessionId: "chat" });
  globalThis.__medhaControlsTest.api.saveOutput = async () => false;
  await click(document.querySelector('[aria-label="Save a copy"]'));
  assert.equal(document.querySelector('[role="alert"]'), null, "cancelling the file dialog is not an error");
});

test("a saved screen calls back only the connection that drew it", async () => {
  const { AppScreen } = await import(await moduleUrl("OutputApp"));
  const { OutputsProvider, SHOWN_ONLY } = await import(await moduleUrl("outputContext"));
  const run = async (chat, recorded, live) => {
    const calls = [];
    const output = { anchor: chat, artifactId: chat, revisionId: chat, name: "Drawing", kind: "app", source: "", screen: { server: "draw", tool: "create_view", resource: "ui://draw/app.html", identity: recorded, input: { elements: "[]" } } };
    globalThis.__medhaControlsTest.scope = { current: { chatKey: chat } };
    globalThis.__medhaControlsTest.api = {
      liveCall: async (_chat, method, params) => { calls.push([method, params]); return method === "mcp.screen" ? { html: "<html>Screen</html>", connect: [], resources: [], frames: [], identity: live } : { content: [] }; },
      screenKeep: async () => {}, screenPut: async () => `medha://localhost/screen/${chat}`, screenDrop: async () => {},
    };
    await act(() => root.render(h(OutputsProvider, { value: { ...SHOWN_ONLY, all: [output] } }, h(AppScreen, { output, size: "full" }))));
    const frame = document.querySelector("iframe");
    const replies = [];
    frame.contentWindow.postMessage = (value) => replies.push(value);
    await act(() => window.dispatchEvent(new dom.window.MessageEvent("message", { source: frame.contentWindow, data: { jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "save_checkpoint", arguments: {} } } })));
    const allow = [...document.querySelectorAll("button")].find((button) => button.textContent.includes("Allow once"));
    if (allow) await click(allow);
    await act(async () => {});
    return { reply: replies.find((reply) => reply.id === 1), sent: calls.filter(([method]) => method === "mcp.screen.call") };
  };
  const moved = await run("screen-moved", "drawn-by", "replaced");
  assert.match(moved.reply.error.message, /changed since this was made/);
  assert.equal(moved.sent.length, 0, "a screen never reaches a server that replaced the one it was drawn by");
  const same = await run("screen-same", "drawn-by", "drawn-by");
  assert.equal(same.sent[0][1].identity, "drawn-by", "the backend checks the recorded connection, not the page's word");
  const older = await run("screen-older", undefined, "current");
  assert.equal(older.sent[0][1].identity, "current", "a screen recorded before identities calls its server as it always did");
});

test("PowerPoint previews retain missing-slide positions and disclose unsupported or truncated content", async () => {
  globalThis.DOMParser = dom.window.DOMParser;
  const { readDeck } = await import(await moduleUrl("outputDeckRead"));
  const make = async (count, content) => {
    const zip = new JSZip();
    zip.file("ppt/presentation.xml", `<p:presentation xmlns:p="urn:p" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst>${Array.from({length:count}, (_,n)=>`<p:sldId id="${n+1}" r:id="r${n+1}"/>`).join("")}</p:sldIdLst></p:presentation>`);
    zip.file("ppt/_rels/presentation.xml.rels", `<Relationships>${Array.from({length:count},(_,n)=>`<Relationship Id="r${n+1}" Target="slides/slide${n+1}.xml"/>`).join("")}</Relationships>`);
    for (let n=0; n<count; n++) if (content(n) !== undefined) zip.file(`ppt/slides/slide${n+1}.xml`, content(n));
    return zip.generateAsync({type:"uint8array", compression:"DEFLATE"});
  };
  const grouped = '<p:sld xmlns:p="urn:p"><p:cSld><p:spTree><p:grpSp/></p:spTree></p:cSld></p:sld>';
  const deck = await readDeck(await make(201, (n)=>n===0 ? grouped : undefined));
  assert.equal(deck.total, 201);
  assert.equal(deck.slides.length, 200);
  assert.match(deck.slides[1], /Slide 2 could not be previewed/);
  assert.match(deck.notices.join(" "), /Simplified PowerPoint.*Grouped shapes.*201 slides|Simplified PowerPoint.*201 slides.*Grouped shapes/);
  const padded = `<p:sld xmlns:p="urn:p"><!--${"x".repeat(3 * 1024 * 1024)}--><p:cSld><p:spTree/></p:cSld></p:sld>`;
  const bounded = await readDeck(await make(12, ()=>padded));
  assert.equal(bounded.slides.length, 12);
  assert.match(bounded.slides.at(-1), /Slide 12 could not be previewed/);
  assert.match(bounded.notices.join(" "), /32 MiB total decompression limit/);
});

test("the screen bridge bounds pending work and drops stale replies when closed", async () => {
  const { bridge } = await import(await moduleUrl("outputBridge"));
  const sent = []; let done;
  const waiting = new Promise((resolve) => { done = resolve; });
  const link = bridge({ context: () => ({}), callTool: () => waiting }, (message) => sent.push(message));
  for (let id = 0; id < 40; id++) link.receive({ jsonrpc: "2.0", id, method: "tools/call", params: { name: "wait", arguments: {} } });
  assert.equal(sent.filter((message) => message.error?.message.includes("too many")).length, 8);
  link.close();
  done({ ok: true });
  await act(async () => {});
  assert.equal(sent.some((message) => message.result), false);
});

test("a rejected follow-up does not mark an active agent idle", async () => {
  let receive,
    live,
    sends = 0;
  globalThis.__medhaControlsTest.api = {
    onLive: async (callback) => {
      receive = callback;
      return () => {};
    },
    liveOpen: async () => {},
    liveCall: async (_key, method) => {
      if (method === "session.settings") return settings;
      if (++sends === 2) throw new Error("follow-up rejected");
      return { accepted: true };
    },
  };
  function Harness() {
    live = useLive();
    return null;
  }
  await act(() => root.render(h(Harness)));
  await act(() => live.send("chat", null, "Original"));
  await act(() =>
    receive("chat", {
      method: "event",
      params: { kind: "tool.call", id: "active-tool", tool: "read" },
    }),
  );
  await act(() =>
    assert.rejects(live.send("chat", null, "Follow-up"), /follow-up rejected/),
  );
  assert.equal(live.live.chat.status, "running");
  assert.equal(live.live.chat.items.at(-1).steps[0].status, "running");
  await act(() =>
    receive("chat", { method: "event", params: { kind: "turn.done" } }),
  );
  assert.equal(live.live.chat.status, "idle");
});

test("large-code worker deduplicates requests, cancels stale work and cannot replace newer code", async () => {
  const previous = globalThis.Worker;
  let worker;
  class FakeWorker {
    listeners = new Map();
    sent = [];
    constructor() {
      worker = this;
    }
    addEventListener(name, callback) {
      this.listeners.set(name, callback);
    }
    postMessage(data) {
      this.sent.push(data);
    }
    terminate() {}
    complete(html) {
      const job = this.sent.at(-1);
      this.listeners.get("message")({ data: { id: job.id, html } });
    }
  }
  globalThis.Worker = FakeWorker;
  try {
    const { highlightAsync } = await import(await moduleUrl("syntax"));
    const first = new AbortController(),
      shared = new AbortController(),
      stale = new AbortController(),
      latest = new AbortController();
    const a = highlightAsync("worker-request-a", "python", first.signal);
    const a2 = highlightAsync("worker-request-a", "python", shared.signal);
    const b = highlightAsync("worker-request-b", "python", stale.signal);
    const c = highlightAsync("worker-request-c", "python", latest.signal);
    assert.equal(worker.sent.length, 1);
    first.abort();
    stale.abort();
    worker.complete("<span>A</span>");
    assert.equal(await a, undefined);
    assert.equal(await b, undefined);
    assert.equal(await a2, "<span>A</span>");
    assert.equal(worker.sent.length, 2);
    assert.equal(worker.sent.at(-1).text, "worker-request-c");
    worker.complete("<span>C</span>");
    assert.equal(await c, "<span>C</span>");
    assert.equal(
      await highlightAsync(
        "worker-request-c",
        "python",
        new AbortController().signal,
      ),
      "<span>C</span>",
    );
    assert.equal(worker.sent.length, 2);
    const { Markdown, SourceCode } = await import(await moduleUrl("Markdown"));
    const oldCode = "print(111)\n".repeat(1000),
      newCode = "print(222)\n".repeat(1000);
    const render = (text) =>
      root.render(
        h(
          "div",
          null,
          h(Markdown, {
            html: `<pre data-lang="python"><code>${text}</code></pre>`,
          }),
          h(SourceCode, { text, language: "python" }),
        ),
      );
    await act(() => render(oldCode));
    assert.equal(worker.sent.length, 3, "both views share one highlight job");
    await act(() => render(newCode));
    assert.equal(worker.sent.length, 3, "only one job runs inside the worker");
    await act(async () => {
      worker.complete('<span class="hljs-number">111</span>');
    });
    assert.equal(
      document.querySelector(".source-code code").textContent,
      newCode,
    );
    assert.equal(document.querySelector(".md code").textContent, newCode);
    assert.equal(worker.sent.at(-1).text, newCode);
    await act(async () => {
      worker.complete('<span class="hljs-number">222</span>');
    });
    assert.equal(
      document.querySelector(".source-code .hljs-number").textContent,
      "222",
    );
    assert.equal(document.querySelector(".md .hljs-number").textContent, "222");
  } finally {
    globalThis.Worker = previous;
  }
});

test("a failed syntax worker releases requests and leaves readable plain text", async () => {
  const previous = globalThis.Worker;
  let worker,
    terminated = false;
  globalThis.Worker = class {
    listeners = new Map();
    constructor() {
      worker = this;
    }
    addEventListener(name, callback) {
      this.listeners.set(name, callback);
    }
    postMessage() {}
    terminate() {
      terminated = true;
    }
  };
  try {
    const { highlightAsync } = await import(
      `${await moduleUrl("syntax")}#failure-case`
    );
    const a = highlightAsync("a", undefined, new AbortController().signal);
    const b = highlightAsync("b", undefined, new AbortController().signal);
    worker.listeners.get("error")();
    assert.equal(await a, undefined);
    assert.equal(await b, undefined);
    assert.equal(terminated, true);
    assert.equal(
      await highlightAsync("later", undefined, new AbortController().signal),
      undefined,
    );
  } finally {
    globalThis.Worker = previous;
  }
});

const { Usage } = await import(await moduleUrl("Usage"));
const { UsageChart } = await import(await moduleUrl("UsageChart"));
const { McpForm } = await import(await moduleUrl("McpServers"));
const { McpCatalog } = await import(await moduleUrl("McpCatalog"));
const tally = (calls, prompt, cost = 0.09) => ({
  calls, prompt_tokens: prompt, completion_tokens: 0, cached_tokens: 0,
  cost_usd: cost, unpriced_calls: cost === null ? calls : 0,
});
function usageSummary(breakdown = true) {
  const now = new Date();
  const day = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}-${String(now.getDate()).padStart(2, "0")}`;
  const total = tally(47, 1200);
  const models = [{ model: "alpha", usage: tally(40, 1000, 0.08) }, { model: "beta", usage: tally(7, 200, 0.01) }];
  return {
    total, models,
    by_day: [{ day, usage: total, ...(breakdown ? { models } : {}) }],
    sessions: [{ id: "chat", title: "Recorded chat", agents: 1, usage: total, ...(breakdown ? { models } : {}) }],
  };
}

test("legacy usage still draws daily totals and chats without per-model fields", async () => {
  globalThis.__medhaControlsTest.api.usage = async () => usageSummary(false);
  await act(async () => root.render(h(Usage)));
  assert.ok([...document.querySelectorAll(".usage-stack")].some((bar) => parseFloat(bar.style.height) > 0));
  const daily = document.querySelector(".usage-daily tbody");
  assert.equal(daily.children.length, 1);
  assert.match(daily.textContent, /1.2k/);
  assert.match(daily.textContent, /\$0.09/);
  assert.match(document.querySelector(".usage-sessions").textContent, /Recorded chat/);
  assert.equal(document.querySelector('[aria-label="Model"]').disabled, true);
  const ticks = [...document.querySelectorAll(".usage-y span")].map((node) => node.textContent);
  assert.equal(new Set(ticks).size, 3);
});

test("model filtering keeps chart, daily numbers and chats in agreement", async () => {
  globalThis.__medhaControlsTest.api.usage = async () => usageSummary();
  await act(async () => root.render(h(Usage)));
  await click(document.querySelector('[aria-label="Model"]'));
  await click(option("beta"));
  assert.match(document.querySelector(".usage-totals").textContent, /Model calls7/);
  assert.match(document.querySelector(".usage-daily tbody").textContent, /200\$0.01/);
  assert.match(document.querySelector(".usage-sessions").textContent, /200 tokens · \$0.01/);
  assert.equal(document.querySelectorAll(".usage-stack i").length, 1);
});

test("zero and single-token charts never print duplicate rounded ticks", async () => {
  for (const count of [0, 1]) {
    await act(async () => root.render(h(UsageChart, {
      days: [{ day: "2026-09-29", values: { a: count }, cost: "$0.00" }],
      series: [{ key: "a", label: "a", slot: 0 }], format: (n) => String(Math.round(n)),
    })));
    const ticks = [...document.querySelectorAll(".usage-y span")].map((node) => node.textContent).filter(Boolean);
    assert.equal(new Set(ticks).size, ticks.length);
    assert.deepEqual(ticks, count ? ["2", "1", "0"] : ["0"]);
  }
});

test("Unity setup requires its folder and preserves spaces as one argument", async () => {
  const saved = [];
  globalThis.__medhaControlsTest.api.settings = async () => ({ keychain: false });
  await act(async () => root.render(h(McpForm, {
    busy: false, onSave: async (args) => saved.push(args),
    initial: { id: "unity", transport: "local", command: ["uv", "--directory", "{unity_mcp_server_src}", "run", "server.py"],
      inputs: [{ name: "unity_mcp_server_src", description: "Unity server source folder" }] },
  })));
  const form = document.querySelector("form");
  await act(async () => form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  assert.equal(saved.length, 0);
  assert.match(document.querySelector('[role="alert"]').textContent, /Enter unity mcp server src/);
  const input = [...document.querySelectorAll("label")].find((node) => node.textContent.includes("unity mcp server src")).querySelector("input");
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
  await act(async () => {
    setter.call(input, "/Users/test/Library/Application Support/UnityMCP/src");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  assert.deepEqual(saved, [["unity", "--", "uv", "--directory", "/Users/test/Library/Application Support/UnityMCP/src", "run", "server.py"]]);
});

test("catalogue Show more appends distinct entries and forwards the next page", async () => {
  const calls = [];
  const row = (name) => ({ name, version: "1", setups: [] });
  globalThis.__medhaControlsTest.api.extensions = async (_method, params) => {
    calls.push(params);
    return params.cursor ? { servers: [row("one"), row("two")], next: null } : { servers: [row("one")], next: "2" };
  };
  await act(async () => root.render(h(McpCatalog, { onPick() {} })));
  await click(document.querySelector(".more"));
  assert.equal(calls[1].cursor, "2");
  assert.equal(calls[1].source, "featured");
  assert.equal(document.querySelectorAll(".catalog-list > li").length, 2);
  assert.equal(document.querySelector(".more"), null);
});

test("the / menu offers enabled skills and choosing one selects it for the next message", async () => {
  const picked = [];
  await act(async () =>
    root.render(
      h(Composer, {
        value: "", onChange() {}, onSend() {}, onStop() {}, running: false, disabled: false, placeholder: "Task",
        settings, settingsLocked: false, control: null, onControl() {}, onLoadSettings: async () => {},
        onConfigure: async () => {}, showReasoning: false, onShowReasoning() {}, attachments: [], onAttachments() {},
        onSurface() {}, onNew() {}, onRewind() {}, onSettings() {}, onExtensions() {},
        skills: [{ name: "frontend-design", description: "Distinctive UI" }, { name: "pdf", description: "PDF files" }],
        onSkill: (name) => picked.push(name),
      }),
    ),
  );
  const textarea = document.querySelector("textarea");
  const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value").set;
  await act(() => {
    setter.call(textarea, "/fro");
    textarea.dispatchEvent(new Event("input", { bubbles: true }));
  });
  const offered = [...document.querySelectorAll('[role="option"]')].map((node) => node.querySelector("b").textContent);
  assert.deepEqual(offered, ["/frontend-design"], "a partial name narrows to the matching skill");
  await click(document.querySelector('[role="option"]'));
  assert.deepEqual(picked, ["frontend-design"]);
});

test("a message sent with a skill leads with its token, and never shows the procedure", async () => {
  const { UserText } = await import(await moduleUrl("UserText"));
  const text =
    "Redesign the pricing page\n\n[Loaded skill: frontend-design] Follow this procedure for the current and related work:\n\n# Frontend Design\n\nSECRET-PROCEDURE-BODY";
  await act(async () => root.render(h(UserText, { text })));
  const message = document.querySelector(".you-text p");
  assert.equal(message.firstElementChild.className, "msg-token token-skill sent", "the token comes before the text");
  assert.equal(message.textContent, "frontend-designRedesign the pricing page");
  assert.doesNotMatch(document.body.textContent, /SECRET-PROCEDURE-BODY/);
});

test("the output a message is about rides in the composer as a token and leads the sent message", async () => {
  const { UserText } = await import(await moduleUrl("UserText"));
  let removed = 0;
  const props = (about) => ({
    value: "", onChange() {}, onSend() {}, onStop() {}, running: false, disabled: false, placeholder: "Task",
    settings, settingsLocked: false, control: null, onControl() {}, onLoadSettings: async () => {},
    onConfigure: async () => {}, showReasoning: false, onShowReasoning() {}, attachments: [], onAttachments() {},
    onSurface() {}, onNew() {}, onRewind() {}, onSettings() {}, onExtensions() {},
    about, onAbout: () => removed++,
  });
  globalThis.matchMedia = () => ({ matches: true });
  await act(async () => root.render(h(Composer, props("Diagram: Read path"))));
  const token = document.querySelector(".msg-token.token-output");
  assert.equal(token.textContent, "Diagram: Read path");
  await act(async () => token.querySelector("button").click());
  assert.equal(removed, 1, "removing it makes the message about something new");
  await act(async () => root.render(h(Composer, props(undefined))));
  assert.equal(document.querySelector(".msg-token"), null);

  await act(async () => root.render(h(UserText, { text: "make the tools blue", about: "Diagram: Read path" })));
  const message = document.querySelector(".you-text p");
  assert.equal(message.firstElementChild.className, "msg-token token-output sent");
  assert.equal(message.textContent, "Diagram: Read pathmake the tools blue", "only the person's words follow the token");
});

test("what is open decides what the next message is about, and a fix is sent as that output's next version", async () => {
  const { OutputScope } = await import(await moduleUrl("OutputScope"));
  const { useOutputs } = await import(await moduleUrl("outputContext"));
  const diagram = { artifact_id: "inline:medha-output-e1-0:diagram", revision_id: "e1:code:0", predecessor: null, source_event: "e1", source_index: 0, ts: 1, kind: "diagram", name: "Read path", inline: true, source: "---\ntitle: Read path\n---\nflowchart LR\n A-->B" };
  const file ={ artifact_id: "file:/tmp/report.html", revision_id: "e2:file", predecessor: null, source_event: "e2", source_index: 0, ts: 2, kind: "page", name: "report.html", inline: false, snapshot: { path: "/tmp/report.html", kind: "page", hash: "h", size: 1, assets: [], missing: [] } };
  globalThis.__medhaControlsTest.api = { artifacts: async () => ({ revisions: [diagram, file], next_cursor: null, omitted: 0 }) };
  const about = [], fixed = [];
  let outputs;
  const Probe = () => ((outputs = useOutputs()), null);
  const mount = async (viewing) => {
    await act(() => root.render(h(OutputScope, { chat: "c", sessionId: "saved", viewing, onShow() {}, onMade: () => true, onAsk() {}, onAbout: (next) => about.push(next), onFix: (text, reference) => fixed.push([text, reference]) }, h(Probe))));
    await act(() => new Promise((done) => setTimeout(done, 80)));
  };
  const reference = { artifact_id: diagram.artifact_id, revision_id: diagram.revision_id, label: "Diagram: Read path" };
  await mount(true);
  assert.equal(about.at(-1), undefined, "with nothing open, a message is about nothing");
  await act(() => outputs.show(outputs.all[0]));
  assert.deepEqual(about.at(-1), reference);
  await act(() => outputs.show(outputs.all[1]));
  assert.equal(about.at(-1), undefined, "a file is versioned by its path, never by a message");
  await act(() => outputs.show(outputs.all[0]));
  await mount(false);
  assert.equal(about.at(-1), undefined, "a closed panel is about nothing");

  await act(() => outputs.fix(outputs.all[0], "Please fix it."));
  await act(() => outputs.fix({ anchor: "live-1:0", kind: "diagram", name: "Diagram", source: "flowchart" }, "Please fix it."));
  assert.deepEqual(fixed, [["Please fix it.", reference], ["Please fix it.", undefined]], "one not saved yet is fixed without a reference");
});

test("Backspace at the start of the message marks the nearest token first, then removes it", async () => {
  let attachments = [{ id: "a1", name: "mock.png", mime: "image/png", data: "", preview: "data:," }];
  const removedSkills = [];
  const props = () => ({
    value: "", onChange() {}, onSend() {}, onStop() {}, running: false, disabled: false, placeholder: "Task",
    settings, settingsLocked: false, control: null, onControl() {}, onLoadSettings: async () => {},
    onConfigure: async () => {}, showReasoning: false, onShowReasoning() {}, attachments,
    onAttachments: (next) => {
      attachments = next;
    },
    onSurface() {}, onNew() {}, onRewind() {}, onSettings() {}, onExtensions() {},
    skill: "frontend-design", onSkill: (name) => removedSkills.push(name),
  });
  globalThis.matchMedia = () => ({ matches: true });
  await act(async () => root.render(h(Composer, props())));
  const textarea = document.querySelector("textarea");
  const labels = () => [...document.querySelectorAll(".msg-token")].map((node) => node.textContent);
  assert.deepEqual(labels(), ["frontend-design", "mock.png"], "the skill leads, then the image");

  await key(textarea, "Backspace");
  assert.ok(document.querySelector(".msg-token.armed").textContent.includes("mock.png"), "first press only marks");
  assert.equal(attachments.length, 1);
  assert.match(document.querySelector(".composer-announce").textContent, /Backspace again to remove mock.png/);

  await key(textarea, "Backspace");
  assert.deepEqual(attachments, [], "second press removes the image");
  await act(async () => root.render(h(Composer, props())));
  await key(textarea, "a");
  await key(textarea, "Backspace");
  await key(textarea, "Backspace");
  assert.deepEqual(removedSkills, [null], "any other key disarms, so the skill needs its own two presses");
});

test("saving a new default refreshes an open chat's model picker without restarting or switching it", async () => {
  // Keep App, Settings, Composer and useLive real. Only the native API and
  // unrelated visual surfaces are substituted.
  const cache = new Map(modules);
  const stub = (name, source) => cache.set(name, "data:text/javascript," + encodeURIComponent(source));
  for (const [name, exports] of Object.entries({
    FileViewer: ["FileViewer"], FirstRun: ["FirstRun"], Extensions: ["Extensions"],
    Rewind: ["Rewind"], LiveTail: ["LiveTail"], Palette: ["Palette"],
    WorkSurface: ["WorkSurface"], Transcript: ["Transcript"], UpdateReady: ["UpdateReady"],
    Welcome: ["Starters", "Welcome"], WorkspaceMenu: ["WorkspaceMenu"],
  })) stub(name, exports.map((name) => `export function ${name}(){return null}`).join(";"));
  stub("OutputScope", "export function OutputScope({children}){return children}");
  stub("Sidebar", `import React from ${JSON.stringify(import.meta.resolve("react"))};
    export const medhaIcon = '';
    export function Sidebar(props){return React.createElement('nav',null,
      React.createElement('button',{onClick:props.onSettings},'Open Settings'),
      React.createElement('button',{onClick:props.onNew},'New session'),
      ...props.sessions.map(session=>React.createElement('button',{
        key:session.id,onClick:()=>props.onSelect(session.id)},session.title)))}`);
  const { App } = await import(await moduleUrl("App", cache));
  globalThis.requestAnimationFrame = dom.window.requestAnimationFrame.bind(dom.window);
  globalThis.cancelAnimationFrame = dom.window.cancelAnimationFrame.bind(dom.window);
  HTMLElement.prototype.scrollIntoView = () => {};
  const saved = [{ name: "old", profile: { model: "old-model", base_url: "http://localhost/v1", protocol: "open-ai-chat", auth: "none" } }];
  let defaultName = "old", receive, failProfileReload = false;
  const active = new Map(), calls = [], opened = [];
  const catalogue = () => saved.map((model) => ({ ...model, default: model.name === defaultName, key_present: false }));
  const currentSettings = (name) => ({
    ...settings, profile: name, model: saved.find((model) => model.name === name).profile.model,
    profiles: catalogue().map((model) => ({ name: model.name, model: model.profile.model, default: model.default, protocol: model.profile.protocol })),
  });
  const api = {
    workspace: async () => "/project", sessions: async () => [{ id: "existing", title: "Existing chat", started_ts: 1 }],
    branch: async () => null, defaults: async () => currentSettings(defaultName),
    events: async () => ({ events: [], next_cursor: null }), extensions: async () => ({ skills: [] }),
    liveFocus: async () => {}, onLive: async (callback) => { receive = callback; return () => {}; },
    liveOpen: async (key, resume) => {
      opened.push(key);
      active.set(key, resume ? "old" : defaultName);
      receive(key, { method: "ready", params: { session: resume ?? key, model: currentSettings(active.get(key)).model } });
    },
    liveCall: async (key, method, change) => {
      calls.push({ key, method, change });
      if (method === "extensions.reload") return {};
      if (method === "session.profile.saved" && failProfileReload) {
        failProfileReload = false;
        throw new Error("Finish or stop active work before changing session settings.");
      }
      assert.ok(["session.settings", "session.configure", "session.profile.saved"].includes(method), method);
      if (method === "session.configure") active.set(key, change.profile);
      return currentSettings(active.get(key));
    },
    settings: async (method = "settings.list", params) => {
      if (method === "settings.model.save") { saved.push({ name: params.name, profile: params.profile }); return { saved: true }; }
      if (method === "settings.model.default") { defaultName = params.name; return { saved: true }; }
      assert.equal(method, "settings.list");
      return { models: catalogue(), protocols: [], search: { provider: "duckduckgo" } };
    },
  };
  Object.assign(globalThis.__medhaControlsTest, { api, workspace: { id: "project", name: "Project" }, theme: "dark", setTheme() {} });
  const button = (text) => [...document.querySelectorAll("button")].find((node) => node.textContent.trim() === text);
  const chip = () => document.querySelector(".chip");
  await act(async () => root.render(h(App)));
  await click(button("Existing chat"));
  await click(chip());
  assert.equal(document.querySelectorAll(".model-options button").length, 1);
  await click(document.querySelector('[aria-label="Close settings"]'));
  await click(button("Open Settings"));

  await act(async () => {
    await api.settings("settings.model.save", { name: "new", profile: { ...saved[0].profile, model: "new-model" } });
    window.dispatchEvent(new CustomEvent("medha-preferences"));
  });
  // Reload the Settings page, then use its real Make default action.
  await click(button("Existing chat"));
  await click(button("Open Settings"));
  await click(document.querySelectorAll(".ix-main")[1]);
  await click(button("Make default"));
  assert.equal(defaultName, "new");

  await click(button("Existing chat"));
  assert.equal(chip().textContent.trim(), "old", "changing the default must not switch an existing chat");
  await click(chip());
  const options = [...document.querySelectorAll(".model-options button")];
  assert.deepEqual(options.map((node) => node.querySelector("b").textContent), ["old", "new"]);
  assert.match(options[1].textContent, /new-model · default/);
  assert.doesNotMatch(options[0].textContent, /default/);
  assert.equal(options[0].getAttribute("aria-pressed"), "true");
  assert.equal(opened.length, 1, "refreshing a picker must reuse the existing chat");
  await click(options[1]);
  assert.equal(chip().textContent.trim(), "new");
  assert.ok(calls.some((call) => call.method === "session.configure" && call.change.profile === "new"));
  await click(document.querySelector('[aria-label="Close settings"]'));
  await click(button("New session"));
  assert.equal(chip().textContent.trim(), "new", "an unopened draft uses the new default");
  await click(chip());
  assert.equal(opened.length, 2);
  assert.equal(active.get(opened[1]), "new", "a new chat starts on the new default");

  // Saving a model's settings reaches the chats that use it, and only those.
  calls.length = 0;
  await act(async () => window.dispatchEvent(new CustomEvent("medha-preferences", { detail: { profile: "old" } })));
  await act(async () => {});
  assert.ok(!calls.some((call) => call.method === "session.profile.saved"), "another model's save leaves these chats alone");
  await act(async () => window.dispatchEvent(new CustomEvent("medha-preferences", { detail: { profile: "new" } })));
  await act(async () => {});
  const reloaded = calls.filter((call) => call.method === "session.profile.saved");
  calls.length = 0;
  failProfileReload = true;
  await act(async () => window.dispatchEvent(new CustomEvent("medha-preferences", { detail: { profile: "new" } })));
  await act(async () => {});
  const failedKey = calls.find((call) => call.method === "session.profile.saved").key;
  calls.length = 0;
  await act(async () => receive(failedKey, { method: "settings", params: currentSettings("new") }));
  await act(async () => {});
  assert.ok(calls.some((call) => call.key === failedKey && call.method === "session.profile.saved"), "a save refused at a turn boundary is retried rather than lost");
  assert.ok(reloaded.length > 0, "a chat on the saved model re-reads it");
  assert.ok(reloaded.every((call) => call.change.profile === "new"));
});

test("context details edit the exact saved profile and do not offer a broken editor for environment-only models", async () => {
  const { WorkSurface } = await import(await moduleUrl("WorkSurface"));
  const edited = [];
  const props = {
    tab: "context", blocks: [], run: null, showReasoning: false,
    sessionKey: null, sessionId: null, locked: false,
    onFile() {}, onTab() {}, onClose() {}, onAgent() {}, ensureOpen: async () => {},
    onEditModel: (profile) => edited.push(profile),
  };
  await act(() => root.render(h(WorkSurface, { ...props, settings: {
    ...settings, profile: "saved", profiles: [{ name: "saved" }], context_limit: undefined,
  } })));
  await click(namedButton("Set context window"));
  assert.deepEqual(edited, ["saved"]);
  await act(() => root.render(h(WorkSurface, { ...props, settings: {
    ...settings, profile: "override", profiles: [], context_limit: undefined,
  } })));
  assert.equal(namedButton("Set context window"), undefined);
  assert.match(document.querySelector(".context-details").textContent, /Unknown.*model configuration/);
  await act(() => root.render(h(WorkSurface, { ...props, settings })));
  assert.ok(document.querySelector(".context-details").textContent.includes(settings.context_limit.toLocaleString()));
  assert.equal(namedButton("Set context window"), undefined);
});

test("choosing a discovered Gemini model lets the backend name its profile and preserves the exact model ID", async () => {
  const { Settings } = await import(await moduleUrl("Settings"));
  const writes = [];
  const api = {
    settings: async (method = "settings.list", params) => {
      if (method === "settings.model.discover") return { models: [{ id: "gemini-3.8-flash", context_length: 1048576 }] };
      if (method === "settings.model.save") { writes.push(params); return { saved: true, name: "gemini-3-8-flash" }; }
      assert.equal(method, "settings.list");
      return { models: [], search: { provider: "duckduckgo" }, protocols: [{
        value: "gemini-interactions", label: "Gemini Interactions v1", available: true, auth: "x-goog-api-key", discovery: true,
        providers: [{ name: "Google Gemini", url: "https://generativelanguage.googleapis.com/v1", auth: "x-goog-api-key" }],
      }] };
    },
  };
  globalThis.__medhaControlsTest.api = api;
  const button = (text) => [...document.querySelectorAll("button")].find((node) => node.textContent.trim() === text);
  await act(async () => root.render(h(Settings, { sessionKey: null, ensureOpen: async () => {} })));
  await click(button("Add model"));
  await click(providerRow("Google Gemini"));
  assert.equal(button("Choose a protocol"), undefined, "a named provider selects its protocol");
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
  await act(() => {
    const key = document.querySelector('input[type="password"]');
    setter.call(key, "dummy-key");
    key.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await click(button("Find models on this server"));
  await click(modelRow("gemini-3.8-flash"));
  const inputFor = (label) => [...document.querySelectorAll("label")].find((node) => node.textContent.startsWith(label)).querySelector("input");
  assert.equal(inputFor("Model ID").value, "gemini-3.8-flash");
  const name = inputFor("Name");
  assert.equal(name.value, "", "let the backend derive a valid, unique profile name");
  assert.equal(name.required, false);
  await act(async () => document.querySelector(".settings-form").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  assert.equal(writes.length, 1);
  assert.equal(writes[0].name, "");
  assert.equal(writes[0].profile.model, "gemini-3.8-flash");
  assert.equal(writes[0].profile.protocol, "gemini-interactions");
  assert.equal(writes[0].profile.auth, "x-goog-api-key");
});

const namedButton = (label) => [...document.querySelectorAll("button")].find((node) => node.textContent.trim() === label);
// The connect sheet: a provider is a row in its list, a found model a row in the form.
function providerRow(name) {
  return [...document.querySelectorAll(".pv-row")].find((node) => node.querySelector("b")?.textContent.trim() === name);
}
function modelRow(id) {
  return [...document.querySelectorAll(".cx-model")].find((node) => node.querySelector("b")?.textContent.trim() === id);
}
async function inputValue(input, value) {
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
  await act(() => {
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

const fieldInput = (name) => [...document.querySelectorAll("label")].find((node) => node.textContent.trim().startsWith(name))?.querySelector("input");
const providerFixture = {
  id: "fixture", name: "Fixture Provider", url: "https://fixture.test/account-a/v1", auth: "bearer",
  parameters: [], models: [{ id: "fixture-model", context_length: 8000 }],
};
function providerSettings(providers = [providerFixture], models = []) {
  return { models, search: { provider: "duckduckgo" }, unavailable_providers: [{ id: "future", name: "Native Future", reason: "This native protocol is not implemented." }], protocols: [{ value: "open-ai-chat", label: "OpenAI Chat", available: true, auth: "bearer", discovery: true, providers }] };
}
async function providerForm(settingsApi) {
  const { Settings } = await import(await moduleUrl("Settings"));
  globalThis.__medhaControlsTest.api = { settings: settingsApi };
  await act(async () => root.render(h(Settings, { sessionKey: null, ensureOpen: async () => {} })));
  await click(namedButton("Add model"));
  assert.equal(namedButton("Choose a protocol"), undefined);
}
const searchBox = () => document.querySelector('input[aria-label="Search providers, or paste a key"]');

test("provider search saves its identity without reusing another endpoint's key or automatically testing", async () => {
  const calls = [];
  const existing = { name: "old", default: true, key_present: true, profile: { protocol: "open-ai-chat", model: "old", base_url: "https://fixture.test/account-b/v1", auth: "bearer" } };
  await providerForm(async (method = "settings.list", params) => {
    calls.push({ method, params });
    if (method === "settings.model.save") return { saved: true };
    assert.equal(method, "settings.list");
    return providerSettings([providerFixture], [existing]);
  });
  await inputValue(searchBox(), "Fixture");
  await click(providerRow("Fixture Provider"));
  assert.equal(fieldInput("API endpoint").value, providerFixture.url);
  assert.equal(fieldInput("Key").placeholder, "Paste your Fixture Provider key", "same host with another account path is a different credential target");
  assert.equal(namedButton("Find models on this server"), undefined, "nothing is looked up before a key");
  assert.match(document.body.textContent, /Paste your key to see Fixture Provider’s models/);
  await inputValue(fieldInput("Key"), "dummy-only-key");
  await inputValue(fieldInput("Model ID"), "fixture-model");
  await act(async () => document.querySelector(".settings-form").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  const save = calls.find((call) => call.method === "settings.model.save");
  assert.equal(save.params.profile.provider_id, "fixture");
  assert.equal(save.params.profile.base_url, providerFixture.url);
  assert.equal(save.params.key, "dummy-only-key");
  assert.ok(!calls.some((call) => call.method === "settings.model.test" || call.method === "settings.model.discover"));
});

test("provider refresh is a secondary labelled control beside the search and reports failure", async () => {
  let finishRefresh, rejectRefresh, lists = 0;
  await providerForm(async (method = "settings.list") => {
    if (method === "settings.list") { lists++; return providerSettings(); }
    if (method === "application.providers.refresh") return new Promise((resolve, reject) => { finishRefresh = resolve; rejectRefresh = reject; });
    assert.fail(method);
  });
  const refresh = document.querySelector('.pv-search button[aria-label="Refresh provider catalogue"]');
  assert.ok(refresh);
  assert.equal(refresh.type, "button");
  assert.ok(refresh.classList.contains("icon-btn"));
  assert.ok(!refresh.classList.contains("btn-line"));
  const unavailable = document.querySelector(".pv-unavailable");
  assert.match(unavailable.querySelector("summary").textContent, /need a connection Medha doesn't have yet/);
  assert.match(unavailable.textContent, /Native Future.*This native protocol is not implemented/);
  assert.equal(unavailable.querySelector("button"), null, "unavailable entries cannot start setup");
  await click(refresh);
  assert.equal(refresh.disabled, true);
  assert.equal(refresh.getAttribute("aria-busy"), "true");
  await act(async () => rejectRefresh(new Error("Catalogue is unavailable. Try again later.")));
  assert.match(document.querySelector('[role="alert"]').textContent, /Catalogue is unavailable/);
  assert.equal(refresh.disabled, false);
  const before = lists;
  await click(refresh);
  await act(async () => finishRefresh({}));
  assert.ok(!document.querySelector(".pv-error"));
  assert.ok(lists > before, "a refreshed catalogue is read again");
  await click(providerRow(providerFixture.name));
  assert.equal(fieldInput("API endpoint").value, providerFixture.url);
});

test("saved custom servers appear under Custom endpoint and reuse only their exact saved key", async () => {
  const calls = [];
  const address = "https://private-gateway.test/team-a/v1";
  const connection = { provider_id: "cloudflare-workers-ai", headers: { "x-company": "tenant-a" }, ca_cert_file: "/company.pem", proxy: "http://proxy.test:8080" };
  const existing = { name: "private", default: true, key_present: true, profile: { ...connection, protocol: "open-ai-chat", model: "old", base_url: address, auth: "bearer", max_ctx: 64000, max_output_tokens: 8192 } };
  const other = { ...existing, name: "other-account", profile: { ...existing.profile, base_url: "https://private-gateway.test/team-b/v1", headers: { "x-company": "tenant-b" } } };
  await providerForm(async (method = "settings.list", params) => {
    calls.push({ method, params });
    if (method === "settings.list") return providerSettings([providerFixture], [existing, other]);
    if (method === "settings.model.discover") return { models: [{ id: "fixture-model", context_length: 8000 }] };
    if (method === "settings.model.save") return { saved: true };
    assert.fail(method);
  });
  const ownRows = [...document.querySelectorAll(".pv-row")].filter((row) => row.textContent.includes("private-gateway.test"));
  assert.ok(ownRows.length > 0, "the person's own servers are offered again under Custom");
  assert.ok(ownRows.every((row) => row.textContent.includes("Your server")), "a user's own endpoint is not a public provider");
  await click(providerRow("Another server"));
  assert.equal(fieldInput("API endpoint").value, "");
  await click(namedButton("Enter a new endpoint"));
  await click(option("private-gateway.test/team-a/v1"));
  assert.equal(fieldInput("API endpoint").value, address);
  assert.match(document.querySelector(".cx-keyline").textContent, /Saved on this computer/);
  assert.equal(fieldInput("Key"), undefined, "a saved key is used, not asked for again");
  const discovery = calls.find((call) => call.method === "settings.model.discover");
  assert.equal(discovery.params.profile.base_url, address);
  assert.equal(discovery.params.profile.auth, "bearer");
  assert.equal(discovery.params.key, "");
  for (const [key, value] of Object.entries(connection)) assert.deepEqual(discovery.params.profile[key], value, key);
  await inputValue(fieldInput("Model ID"), "another-model");
  await act(async () => document.querySelector(".settings-form").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  const save = calls.find((call) => call.method === "settings.model.save");
  assert.equal(save.params.profile.base_url, address);
  for (const [key, value] of Object.entries(connection)) assert.deepEqual(save.params.profile[key], value, key);
  assert.equal(save.params.profile.max_ctx, null, "the old model's window is not copied");
  assert.equal(save.params.profile.max_output_tokens, null, "the old model's output cap is not copied");

  // Reopening a resolved catalogue deployment must not ask for its account again.
  await act(() => root.render(null));
  const { Settings } = await import(await moduleUrl("Settings"));
  const reopened = { ...existing, name: "another-model", profile: save.params.profile };
  const native = { ...providerFixture, id: connection.provider_id, parameters: ["CLOUDFLARE_ACCOUNT_ID"], url: "https://fixture.test/${CLOUDFLARE_ACCOUNT_ID}/v1" };
  globalThis.__medhaControlsTest.api = { settings: async () => providerSettings([native], [reopened]) };
  await act(async () => root.render(h(Settings, { sessionKey: null, ensureOpen: async () => {}, editModel: reopened.name })));
  assert.equal(fieldInput("CLOUDFLARE ACCOUNT ID"), undefined);
  assert.equal(namedButton("Use endpoint"), undefined);
  assert.equal(fieldInput("API endpoint").value, address);

  await act(() => root.render(null));
  await providerForm(async () => providerSettings());
  assert.ok(![...document.querySelectorAll(".pv-row")].some((row) => row.textContent.includes("private-gateway.test")), "the endpoint does not appear in another user's empty setup");
  await click(providerRow("Another server"));
  assert.ok(!namedButton("Enter a new endpoint"), "no saved-server picker is needed without saved servers");
});

test("model setup distinguishes a timeout from a rejected model and keeps saving available", async () => {
  const writes = [];
  await providerForm(async (method = "settings.list", params) => {
    if (method === "settings.list") return providerSettings();
    if (method === "settings.model.test") return {
      text: { status: "failed", failure: "timeout", message: "The setup test reached its time limit. Model support is not confirmed." },
      streaming: { status: "skipped", failure: null, message: "Not tested because an earlier check failed." },
      tools: { status: "skipped", failure: null, message: "Not tested because an earlier check failed." },
    };
    if (method === "settings.model.save") { writes.push(params); return { saved: true }; }
    assert.fail(method);
  });
  await click(providerRow(providerFixture.name));
  await inputValue(fieldInput("Key"), "dummy-only-key");
  await inputValue(fieldInput("Model ID"), "fixture-model");
  assert.match(document.body.textContent, /up to four small test requests, which your provider may bill/);
  await click(namedButton("Check connection"));
  assert.match(document.querySelector('[role="status"]').textContent, /Text: timed out/);
  assert.ok(!document.querySelector('[role="status"]').textContent.includes("Text: failed"));
  assert.match(document.querySelector('[role="status"]').textContent, /Streaming: skipped/);
  await act(async () => document.querySelector(".settings-form").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  assert.equal(writes.length, 1);
});

/** The backend's answer when a server will not list: the named provider's catalogue and why, or the error. */
function standIn(params, providers, reason) {
  const provider = providers.find((p) => p.id === params?.profile?.provider_id);
  if (provider?.models?.length) return { models: provider.models, unverified: reason };
  throw new Error(reason);
}

test("switching to a saved custom server never offers the previous provider's catalogue on discovery failure", async () => {
  const saved = [
    { name: "public", default: true, key_present: true, profile: { protocol: "open-ai-chat", model: "old", base_url: providerFixture.url, auth: "bearer" } },
    { name: "private", default: false, key_present: true, profile: { protocol: "open-ai-chat", model: "old-private", base_url: "https://private-gateway.test/v1", auth: "bearer" } },
  ];
  await providerForm(async (method = "settings.list", params) => {
    if (method === "settings.list") return providerSettings([providerFixture], saved);
    if (method === "settings.model.discover") return standIn(params, [providerFixture], "Discovery unavailable");
    assert.fail(method);
  });
  await click(providerRow(providerFixture.name));
  assert.match(document.body.textContent, /Showing catalogue models/);
  await click(namedButton("‹ All providers")); await click(providerRow("Another server"));
  await click(namedButton("Enter a new endpoint")); await click(option("private-gateway.test"));
  assert.ok(!document.body.textContent.includes("Showing catalogue models"));
  assert.equal(document.querySelector(".cx-model"), null);
  assert.match(document.body.textContent, /enter the model ID manually/);
});

test("failed discovery offers catalogue models, clears stale context and permits manual save after an optional failed test", async () => {
  const calls = [];
  let finishTest;
  const provider = { ...providerFixture, models: [...providerFixture.models, { id: "manual-window", context_length: null }] };
  await providerForm(async (method = "settings.list", params) => {
    calls.push({ method, params });
    if (method === "settings.list") return providerSettings([provider]);
    if (method === "settings.model.discover") return standIn(params, [provider], "403: access not granted to this account");
    if (method === "settings.model.test") return new Promise((resolve) => { finishTest = resolve; });
    if (method === "settings.model.save") return { saved: true };
    assert.fail(method);
  });
  await click(providerRow(provider.name));
  await inputValue(fieldInput("Key"), "dummy-only-key");
  await click(namedButton("Find models on this server"));
  assert.match(document.querySelector(".model-discovery").parentElement.textContent, /availability is unverified/);
  await click(modelRow("fixture-model"));
  assert.equal(fieldInput("Context window").value, "8000");
  await click(modelRow("manual-window"));
  assert.equal(fieldInput("Context window").value, "", "the earlier model's context must not persist");
  await click(namedButton("Check connection"));
  assert.ok(finishTest, "test runs only on explicit click");
  await inputValue(fieldInput("Model ID"), "other-model");
  const failed = { status: "failed", failure: "access_restricted", message: "old model not allowed" };
  await act(async () => finishTest({ text: failed, streaming: failed, tools: failed }));
  assert.ok(!document.body.textContent.includes("old model not allowed"), "a late result cannot validate another model");
  await act(async () => document.querySelector(".settings-form").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  assert.equal(calls.find((call) => call.method === "settings.model.save").params.profile.model, "other-model");
});

test("endpoint parameters resolve explicitly and a closed form ignores late resolution", async () => {
  const { ProviderFields } = await import(await moduleUrl("ProviderSetup"));
  let finish, requests = [], resolved = [];
  globalThis.__medhaControlsTest.api = { settings: (method, params) => {
    requests.push({ method, params });
    return new Promise((resolve) => { finish = resolve; });
  } };
  const provider = { ...providerFixture, id: "cloudflare-workers-ai", parameters: ["CLOUDFLARE_ACCOUNT_ID"] };
  await act(() => root.render(h(ProviderFields, { provider, disabled: false, onChange() {}, onResolved: (url) => resolved.push(url) })));
  assert.equal(namedButton("Use endpoint").disabled, true);
  await inputValue(fieldInput("CLOUDFLARE ACCOUNT ID"), "my-account");
  assert.equal(requests.length, 0);
  await click(namedButton("Use endpoint"));
  assert.deepEqual(requests, [{ method: "application.providers.resolve", params: { provider_id: provider.id, parameters: { CLOUDFLARE_ACCOUNT_ID: "my-account" } } }]);
  await act(() => root.render(null));
  await act(async () => finish({ base_url: "https://fixture.test/my-account/v1" }));
  assert.deepEqual(resolved, []);
});

test("first-run setup filters providers and keeps unsupported providers visible with reasons", async () => {
  const { FirstRun } = await import(await moduleUrl("FirstRun"));
  const writes = [];
  const other = { ...providerFixture, id: "other", name: "Other Provider" };
  globalThis.__medhaControlsTest.api = { settings: async (method = "settings.list", params) => {
    if (method === "settings.list") return providerSettings([providerFixture, other]);
    if (method === "settings.model.discover") return standIn(params, [providerFixture, other], "Discovery is unavailable");
    if (method === "settings.model.save") { writes.push(params); return { saved: true }; }
    if (method === "settings.model.context") return { published: null };
    assert.fail("No model test should run automatically: " + method);
  } };
  await act(async () => root.render(h(FirstRun, { onDone() {}, onClose() {} })));
  await inputValue(searchBox(), "Fixture");
  assert.equal(providerRow("Other Provider"), undefined);
  assert.match(document.querySelector(".pv-unavailable").textContent, /Native Future.*not implemented/);
  await click(providerRow("Fixture Provider"));
  await inputValue(fieldInput("Key"), "dummy-key");
  await click(namedButton("Find models on this server"));
  assert.match(document.body.textContent, /availability is unverified/);
  assert.equal(document.querySelector(".checkbox-label"), null, "the first model is always the default");
  await click(modelRow("fixture-model"));
  await click(namedButton("Start"));
  assert.equal(writes.length, 1);
  assert.equal(writes[0].profile.provider_id, "fixture");
  assert.equal(writes[0].profile.model, "fixture-model");
  assert.equal(writes[0].default, true);
});

test("first-run provider selection chooses Gemini without a protocol question and keeps custom setup available", async () => {
  const { FirstRun } = await import(await moduleUrl("FirstRun"));
  const calls = [];
  const google = { id: "google", name: "Google Gemini", url: "https://generativelanguage.googleapis.com/v1", auth: "x-goog-api-key", models: [] };
  globalThis.__medhaControlsTest.api = { settings: async (method = "settings.list", params) => {
    calls.push({ method, params });
    if (method === "settings.list") return { ...providerSettings(), protocols: [
      ...providerSettings().protocols,
      { value: "gemini-interactions", label: "Gemini Interactions", available: true, auth: "x-goog-api-key", discovery: true, providers: [google] },
    ] };
    if (method === "settings.model.discover") return { models: [{ id: "gemini-fixture", context_length: 8000 }] };
    if (method === "settings.model.save") return { saved: true };
    assert.fail(method);
  } };
  let closed = 0;
  await act(async () => root.render(h(FirstRun, { onDone() {}, onClose() { closed++; } })));
  await click(providerRow("Google Gemini"));
  assert.equal(namedButton("Choose a protocol"), undefined, "protocol does not precede a named provider");
  await inputValue(fieldInput("Key"), "dummy-gemini-key");
  await click(namedButton("Find models on this server"));
  await click(modelRow("gemini-fixture"));
  await click(namedButton("Start"));
  const profile = calls.find((call) => call.method === "settings.model.save").params.profile;
  assert.deepEqual([profile.protocol, profile.provider_id, profile.auth], ["gemini-interactions", "google", "x-goog-api-key"]);
  await act(async () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })));
  assert.equal(closed, 0, "Esc steps back to the list before it closes anything");
  await click(providerRow("Another server"));
  await click(namedButton("OpenAI Chat"));  assert.ok(option("OpenAI Chat") && option("Gemini Interactions"), "custom endpoints retain explicit protocol selection");
  await click(namedButton("‹ All providers"));
  await click(namedButton("Not now"));
  assert.equal(closed, 1);
});

async function extensionForm(kind, chooseExtensionSource) {
  const writes = [];
  globalThis.__medhaControlsTest.chooseExtensionSource = chooseExtensionSource;
  globalThis.__medhaControlsTest.api = {
    settings: async () => ({ mcp: [] }),
    extensions: async (method, params) => {
      if (!method) return { plugins: [], skills: [], notices: [], skill_errors: [] };
      assert.ok(["extensions.skill.install", "extensions.install"].includes(method));
      writes.push([method, params]);
      return { enabled: true };
    },
  };
  const cache = new Map(modules);
  // The unrelated connectors page loads images through Vite's browser-only glob.
  cache.set("Connectors", "data:text/javascript," + encodeURIComponent("export function Connectors(){return null}"));
  const { Extensions } = await import(await moduleUrl("Extensions", cache));
  const render = async (locked = false) => act(async () => root.render(h(Extensions, {
    sessionKey: null, ensureOpen: async () => {}, locked, onAsk() {},
  })));
  await render();
  await click(namedButton(kind === "skill" ? "Skills" : "Plugins"));
  await click(namedButton(`Add ${kind}`));
  return { writes, render };
}

test("Add skill browses a folder or Markdown file and waits for an explicit install", async () => {
  const picked = [];
  const folder = "/Users/test/My skills/research";
  const file = "/Users/test/Documents/SKILL.md";
  const { writes } = await extensionForm("skill", async (kind) => {
    picked.push(kind);
    return kind === "skill_folder" ? folder : file;
  });
  await click(document.querySelector('[aria-label="Choose local skill"]'));
  await click(namedButton("Choose folder"));
  assert.deepEqual(picked, ["skill_folder"]);
  assert.equal(document.querySelector('[aria-label="Skill source"]').value, folder);
  assert.deepEqual(writes, [], "selecting a source must not install it");
  await click(namedButton("Install skill"));
  assert.deepEqual(writes, [["extensions.skill.install", { source: folder }]]);
  assert.match(document.querySelector('[role="status"]').textContent, /Skill installed/);

  await click(namedButton("Add skill"));
  await click(document.querySelector('[aria-label="Choose local skill"]'));
  await key(namedButton("Choose folder"), "Escape");
  assert.equal(document.querySelector('[aria-label="Choose a skill from your files"]'), null);
  assert.equal(document.activeElement, document.querySelector('[aria-label="Choose local skill"]'));
  await click(document.querySelector('[aria-label="Choose local skill"]'));
  await click(namedButton("Choose SKILL.md"));
  assert.deepEqual(picked, ["skill_folder", "skill_file"]);
  assert.equal(document.querySelector('[aria-label="Skill source"]').value, file);
  assert.equal(writes.length, 1);
  await click(namedButton("Install skill"));
  assert.deepEqual(writes[1], ["extensions.skill.install", { source: file }]);
});

test("Add plugin retains pasted sources on cancel and installs a chosen folder with review still required", async () => {
  const picked = [];
  let selection = null;
  const { writes } = await extensionForm("plugin", async (kind) => {
    picked.push(kind);
    return selection;
  });
  const input = () => document.querySelector('[aria-label="Plugin source"]');
  await inputValue(input(), "owner/repo");
  await click(document.querySelector('[aria-label="Choose local plugin"]'));
  assert.equal(input().value, "owner/repo", "cancel does not erase a pasted source");
  await click(namedButton("Install"));
  assert.deepEqual(writes[0], ["extensions.install", { source: "owner/repo" }]);
  assert.match(document.querySelector('[role="status"]').textContent, /Installed and off.*review its access/);

  await click(namedButton("Add plugin"));
  selection = "/Users/test/Downloads/My plugin";
  await click(document.querySelector('[aria-label="Choose local plugin"]'));
  assert.equal(input().value, selection);
  assert.equal(writes.length, 1);
  await click(namedButton("Install"));
  assert.deepEqual(writes[1], ["extensions.install", { source: selection }]);
  assert.deepEqual(picked, ["plugin_folder", "plugin_folder"]);
});

test("picker errors allow retry, pending selections cannot install or leak into another extension form", async () => {
  let choose = async () => { throw new Error("Picker failed"); };
  const { writes, render } = await extensionForm("skill", (kind) => choose(kind));
  const input = () => document.querySelector('[aria-label="Skill source"]');
  const browse = () => document.querySelector('[aria-label="Choose local skill"]');
  await inputValue(input(), "https://github.com/owner/repo/tree/main/skill");
  await click(browse());
  await click(namedButton("Choose SKILL.md"));
  assert.match(document.querySelector('[role="alert"]').textContent, /Picker failed/);
  assert.equal(browse().disabled, false);
  choose = async () => null;
  await click(browse());
  await click(namedButton("Choose SKILL.md"));
  assert.equal(document.querySelector('[role="alert"]'), null);
  assert.equal(input().value, "https://github.com/owner/repo/tree/main/skill");
  await click(namedButton("Install skill"));
  assert.deepEqual(writes[0], ["extensions.skill.install", { source: "https://github.com/owner/repo/tree/main/skill" }]);

  await click(namedButton("Add skill"));
  await render(true);
  assert.equal(browse().disabled, true);
  assert.equal(input().disabled, true);
  await render(false);
  let finish;
  choose = () => new Promise((resolve) => { finish = resolve; });
  await click(browse());
  await click(namedButton("Choose folder"));
  assert.equal(browse().disabled, true);
  assert.equal(input().disabled, true);
  await act(() => document.querySelector(".plugin-install").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  assert.equal(writes.length, 1);
  await click(namedButton("Plugins"));
  await act(async () => finish("/Users/test/old skill"));
  assert.equal(document.querySelector('[aria-label="Plugin source"]').value, "");
  assert.equal(writes.length, 1);
});

const screenshotData = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
async function attachmentChat() {
  const { LiveTail } = await import(await moduleUrl("LiveTail"));
  const { Transcript } = await import(await moduleUrl("Transcript"));
  const { toBlocks } = await import(await moduleUrl("timeline"));
  const writes = [], reads = [];
  let receive, live, changeHistory;
  globalThis.__medhaControlsTest.api = {
    onLive: async (callback) => { receive = callback; return () => {}; },
    liveOpen: async () => {},
    liveCall: async (_key, method, params) => {
      if (method === "session.settings") return settings;
      assert.equal(method, "message.send");
      writes.push(params);
      return { accepted: true };
    },
    historyImage: async (hash, thumbnail) => {
      reads.push([hash, thumbnail]);
      return { name: "saved image", mime: "image/png", data: screenshotData };
    },
  };
  globalThis.__medhaControlsTest.admitImage = async () => ({ mime: "image/png", data: screenshotData });
  globalThis.__medhaControlsTest.chooseAttachments = async () => null;
  function Harness() {
    const [draft, setDraft] = React.useState("Take a look");
    const [attachments, setAttachments] = React.useState([]);
    const [history, setHistory] = React.useState(null);
    changeHistory = setHistory;
    live = useLive();
    return h(React.Fragment, null,
      history ? h(Transcript, { blocks: toBlocks(history) })
        : live.live.chat && h(LiveTail, { state: live.live.chat, onApprove() {}, onAnswer() {}, onOpenAgent() {} }),
      h(Composer, {
        value: draft, onChange: setDraft,
        onSend: () => { void live.send("chat", null, draft, attachments); setDraft(""); setAttachments([]); },
        onStop() {}, running: false, disabled: false, placeholder: "Task", settings, settingsLocked: false,
        control: null, onControl() {}, onLoadSettings: async () => {}, onConfigure: async () => {},
        showReasoning: false, onShowReasoning() {}, attachments, onAttachments: setAttachments,
        onSurface() {}, onNew() {}, onRewind() {}, onSettings() {}, onExtensions() {},
      }));
  }
  await act(() => root.render(h(Harness)));
  return {
    writes, reads,
    receive: (params) => act(async () => receive("chat", { method: "event", params })),
    history: (events) => act(async () => changeHistory(events)),
    state: () => live.live.chat,
  };
}

test("a pasted screenshot is visible beside its sent text, survives acknowledgement, and displays again from history", async () => {
  const chat = await attachmentChat();
  const screenshot = new dom.window.File(["test image"], "Screenshot.png", { type: "image/png" });
  screenshot.arrayBuffer = async () => new Uint8Array([1, 2, 3]).buffer;
  await act(async () => {
    const pasted = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(pasted, "clipboardData", { value: { files: [screenshot] } });
    document.querySelector("textarea").dispatchEvent(pasted);
  });
  assert.equal(document.querySelectorAll(".msg-token.token-image img").length, 1);
  await click(document.querySelector('[aria-label="Send message"]'));
  assert.equal(chat.writes[0].images[0].name, "Screenshot.png");
  assert.equal(document.querySelector(".you .sent-image img").alt, "Screenshot.png");
  assert.match(document.querySelector(".you").textContent, /Take a look/);

  const images = [{ hash: "a".repeat(64), name: "Screenshot.png", mime: "image/png" }];
  await chat.receive({ kind: "message.accepted", content: "Take a look", images });
  assert.equal(document.querySelectorAll(".you").length, 1, "the acknowledgement must not duplicate the message");
  assert.deepEqual(chat.reads, [[images[0].hash, true]]);
  await click(document.querySelector('[aria-label="Open Screenshot.png"]'));
  assert.equal(document.querySelector(".message-image-full").alt, "Screenshot.png");
  assert.deepEqual(chat.reads.at(-1), [images[0].hash, false]);
  await click(document.querySelector('[aria-label="Close"]'));
  await chat.history([{ kind: "user", id: "saved-user", text: "Take a look", ts: 1, images }]);
  assert.equal(document.querySelector(".you .sent-image img").alt, "Screenshot.png");
  assert.match(document.querySelector(".you").textContent, /Take a look/);
});

test("the composer selects arbitrary files and a folder, preserves cancellation and sends durable path references", async () => {
  const chat = await attachmentChat();
  const pdf = { id: "pdf", kind: "file", name: "Report.pdf", path: '/Users/test/My docs/report "final".pdf' };
  const archive = { id: "archive", kind: "file", name: "Archive.zip", path: "/Users/test/Archive.zip" };
  const folder = { id: "folder", kind: "folder", name: "Project", path: "/Users/test/Project files" };
  const choices = [];
  globalThis.__medhaControlsTest.chooseAttachments = async (kind) => {
    choices.push(kind);
    return kind === "folder" ? [folder] : [pdf, archive];
  };
  await click(document.querySelector('[aria-label="Attach files"]'));
  await click(namedButton("Files"));
  assert.equal(document.querySelectorAll(".msg-token.token-file").length, 2);
  globalThis.__medhaControlsTest.chooseAttachments = async () => null;
  await click(document.querySelector('[aria-label="Attach files"]'));
  await click(namedButton("Files"));
  assert.equal(document.querySelectorAll(".msg-token.token-file").length, 2);
  globalThis.__medhaControlsTest.chooseAttachments = async (kind) => { choices.push(kind); return [folder]; };
  await click(document.querySelector('[aria-label="Attach files"]'));
  await click(namedButton("Folder"));
  assert.equal(document.querySelectorAll(".msg-token.token-folder").length, 1);
  await click(document.querySelector('[aria-label="Send message"]'));
  const { attachedFiles } = await import(await moduleUrl("messageRequest"));
  const parsed = attachedFiles(chat.writes[0].content);
  assert.equal(parsed.text, "Take a look");
  assert.deepEqual(parsed.files.map((file) => file.path), [pdf.path, archive.path, folder.path]);
  assert.deepEqual(chat.writes[0].images, []);
  assert.deepEqual(choices, ["files", "folder"]);
  assert.equal(document.querySelectorAll(".you .sent-file").length, 3);
  assert.equal(document.querySelector(".you p").textContent, "Take a look");
  assert.equal(chat.state().items[0].text, chat.writes[0].content);
  await chat.history([{ kind: "user", id: "saved-files", text: chat.writes[0].content, ts: 1 }]);
  assert.equal(document.querySelectorAll(".you .sent-file").length, 3);
});

test("rewind restores both saved images and local files as removable draft attachments", async () => {
  const { Rewind } = await import(await moduleUrl("Rewind"));
  const { messageRequest } = await import(await moduleUrl("messageRequest"));
  const document = { id: "doc", kind: "file", name: "Report.pdf", path: "/Users/test/Report.pdf" };
  const prefill = messageRequest("Read the report", [document]).content;
  let restored;
  globalThis.__medhaControlsTest.api.liveCall = async (_key, method) => {
    if (method === "session.rewind.points") return { points: [{ id: "point", text: prefill, files: 0 }] };
    assert.equal(method, "session.rewind");
    return { source: "old", session: "new", prefill, code_only: false,
      images: [{ id: "image", name: "Screenshot.png", mime: "image/png", data: screenshotData }] };
  };
  await act(async () => root.render(h(Rewind, { sessionKey: "chat", ensureOpen: async () => {}, onClose() {}, onComplete: (result) => { restored = result; } })));
  await click(namedButton("Rewind"));
  assert.equal(restored.prefill, "Read the report");
  assert.equal(restored.attachments[0].preview, `data:image/png;base64,${screenshotData}`);
  assert.equal(restored.attachments[1].path, document.path);
});

test("a late file selection preserves token removals and non-image drops explain the native file chooser", async () => {
  const chat = await attachmentChat();
  const pdf = { id: "pdf", kind: "file", name: "Report.pdf", path: "/Users/test/Report.pdf" };
  const archive = { id: "zip", kind: "file", name: "Archive.zip", path: "/Users/test/Archive.zip" };
  const folder = { id: "folder", kind: "folder", name: "Project", path: "/Users/test/Project" };
  globalThis.__medhaControlsTest.chooseAttachments = async () => [pdf, archive];
  await click(document.querySelector('[aria-label="Attach files"]'));
  await click(namedButton("Files"));
  let finish;
  globalThis.__medhaControlsTest.chooseAttachments = () => new Promise((resolve) => { finish = resolve; });
  await click(document.querySelector('[aria-label="Attach files"]'));
  await click(namedButton("Folder"));
  await click(document.querySelector('[aria-label="Remove Report.pdf"]'));
  assert.equal(document.querySelectorAll(".msg-token.token-file").length, 1);
  await act(async () => finish([folder]));
  assert.equal(document.querySelectorAll(".msg-token.token-file").length, 1);
  assert.equal(document.querySelectorAll(".msg-token.token-folder").length, 1);

  await act(async () => {
    const drop = new Event("drop", { bubbles: true, cancelable: true });
    Object.defineProperty(drop, "dataTransfer", { value: { files: [new dom.window.File(["%PDF"], "Report.pdf", { type: "application/pdf" })] } });
    document.querySelector("form.composer").dispatchEvent(drop);
  });
  assert.match(document.querySelector(".composer-error").textContent, /Attach → Files or Folder/);
  await click(document.querySelector('[aria-label="Send message"]'));
  const { attachedFiles } = await import(await moduleUrl("messageRequest"));
  assert.deepEqual(attachedFiles(chat.writes[0].content).files.map((file) => file.path), [archive.path, folder.path]);
});

test("a long picker narrows as you type, forgives a slip, and picks with Enter", async () => {
  const ids = ["stepfun/step-5-preview", "anthropic/claude-haiku-5.5", "anthropic/claude-haiku-5.5:batch", "google/gemini-nano-banana-2.1", "mistralai/mistral-large-4-0"];
  let submitted = 0;
  function Harness() {
    const [value, setValue] = React.useState("");
    return h("form", { onSubmit(event) { event.preventDefault(); submitted++; } },
      h(Select, { id: "picker", value, search: "Search models", onChange: (event) => setValue(event.target.value) },
        h("option", { value: "" }, "Choose a model"), ...ids.map((id) => h("option", { key: id, value: id }, id))));
  }
  await act(() => root.render(h(Harness)));
  await click(document.getElementById("picker"));
  const box = document.querySelector('input[aria-label="Search models"]');
  assert.equal(document.activeElement, box, "typing starts in the search box");
  const listed = () => [...document.querySelectorAll('[role="option"]')].map((node) => node.textContent.trim());
  await inputValue(box, "haiku batch");
  assert.deepEqual(listed(), ["anthropic/claude-haiku-5.5:batch"], "every typed word must fit");
  await inputValue(box, "gmni");
  assert.deepEqual(listed(), ["google/gemini-nano-banana-2.1"], "a dropped letter still finds it");
  await inputValue(box, "zzzz");
  assert.deepEqual(listed(), []);
  await inputValue(box, "claude");
  assert.deepEqual(listed(), ids.slice(1, 3));
  await key(box, "ArrowDown");
  await key(box, "Enter");
  assert.equal(document.getElementById("picker").textContent, "anthropic/claude-haiku-5.5:batch");
  assert.equal(document.querySelector('[role="listbox"]'), null);
  assert.equal(submitted, 0);
});

test("a second model on a server whose key is saved asks for no key and lists its models at once", async () => {
  const { Settings } = await import(await moduleUrl("Settings"));
  const calls = [];
  const nvidia = "https://integrate.api.nvidia.com/v1";
  globalThis.__medhaControlsTest.api = {
    settings: async (method = "settings.list", params) => {
      if (method !== "settings.list") calls.push([method, params]);
      if (method === "settings.model.discover") return { models: [{ id: "meta/muse-glimmer-30b", context_length: 130000 }] };
      if (method === "settings.model.save") return { saved: true, name: "muse-glimmer-30b" };
      return {
        models: [{ name: "glm-5-3", default: true, key_present: true, profile: { model: "z-ai/glm-5.3", base_url: nvidia, protocol: "open-ai-chat", auth: "bearer" } }],
        search: { provider: "duckduckgo" },
        protocols: [{ value: "open-ai-chat", label: "OpenAI-compatible Chat", available: true, auth: "bearer", discovery: true, providers: [] }],
      };
    },
  };
  await act(async () => root.render(h(Settings, { sessionKey: null, ensureOpen: async () => {} })));
  await click(namedButton("Add model"));
  await click(providerRow("Another server"));
  await click(namedButton("Enter a new endpoint"));
  await click(option("integrate.api.nvidia.com"));
  await act(async () => {});
  assert.deepEqual(calls.map(([method, params]) => [method, params.profile.base_url, params.key]), [["settings.model.discover", nvidia, ""]], "the saved key is used; none is typed or sent");
  await click(modelRow("meta/muse-glimmer-30b"));
  await act(async () => document.querySelector(".settings-form").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  const [, saved] = calls.at(-1);
  assert.deepEqual([saved.profile.model, saved.profile.base_url, saved.profile.auth, saved.key], ["meta/muse-glimmer-30b", nvidia, "bearer", ""]);
});
