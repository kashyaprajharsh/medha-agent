import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";
import { JSDOM } from "jsdom";

const source = await readFile(new URL("../src/outputConsent.ts", import.meta.url), "utf8");
const { outputText } = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext } });
let serial = 0;
const fresh = () => import(`data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}#${++serial}`);
const page = (extra = {}) => ({ connect: [], resources: ["https://screen.example"], frames: [], identity: "connection-a", ...extra });
test.beforeEach(() => { globalThis.localStorage = new JSDOM("", { url: "https://medha.test" }).window.localStorage; });

test("remembered consent keeps schemes, ports, capability kinds and server identity separate", async () => {
  const c = await fresh();
  const original = page();
  const identity = c.screenIdentity("drawing", "ui://drawing", original);
  c.agreeScreen(identity, c.screenScope(original), true);
  const reopened = await fresh();
  assert.equal(reopened.screenAgreed(identity, c.screenScope(original)), true);
  for (const requested of [
    page({ connect: ["wss://screen.example:8443"] }),
    page({ resources: ["https://screen.example:8443"] }),
    page({ frames: ["https://screen.example"] }),
    page({ resources: ["https://*.screen.example"] }),
    page({ resources: ["http://screen.example"] }),
    page({ clipboard: true }),
  ]) assert.equal(reopened.screenAgreed(identity, c.screenScope(requested)), false, JSON.stringify(requested));
  assert.equal(reopened.screenAgreed(c.screenIdentity("drawing", "ui://drawing", page({ identity: "connection-b" })), c.screenScope(original)), false);
  assert.equal(c.screenScope(page({ resources: ["https://a.example", "https://b.example"] })), c.screenScope(page({ resources: ["https://b.example", "https://a.example", "https://a.example"] })));
});

test("session approvals and legacy hostname grants do not turn into durable consent", async () => {
  localStorage.setItem("medha-screen-agreed", JSON.stringify(["drawing:screen.example"]));
  const c = await fresh();
  const declaration = page();
  const identity = c.screenIdentity("drawing", "ui://drawing", declaration);
  assert.equal(c.screenAgreed(identity, c.screenScope(declaration)), false);
  c.agreeScreen(identity, c.screenScope(declaration), false);
  assert.equal(c.screenAgreed(identity, c.screenScope(declaration)), true);
  const reopened = await fresh();
  assert.equal(reopened.screenAgreed(identity, c.screenScope(declaration)), false);
});

test("durable grants expire and remain bounded", async () => {
  const c = await fresh();
  const declaration = page();
  const scope = c.screenScope(declaration);
  for (let at = 0; at < 70; at++) c.agreeScreen(c.screenIdentity(`server-${at}`, "ui://drawing", declaration), scope, true);
  const saved = JSON.parse(localStorage.getItem("medha-screen-grants-v2"));
  assert.equal(saved.length, 64);
  assert.equal(c.screenAgreed(c.screenIdentity("server-0", "ui://drawing", declaration), scope), false);
  const expired = c.screenIdentity("expired", "ui://drawing", declaration);
  localStorage.setItem("medha-screen-grants-v2", JSON.stringify([
    { identity: expired, scope, at: Date.now() - 91 * 24 * 60 * 60_000 },
    { identity: "future", scope, at: Date.now() + 60_000 },
  ]));
  const reopened = await fresh();
  assert.equal(reopened.screenAgreed(expired, scope), false);
  assert.equal(reopened.screenAgreed("future", scope), false);
});
