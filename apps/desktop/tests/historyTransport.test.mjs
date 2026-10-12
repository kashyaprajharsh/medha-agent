import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

async function load(file) {
  const source = await readFile(new URL(`../src/${file}`, import.meta.url), "utf8");
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
  });
  return import(`data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`);
}

const { toBlocks } = await load("timeline.ts");
const { messageRequest } = await load("messageRequest.ts");

test("saved structured omissions produce notices and keep surrounding history", () => {
  const events = [
    { id: "call", kind: "tool_call", ts: 0, tool_id: "tool", text: "mcp.tool" },
    { id: "one", kind: "tool_result", ts: 1, tool_id: "tool", output: "saved result", omitted: ["screen", "plan"] },
    { id: "two", kind: "assistant", ts: 2, text: "kept answer" },
  ];
  const blocks = toBlocks(events);
  assert.equal(blocks.filter((block) => block.kind === "notice").length, 1);
  assert.match(blocks.find((block) => block.kind === "notice").text, /screen and plan/);
  assert.ok(blocks.some((block) => block.kind === "assistant" && block.text === "kept answer"));
  assert.ok(blocks.some((block) => block.kind === "tools" && block.steps.some((step) => step.output === "saved result")));
  assert.deepEqual(events[1].omitted, ["screen", "plan"]);
});

test("a reopened chat names the model that stood in, once per message of the person's", () => {
  const stand_in = { model: "glm-flash", for: "qwen-main" };
  const blocks = toBlocks([
    { id: "u1", kind: "user", ts: 0, text: "first" },
    { id: "a1", kind: "assistant", ts: 1, text: "part one", stand_in },
    { id: "a2", kind: "assistant", ts: 2, text: "part two", stand_in },
    { id: "u2", kind: "user", ts: 3, text: "second" },
    { id: "a3", kind: "assistant", ts: 4, text: "own model again" },
    { id: "u3", kind: "user", ts: 5, text: "third" },
    { id: "a4", kind: "assistant", ts: 6, text: "stood in again", stand_in },
  ]);
  const shape = blocks.map((block) => (block.kind === "notice" ? block.text : block.kind));
  const said = "Answered by glm-flash, standing in for qwen-main.";
  assert.deepEqual(shape, ["user", said, "assistant", "assistant", "user", "assistant", "user", said, "assistant"]);
});

test("the send budget measures aggregate processed images and UTF-8 JSON", () => {
  const attachment = { mime: "image/png", data: "x".repeat(5 * 1024 * 1024) };
  assert.equal(messageRequest("hello", [attachment, attachment]).images.length, 2);
  assert.throws(() => messageRequest("hello", [attachment, attachment, attachment, attachment]), /16 MiB/);
  assert.throws(() => messageRequest("😃".repeat(4 * 1024 * 1024), []), /16 MiB/);
  assert.equal(messageRequest("hello", []).content, "hello");
});
