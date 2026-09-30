import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import ts from "typescript";

const source = await readFile(new URL("../src/strum.ts", import.meta.url), "utf8");
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
});
const { crossed, ring } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`
);

const LANES = [22, 36, 50, 64];

test("a sweep plucks every string it crosses once, in the order it meets them", () => {
  assert.deepEqual(crossed(0, 84, LANES), [0, 1, 2, 3]);
  assert.deepEqual(crossed(84, 0, LANES), [3, 2, 1, 0]);
});

test("a sweep split across pointer events still plucks each string exactly once", () => {
  // A string exactly under one sample must not ring on both of its moves.
  const down = [0, 22, 30, 36, 60, 84];
  const hits = down.slice(1).flatMap((y, i) => crossed(down[i], y, LANES));
  assert.deepEqual(hits, [0, 1, 2, 3]);
  const up = [...down].reverse();
  assert.deepEqual(up.slice(1).flatMap((y, i) => crossed(up[i], y, LANES)), [3, 2, 1, 0]);
});

test("wobbling between two strings plucks nothing", () => {
  assert.deepEqual(crossed(40, 47, LANES), []);
  assert.deepEqual(crossed(47, 40, LANES), []);
});

test("a released string starts fully displaced and dies away", () => {
  assert.equal(ring(1, 0, 8, 0.6).offset, 1);
  const later = ring(1, 3, 8, 0.6);
  assert.ok(later.envelope < 0.01, `still ringing after 3s: ${later.envelope}`);
  assert.ok(Math.abs(later.offset) <= later.envelope);
});
