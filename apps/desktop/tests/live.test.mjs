import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const source = await readFile(
  new URL("../src/live.ts", import.meta.url),
  "utf8",
);
const { outputText } = ts.transpileModule(source, {
  compilerOptions: {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
  },
});
const { reduceLive, startingLive, recordSent, queuedMessages, explainError } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`
);
const event = (kind, params = {}) => ({
  method: "event",
  params: { kind, ...params },
});

test("pushed outputs are kept until they may be incomplete, then listed afresh", () => {
  const record = (id) => ({ artifact_id: `file:${id}`, revision_id: `${id}:file`, predecessor: null, source_event: id, source_index: 0, ts: 1, kind: "page", name: id });
  let state = reduceLive(startingLive(), event("outputs", { revisions: [record("a")] }));
  state = reduceLive(state, event("outputs", { revisions: [record("b"), { bogus: true }] }));
  assert.deepEqual(state.outputs.map((output) => output.revision_id), ["a:file", "b:file"]);
  const epoch = state.outputsEpoch ?? 0;
  // A rewind or switch, and a recovery after the stream was away, may each have
  // missed what was sent: both drop the list and ask for a fresh listing.
  const reset = reduceLive(state, event("presentation.reset", { revision: 2 }));
  assert.deepEqual(reset.outputs, []);
  assert.equal(reset.outputsEpoch, epoch + 1);
  const recovered = reduceLive(state, { method: "session.presentation", params: { frames: [] } });
  assert.deepEqual(recovered.outputs, []);
  assert.equal(recovered.outputsEpoch, epoch + 1);
  const flooded = reduceLive(state, event("outputs", { revisions: Array.from({ length: 2000 }, (_, n) => record(`n${n}`)) }));
  assert.deepEqual(flooded.outputs, [], "past the bound the list is let go, not grown");
  assert.equal(flooded.outputsEpoch, epoch + 1);
});

test("identical live replies bind to their own durable source events without losing rendered HTML", () => {
  let state = reduceLive(startingLive(), event("turn.started", { turn: 1 }));
  state = reduceLive(state, event("model.text", { delta: "same diagram", html: "<p>same diagram</p>" }));
  state = reduceLive(state, event("model.text", { delta: "", source_event: "event-one" }));
  assert.equal(state.items.at(-1).sourceEvent, "event-one");
  assert.equal(state.items.at(-1).html, "<p>same diagram</p>");
  state = reduceLive(state, event("turn.done"));
  state = reduceLive(state, event("turn.started", { turn: 2 }));
  state = reduceLive(state, event("message.accepted", { content: "again" }));
  state = reduceLive(state, event("model.text", { delta: "same diagram", html: "<p>same diagram</p>" }));
  state = reduceLive(state, event("model.text", { delta: "", source_event: "event-two" }));
  assert.deepEqual(state.items.filter(item => item.kind === "text").map(item => item.sourceEvent), ["event-one", "event-two"]);
});

test("late attachment replaces active state and retains settings, prompts and agents", () => {
  let state = reduceLive(startingLive(), event("model.text", { delta: "stale" }));
  const snapshot = {
    method: "session.presentation",
    params: {
      session: "conversation", running: true,
      settings: { model: "test-model", mode: "plan" },
      frames: [event("model.reasoning", { delta: "checking" }), event("model.text", { delta: "**current", html: "<p>**current</p>" })],
      approvals: [{ gate_id: 9, action: "shell.exec", detail: "pwd" }],
      questions: [{ question_id: 10, questions: [{ prompt: "Choose", options: [] }] }],
      agents: [{ name: "worker", path: "worker", session: "child", objective: "read", status: "running" }],
    },
  };
  state = reduceLive(state, snapshot);
  assert.equal(state.sessionId, "conversation");
  assert.equal(state.model, "test-model");
  assert.equal(state.settings.mode, "plan");
  assert.equal(state.status, "running");
  assert.equal(state.items.at(-1).text, "**current");
  assert.deepEqual(state.approvals.map((a) => a.gateId), [9]);
  assert.deepEqual(state.questions.map((q) => q.id), [10]);
  assert.equal(state.agents[0].session, "child");
  state = reduceLive(state, snapshot);
  assert.equal(state.items.length, 2, "a repeated snapshot must replace, not duplicate");
  state = reduceLive(state, event("model.text", { delta: "**", html: "<p><strong>current</strong></p>" }));
  assert.equal(state.items.at(-1).text, "**current**");
  assert.match(state.items.at(-1).html, /<strong>current<\/strong>/);
});

test("idle shared attachment keeps durable history outside the live tail", () => {
  const state = reduceLive(startingLive(), { method: "session.presentation", params: {
    session: "conversation", running: false, frames: [], approvals: [], questions: [], agents: [],
  } });
  assert.equal(state.status, "idle");
  assert.deepEqual(state.items, []);
});

test("other viewers' messages appear while optimistic local messages appear once", () => {
  let state = recordSent(startingLive(), { kind: "user", id: "local-id", text: "local", ts: 1 });
  state = reduceLive(state, event("message.accepted", { content: "local" }));
  assert.equal(state.items.filter((item) => item.kind === "user").length, 1);
  assert.equal(state.items[0].id, "local-id", "the optimistic item keeps its identity");
  state = reduceLive(state, event("turn.done"));
  state = reduceLive(state, event("message.accepted", { content: "other viewer" }));
  state = reduceLive(state, event("turn.started", { turn: 2 }));
  assert.equal(state.status, "running");
  assert.deepEqual(state.items.filter((item) => item.kind === "user").map((item) => item.text), ["local", "other viewer"]);
  state = reduceLive(state, event("message.steered", { content: "other viewer steer" }));
  assert.equal(state.items.at(-1).text, "other viewer steer");
});

test("what a message was about is the backend's word, and never doubles the sent message", () => {
  const sent = (about) => recordSent(startingLive(), { kind: "user", id: "local-id", text: "make it blue", ts: 1, about });
  const users = (state) => state.items.filter((item) => item.kind === "user").map((item) => [item.id, item.text, item.about]);
  // Acceptance carries the person's own words, so it matches the message already shown.
  const confirmed = reduceLive(sent("Diagram: Read path"), event("message.accepted", { content: "make it blue", about: "Diagram: Read path" }));
  assert.deepEqual(users(confirmed), [["local-id", "make it blue", "Diagram: Read path"]]);
  // A reference the backend could not confirm loses its label.
  const dropped = reduceLive(sent("Diagram: Read path"), event("message.accepted", { content: "make it blue" }));
  assert.deepEqual(users(dropped), [["local-id", "make it blue", undefined]]);
  // Another window, which never sent it, shows the same label.
  const elsewhere = reduceLive({ ...startingLive(), status: "idle" }, event("message.accepted", { content: "make it blue", about: "Diagram: Read path" }));
  assert.deepEqual(users(elsewhere).map(([, text, about]) => [text, about]), [["make it blue", "Diagram: Read path"]]);
});

test("image references reach other viewers and a recovered current turn", () => {
  const images = [{ hash: "a".repeat(64), name: "shot.png", mime: "image/png" }];
  const accepted = event("message.accepted", { content: "Look", images });
  let state = reduceLive(startingLive(), accepted);
  assert.deepEqual(state.items[0].images, images);
  state = reduceLive(state, { method: "session.presentation", params: {
    session: "conversation", running: true, frames: [accepted], approvals: [], questions: [], agents: [],
  } });
  assert.deepEqual(state.items[0].images, images);
  assert.equal(state.items.length, 1);
});

test("a retry removes the abandoned response while preserving completed work", () => {
  let state = reduceLive(
    startingLive(),
    event("tool.call", { id: "read", tool: "read" }),
  );
  state = reduceLive(
    state,
    event("tool.observation", {
      id: "read",
      ok: true,
      output: "File contents",
    }),
  );
  state = reduceLive(
    state,
    event("model.text", { delta: "Abandoned response" }),
  );
  state = reduceLive(
    state,
    event("model.reasoning", { delta: "Abandoned thought" }),
  );
  state = reduceLive(state, event("model.restarted"));
  assert.equal(state.items.length, 1);
  assert.equal(state.items[0].steps[0].output, "File contents");
  state = reduceLive(
    state,
    event("model.text", { delta: "Replacement response" }),
  );
  assert.equal(state.items[1].text, "Replacement response");
});

test("budget and verification stops remain different from ordinary completion", () => {
  const budget = reduceLive(
    startingLive(),
    event("turn.done", { stopped: "tokens" }),
  );
  const failed = reduceLive(
    startingLive(),
    event("turn.done", { stopped: "verification_failed" }),
  );
  const done = reduceLive(
    startingLive(),
    event("turn.done", { stopped: null }),
  );
  assert.equal(budget.stopReason, "tokens");
  assert.match(budget.notice, /tokens limit/);
  assert.match(failed.notice, /Verification failed/);
  assert.equal(done.notice, undefined);
});

test("an approval settled in another window leaves this one", () => {
  let state = startingLive();
  for (const gate_id of [1, 2]) {
    state = reduceLive(state, {
      method: "approval",
      params: { gate_id, action: "shell.exec" },
    });
  }
  state = reduceLive(state, {
    method: "approval.resolved",
    params: { gate_id: 1, approved: true },
  });
  assert.deepEqual(
    state.approvals.map((approval) => approval.gateId),
    [2],
  );
});

test("stopping pending work stops its spinner and releases human prompts", () => {
  let state = reduceLive(
    startingLive(),
    event("tool.call", { id: "slow", tool: "shell.exec" }),
  );
  state = reduceLive(state, {
    method: "approval",
    params: { gate_id: 1, action: "shell.exec" },
  });
  state = reduceLive(state, {
    method: "question",
    params: { question_id: 2, questions: [] },
  });
  state = reduceLive(state, event("turn.cancelled"));
  assert.equal(state.items[0].steps[0].status, "stopped");
  assert.deepEqual(state.approvals, []);
  assert.deepEqual(state.questions, []);
});

test("unknown cache usage stays unknown while a reported zero stays zero", () => {
  const unknown = reduceLive(
    startingLive(),
    event("usage", { prompt_tokens: 30, total_tokens: 50 }),
  );
  const zero = reduceLive(
    unknown,
    event("usage", {
      prompt_tokens: 30,
      total_tokens: 50,
      cached_prompt_tokens: 0,
    }),
  );
  assert.equal(unknown.usage.cached, undefined);
  assert.equal(zero.usage.cached, 0);
});

test("answer acknowledgement releases only the corresponding form", () => {
  let state = reduceLive(startingLive(), {
    method: "question",
    params: { question_id: 1, questions: [] },
  });
  state = reduceLive(state, {
    method: "question",
    params: { question_id: 2, questions: [] },
  });
  state = reduceLive(state, {
    method: "question.answered",
    params: { question_id: 1 },
  });
  assert.deepEqual(
    state.questions.map((form) => form.id),
    [2],
  );
});

test("configuration request errors do not masquerade as failed turns", () => {
  const state = reduceLive(startingLive(), {
    id: 7,
    error: { message: "Unsupported effort" },
  });
  assert.equal(state.error, undefined);
});

test("nested agents retain full addresses and writer role", () => {
  const state = reduceLive(startingLive(), {
    method: "agents",
    params: {
      agents: [
        {
          name: "review",
          path: "/writer/review",
          write: true,
          session: "child",
          status: "running",
        },
      ],
    },
  });
  assert.equal(state.agents[0].path, "/writer/review");
  assert.equal(state.agents[0].write, true);
});

test("rewind switches to its branch and clears the old live tail and approvals", () => {
  let state = reduceLive(startingLive(), {
    method: "ready",
    params: { session: "original", model: "local" },
  });
  state = reduceLive(state, event("model.text", { delta: "Old answer" }));
  const next = reduceLive(state, {
    method: "session.rewound",
    params: { session: "branch", code_only: false },
  });
  assert.equal(next.sessionId, "branch");
  assert.equal(next.status, "idle");
  assert.equal(next.items.length, 0);
  assert.equal(next.model, "local");
  const filesOnly = reduceLive(state, {
    method: "session.rewound",
    params: { session: "original", code_only: true },
  });
  assert.equal(filesOnly.sessionId, "original");
  assert.deepEqual(filesOnly.items, state.items);
});

test("follow-ups enter the timeline at the steering boundary, not above earlier work", () => {
  const user = (id, text) => ({ kind: "user", id, text, ts: 1 });
  let state = recordSent(startingLive(), user("initial", "task"));
  state = reduceLive(state, event("model.text", { delta: "before" }));
  state = reduceLive(state, event("tool.call", { id: "tool", tool: "read" }));
  state = recordSent(state, user("followup", "change direction"));
  assert.deepEqual(
    state.items.map((item) => item.kind),
    ["user", "text", "tools"],
  );
  assert.deepEqual(
    queuedMessages(state).map((item) => item.id),
    ["followup"],
  );
  state = reduceLive(
    state,
    event("message.steered", { content: "change direction" }),
  );
  state = reduceLive(
    state,
    event("tool.observation", { id: "tool", ok: true, output: "done" }),
  );
  state = reduceLive(
    state,
    event("model.text", { delta: "after", html: "<p>after</p>" }),
  );
  assert.deepEqual(
    state.items.map((item) => item.kind),
    ["user", "text", "tools", "user", "text"],
  );
  assert.equal(state.items[3].id, "followup");
  assert.equal(
    state.items[2].steps[0].status,
    "ok",
    "earlier in-flight tools still receive their results",
  );
  assert.equal(state.items[4].text, "after");
  assert.deepEqual(queuedMessages(state), []);
});

test("a follow-up a stopped turn never read goes back to the composer, not the timeline", () => {
  const user = (id, text) => ({ kind: "user", id, text, ts: 1 });
  let state = recordSent(startingLive(), user("initial", "task"));
  state = reduceLive(state, { method: "ready", params: { session: "s" } });
  state = reduceLive(state, event("model.text", { delta: "answer" }));
  state = recordSent(state, user("late", "also do this"));
  state = recordSent(state, user("twin", "also do this"));
  state = reduceLive(
    state,
    event("message.returned", { contents: ["also do this", "[agent report]"] }),
  );
  state = reduceLive(state, event("turn.cancelled", {}));
  assert.equal(state.status, "idle");
  assert.deepEqual(state.returned, ["also do this"], "only typed text returns");
  assert.deepEqual(
    state.items.filter((item) => item.kind === "user").map((item) => item.id),
    ["initial", "twin"],
    "only the returned copy leaves the timeline",
  );
});

test("a follow-up carried into the next run joins the timeline and keeps the run going", () => {
  const user = (id, text) => ({ kind: "user", id, text, ts: 1 });
  let state = recordSent(startingLive(), user("initial", "task"));
  state = reduceLive(state, { method: "ready", params: { session: "s" } });
  state = reduceLive(state, event("model.text", { delta: "answer" }));
  state = recordSent(state, user("late", "check the rules too"));
  state = reduceLive(
    state,
    event("message.steered", { content: "check the rules too" }),
  );
  state = reduceLive(
    state,
    event("turn.continued", { stopped: "verification_failed" }),
  );
  assert.equal(state.status, "running");
  assert.match(state.notice, /Verification failed/, "the earlier stop stays visible");
  assert.deepEqual(
    state.items.filter((item) => item.kind === "user").map((item) => item.id),
    ["initial", "late"],
  );
  assert.equal(state.returned, undefined);
});

test("identical follow-up text stays distinct and unread messages survive cancellation", () => {
  let state = reduceLive(
    startingLive(),
    event("model.text", { delta: "before" }),
  );
  for (const id of ["first", "second", "unread"])
    state = recordSent(state, {
      kind: "user",
      id,
      text: "same instruction",
      ts: 1,
    });
  state = reduceLive(
    state,
    event("message.steered", { content: "same instruction" }),
  );
  state = reduceLive(state, event("model.text", { delta: "middle" }));
  state = reduceLive(
    state,
    event("message.steered", { content: "same instruction" }),
  );
  state = reduceLive(state, event("model.text", { delta: "retry me" }));
  state = reduceLive(state, event("model.restarted"));
  assert.deepEqual(
    state.items.filter((item) => item.kind === "user").map((item) => item.id),
    ["first", "second"],
  );
  state = reduceLive(state, event("turn.cancelled"));
  assert.deepEqual(
    state.items.filter((item) => item.kind === "user").map((item) => item.id),
    ["first", "second", "unread"],
  );
  assert.deepEqual(queuedMessages(state), []);
});

test("compaction, provider waiting, streaming and cancellation have distinct activity states", () => {
  let state = reduceLive(startingLive(), event("compacting", { active: true }));
  assert.equal(state.activity.kind, "compacting");
  state = reduceLive(
    state,
    event("compaction", { before: 99000, after: 20000 }),
  );
  state = reduceLive(state, event("compacting", { active: false }));
  assert.equal(state.activity, undefined);
  state = reduceLive(state, event("model.waiting"));
  assert.equal(state.activity.kind, "model");
  state = reduceLive(
    state,
    event("notice", { text: "Model request failed. Retrying (1/3)…" }),
  );
  assert.match(state.notice, /Retrying/);
  state = reduceLive(state, event("model.reasoning", { delta: "thinking" }));
  assert.equal(state.activity, undefined);
  assert.equal(state.notice, undefined);
  state = reduceLive(state, event("model.waiting"));
  state = reduceLive(state, event("turn.cancelled"));
  assert.equal(state.activity, undefined);
});

test("context percentage is the recounted input budget, never a fake compaction reset", () => {
  let state = reduceLive(
    startingLive(),
    event("context_pressure", {
      input_tokens: 99143,
      usable_input_tokens: 100000,
      percent: 99,
      quality: "local_estimate",
    }),
  );
  state = reduceLive(
    state,
    event("compaction", { before: 99143, after: 20000 }),
  );
  assert.equal(
    state.contextPercent,
    99,
    "wait for the prepared request recount",
  );
  state = reduceLive(
    state,
    event("context_pressure", {
      input_tokens: 24000,
      usable_input_tokens: 100000,
      percent: 24,
      quality: "local_estimate",
    }),
  );
  assert.equal(state.contextPercent, 24);
  assert.deepEqual(state.contextPressure, {
    input: 24000,
    usable: 100000,
    quality: "local_estimate",
  });
  state = reduceLive(state, event("context_pressure", { input_tokens: 24000 }));
  assert.equal(
    state.contextPercent,
    undefined,
    "unknown limits must clear stale percentage",
  );
});

test("unavailable tools retain a structured error through live and history views", async () => {
  const source = await readFile(new URL("../src/timeline.ts", import.meta.url), "utf8");
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
  });
  const { toBlocks, stepVerb, stepStatusLabel } = await import(
    `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`
  );
  const tool = "mcp__mcp-remote__brave_search";
  let state = reduceLive(startingLive(), event("tool.call", {
    id: "guess", tool, verb: "Brave search", target: "agents",
  }));
  state = reduceLive(state, event("tool.observation", {
    id: "guess", ok: false, error_code: "tool_unavailable", detail: "Tool is unavailable",
  }));
  const history = toBlocks([
    { id: "call", kind: "tool_call", ts: 1, tool_id: "guess", text: tool, verb: "Brave search" },
    { id: "obs", kind: "tool_result", ts: 2, tool_id: "guess", status: "error",
      error_code: "tool_unavailable", detail: "Tool is unavailable" },
  ]);
  for (const step of [state.items[0].steps[0], history[0].steps[0]]) {
    assert.equal(step.status, "failed");
    assert.equal(stepVerb(step), tool);
    assert.equal(stepStatusLabel(step), "Unavailable tool");
    assert.equal(step.detail, "Tool is unavailable");
  }
  const denied = { tool, verb: "Brave search", status: "denied" };
  assert.equal(stepVerb(denied), "Brave search");
  assert.equal(stepStatusLabel(denied), "Denied");
});

test("a call being written is kept only while it is useful, and never without bound", () => {
  const feed = (state, id, delta, tool = "mcp__draw__view") => reduceLive(state, event("tool.input", { id, tool, delta }));
  let state = feed(feed(startingLive(), "a", '{"elements":"['), "a", '{}]"}');
  assert.deepEqual(state.writing, [{ id: "a", tool: "mcp__draw__view", text: '{"elements":"[{}]"}' }]);

  // Its screen arriving is what ends it; the result alone leaves the drawing up.
  state = reduceLive(state, event("tool.call", { id: "a", tool: "mcp__draw__view" }));
  state = reduceLive(state, event("tool.observation", { id: "a", tool: "mcp__draw__view", ok: true }));
  assert.equal(state.writing.length, 1);
  state = reduceLive(state, event("tool.screen", { id: "a", screen: { server: "draw", tool: "view", resource: "ui://draw/view" } }));
  assert.deepEqual(state.writing, []);
  assert.equal(state.items.at(-1).steps[0].screen.resource, "ui://draw/view");

  // A failed call, a retry and the end of the turn each drop what was written.
  state = feed(state, "b", "{");
  state = reduceLive(state, event("tool.call", { id: "b", tool: "mcp__draw__view" }));
  state = reduceLive(state, event("tool.observation", { id: "b", tool: "mcp__draw__view", ok: false }));
  assert.deepEqual(state.writing, []);
  assert.equal(reduceLive(feed(state, "c", "{"), event("model.restarted")).writing, undefined);
  assert.equal(reduceLive(feed(state, "c", "{"), event("turn.done")).writing, undefined);

  // Many calls, or one enormous one, stop growing the window's memory.
  for (const id of ["1", "2", "3", "4", "5", "6"]) state = feed(state, id, "{");
  assert.deepEqual(state.writing.map((call) => call.id), ["3", "4", "5", "6"]);
  const big = "x".repeat(600 * 1024);
  state = feed(feed(feed(state, "6", big), "6", big), "6", big);
  assert.ok(state.writing.at(-1).text.length <= 1024 * 1024);
});

test("reused provider call IDs attach a screen only to its own saved observation", () => {
  let state = startingLive();
  const call = () => { state = reduceLive(state, event("tool.call", { id: "reused", tool: "mcp__draw__view" })); };
  const result = (source) => { state = reduceLive(state, event("tool.observation", { id: "reused", ok: true, source_event: source })); };
  const screen = (source) => { state = reduceLive(state, event("tool.screen", { id: "reused", screen: { server: "draw", tool: "view", resource: "ui://draw", source_event: source } })); };
  call(); result("first"); screen("first");
  state = reduceLive(state, event("model.text", { delta: "Next drawing" }));
  call(); result("second"); screen("second");
  const groups = state.items.filter(item => item.kind === "tools");
  assert.equal(groups.length, 2);
  assert.equal(groups[0].steps[0].screen.source_event, "first");
  assert.equal(groups[1].steps[0].screen.source_event, "second");
  // A late duplicate must not overwrite the other drawing, either.
  screen("first");
  assert.equal(state.items.filter(item => item.kind === "tools")[1].steps[0].screen.source_event, "second");
});

test("a refused request shows the provider's reason instead of blaming the key", () => {
  // Exactly what a gated free model answers to a valid key.
  const gated =
    'provider error: provider returned status 403: {"error":{"code":403,"message":"vendor/model:free is only available on agentic harnesses.","metadata":{"failed_routing_step":"Gate Free Endpoints by Agentic Harness"}}}\nThe task did not finish.';
  const shown = explainError(gated);
  assert.match(shown, /only available on agentic harnesses/);
  assert.doesNotMatch(shown, /credentials/);
  // No readable reason still must not claim the key is wrong.
  assert.doesNotMatch(explainError("provider error: provider returned status 403: <html>Forbidden</html>"), /rejected the credentials/);
  assert.match(explainError('provider error: provider returned status 401: {"detail":"Invalid API Key"}'), /rejected the credentials/);
});
