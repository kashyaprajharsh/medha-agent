import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const load = async (name) => {
  const source = await readFile(new URL(`../src/${name}.ts`, import.meta.url), "utf8");
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
  });
  return import(`data:text/javascript,${encodeURIComponent(outputText)}`);
};
const { hitRate } = await load("cache");

test("a hit rate is shown only for what a server reported, and never rounds up to a full hit", () => {
  assert.equal(hitRate(9_000, 10_000), "90%");
  assert.equal(hitRate(0, 10_000), "0%", "a reported miss is a real 0%");
  assert.equal(hitRate(undefined, 10_000), undefined, "silence is not a miss");
  assert.equal(hitRate(0, 0), undefined, "nothing measured, nothing claimed");
  assert.equal(hitRate(9_999, 10_000), ">99%");
  assert.equal(hitRate(10_000, 10_000), "100%");
  assert.equal(hitRate(12_000, 10_000), "100%", "a count above the prompt is held to it");
});
