// First-run setup looks for servers and saves a model without the person filling a
// form, so its few rules decide what is contacted unasked and what is stored.
import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import ts from "typescript";

const source = await readFile(new URL("../src/modelSetup.ts", import.meta.url), "utf8");
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
});
const { authFor, isLocal, profileFor, reasonFor, sizeNote } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`
);

test("only this computer is looked at without being asked", () => {
  for (const url of ["http://localhost:11434/v1", "http://127.0.0.1:1234/v1", "http://[::1]:8080/v1"])
    assert.equal(isLocal(url), true, url);
  for (const url of ["https://localhost.example.com/v1", "http://127.0.0.1.example.com/v1", "http://192.168.1.20:8000/v1", "https://openrouter.ai/api/v1", "not a url"])
    assert.equal(isLocal(url), false, url);
});

test("your own server is sent a key only when you gave one", () => {
  assert.equal(authFor({ custom: true, auth: "bearer" }, ""), "none");
  assert.equal(authFor({ custom: true, auth: "bearer" }, "sk-1"), "bearer");
  assert.equal(authFor({ custom: false, auth: "bearer" }, ""), "bearer");
});

test("a model saved here is the one Settings would save", () => {
  const profile = profileFor({ protocol: "open-ai-chat", url: "http://localhost:11434/v1", auth: "none" }, "qwen3", 262144);
  assert.deepEqual(profile, {
    model: "qwen3",
    base_url: "http://localhost:11434/v1",
    protocol: "open-ai-chat",
    auth: "none",
    max_ctx: 262144,
    max_output_tokens: null,
    reasoning: "unknown",
    reasoning_efforts: null,
    image_input: "auto",
    token_counter: "none",
    token_accounting: "adaptive",
    chat_token_limit: "auto",
    headers: {},
  });
});

test("an empty context size is never passed off as a limit Medha does not have", () => {
  assert.match(sizeNote(true, 262144), /^The server did not report it\. Left empty, Medha uses 262,144 tokens/);
  assert.match(sizeNote(true, null), /found no size.*will not be shortened automatically/);
  assert.doesNotMatch(sizeNote(false, null), /\d/);
  assert.doesNotMatch(sizeNote(false, undefined), /uses|\d/);
});

test("a failed lookup says what to do, not what the socket said", () => {
  assert.match(reasonFor("401 Unauthorized: invalid api key", "https://openrouter.ai/api/v1"), /key was not accepted/);
  assert.equal(
    reasonFor("error sending request: connection refused", "http://10.0.0.9:8000/v1"),
    "Nothing answered at 10.0.0.9:8000. Start the server, or check the address.",
  );
  assert.equal(reasonFor("Error: the server returned HTML", "http://x.test/v1"), "the server returned HTML");
});
