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

test("the send budget measures aggregate processed images and UTF-8 JSON", () => {
  const attachment = { mime: "image/png", data: "x".repeat(5 * 1024 * 1024) };
  assert.equal(messageRequest("hello", [attachment, attachment]).images.length, 2);
  assert.throws(() => messageRequest("hello", [attachment, attachment, attachment, attachment]), /16 MiB/);
  assert.throws(() => messageRequest("😃".repeat(4 * 1024 * 1024), []), /16 MiB/);
  assert.equal(messageRequest("hello", []).content, "hello");
});
