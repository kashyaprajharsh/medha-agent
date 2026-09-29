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
        "export function useWorkspace(){return globalThis.__medhaControlsTest} export function useWorkspaceApi(){return globalThis.__medhaControlsTest.api}",
      ),
  ],
  [
    "api",
    "data:text/javascript," + encodeURIComponent("export const desktop = {}"),
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
