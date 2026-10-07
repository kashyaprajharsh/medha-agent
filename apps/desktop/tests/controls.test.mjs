import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import ts from "typescript";
import { JSDOM } from "jsdom";
import React, { act } from "react";

const dom = new JSDOM('<!doctype html><div id="root"></div>', {
  pretendToBeVisual: true,
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
])
  globalThis[name] = dom.window[name];
const { createRoot } = await import("react-dom/client");
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
globalThis.matchMedia = () => ({ matches: true });
globalThis.ResizeObserver = class {
  observe() {}
  disconnect() {}
};
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
    "data:text/javascript," + encodeURIComponent("export const desktop = {}"),
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
async function moduleUrl(name) {
  if (modules.has(name)) return modules.get(name);
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
  for (const match of [...outputText.matchAll(/from ["']([^"']+)["']/g)]) {
    const dependency = match[1];
    const url = dependency.startsWith("./")
      ? await moduleUrl(dependency.slice(2))
      : import.meta.resolve(dependency);
    outputText = outputText
      .replaceAll(`"${dependency}"`, JSON.stringify(url))
      .replaceAll(`'${dependency}'`, JSON.stringify(url));
  }
  const url = `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`;
  modules.set(name, url);
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
  assert.deepEqual(shown, [
    {
      anchor: "live-0:1",
      kind: "diagram",
      source: "---\ntitle: Read path\n---\nflowchart LR\n  A --> B\n  B --> C",
      path: undefined,
      screen: undefined,
      name: "Read path",
    },
  ]);

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
  const Probe = () => ((outputs = useOutputs()), null);
  const version = (body) =>
    `<pre data-lang="mermaid"><code>---\ntitle: Read path\n---\nflowchart LR\n  A --&gt; ${body}</code></pre>`;
  const chat = async (...replies) => {
    await act(() =>
      root.render(
        h(
          OutputScope,
          { chat: "c", viewing: true, onShow() {}, onMade: () => free, onAsk() {} },
          h(Probe),
          replies.map((html, index) =>
            h(Markdown, { key: index, html, scope: `r${index}`, at: index, streaming: index === writing }),
          ),
        ),
      ),
    );
    await act(() => new Promise((resolve) => setTimeout(resolve, 50)));
  };

  await chat(version("B"));
  await act(() => outputs.show(outputs.all[0]));
  assert.equal(outputs.open.anchor, "r0:0");

  await chat(version("B"), version("C"));
  assert.equal(outputs.open.anchor, "r1:0", "the viewer follows the revision");
  assert.equal(document.querySelectorAll(".out-plate").length, 1, "the first version keeps its plate");
  assert.match(document.querySelector(".out-revision.current").textContent, /Read path.*v2/);

  await act(() => outputs.follow(outputs.all[0]));
  await chat(version("B"), version("C"), version("D"));
  assert.equal(outputs.open.anchor, "r0:0", "a chosen older version stays put");
  assert.deepEqual(outputs.all.map((output) => output.anchor), ["r0:0", "r1:0", "r2:0"]);

  await chat(version("B"));
  assert.deepEqual(outputs.all.map((output) => output.anchor), ["r0:0"], "a reply that leaves takes its outputs");

  // What Medha finishes writing opens by itself, unless the person is busy elsewhere.
  const other = '<pre data-lang="mermaid"><code>flowchart LR\n  X --&gt; Y</code></pre>';
  await act(() => outputs.browse());
  writing = 1;
  await chat(version("B"), other);
  assert.equal(outputs.open, null, "nothing opens while it is still being written");
  writing = -1;
  await chat(version("B"), other);
  assert.equal(outputs.open.anchor, "r1:0");
  await act(() => outputs.browse());
  free = false;
  writing = 2;
  await chat(version("B"), other, other);
  writing = -1;
  await chat(version("B"), other, other);
  assert.equal(outputs.open, null, "it stays out of the way of another panel");
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
