import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { Worker } from "node:worker_threads";
import { once } from "node:events";
import ts from "typescript";
import hljs from "highlight.js/lib/common";

// Execute the actual worker source off the main thread. The small adapter maps
// browser postMessage/onmessage to Node's worker-thread transport for this test.
async function moduleUrl(name, imports) {
  const source = await readFile(new URL(`../src/${name}.ts`, import.meta.url), "utf8");
  let { outputText } = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
  });
  for (const [from, to] of Object.entries(imports))
    outputText = outputText.replace(`"${from}"`, JSON.stringify(to));
  return `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`;
}
const highlightUrl = await moduleUrl("highlight", {
  "highlight.js/lib/common": import.meta.resolve("highlight.js/lib/common"),
});
const { highlightCode } = await import(highlightUrl);
const url = await moduleUrl("syntax.worker", { "./highlight": highlightUrl });

test("an unnamed block is coloured only on a confident guess; unknown names stay plain", () => {
  const plain = (html) => !html.includes("hljs-");
  assert.ok(plain(highlightCode("plain fenced code without a language")), "prose is not CSS");
  assert.ok(plain(highlightCode("submission.zip\n├── agent.yaml\n└── configs/")), "a tree is not Python");
  assert.ok(plain(highlightCode("fn main() {}", "output")), "an unknown name is not guessed");
  assert.equal(highlightCode("a < b && \"c\"", "output"), "a &lt; b &amp;&amp; &quot;c&quot;");
  assert.ok(highlightCode("fn main() {}", "rust").includes("hljs-keyword"), "a named language is trusted");
  const js = 'const x = await fetch(url);\nif (!x.ok) throw new Error("bad");\nexport default function f() { return x; }';
  assert.ok(highlightCode(js).includes("hljs-keyword"), "real code still gets colour");
});

test("large automatic syntax highlighting runs off-thread and retains escaped coloured output", async (t) => {
  const worker = new Worker(
    `
    const {parentPort}=require('node:worker_threads');
    globalThis.self={postMessage: data=>parentPort.postMessage(data)};
    (async()=>{
      await import(${JSON.stringify(url)});
      parentPort.on('message',data=>self.onmessage({data}));
      parentPort.postMessage({ready:true});
    })();
  `,
    { eval: true },
  );
  t.after(() => worker.terminate());
  const [ready] = await once(worker, "message");
  assert.equal(ready.ready, true);
  const text =
    "def f(items):\n    return [x * 2 for x in items if x > 0]\n".repeat(1000);
  const expected = hljs.highlightAuto(text).value;
  let ticks = 0;
  const timer = setInterval(() => ticks++, 1);
  try {
    const result = once(worker, "message");
    worker.postMessage({ id: 1, text });
    const [message] = await result;
    assert.equal(message.html, expected);
    assert.ok(message.html.includes("hljs-keyword"));
    assert.ok(
      ticks > 0,
      "the main event loop progresses while the worker highlights",
    );
  } finally {
    clearInterval(timer);
  }
  const result = once(worker, "message");
  worker.postMessage({
    id: 2,
    text: 'print("<script>bad()</script>")',
    language: "python",
  });
  const [message] = await result;
  assert.ok(!message.html.includes("<script>"));
  assert.ok(message.html.includes("&lt;script&gt;"));
});
