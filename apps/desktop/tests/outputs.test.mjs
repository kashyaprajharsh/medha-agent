import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../src/outputBlocks.ts", import.meta.url), "utf8");
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
});
const { splitOutputs, titleOf, lastLine, slidesOf, slidePage, versionsOf, latestOf, noteOf, growing, fileReference } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`
);

const block = (lang, body) => `<pre data-lang="${lang}"><code>${body}</code></pre>`;

test("a reply with nothing drawn is passed through byte for byte", () => {
  const html = `<p>Plain.</p>${block("rust", "let a = 1 &lt; 2;")}<pre><code>bare</code></pre>`;
  assert.deepEqual(splitOutputs(html), [{ type: "html", html }]);
  assert.deepEqual(splitOutputs(""), [{ type: "html", html: "" }]);
});

test("a drawn block leaves the prose around it and is unescaped exactly once", () => {
  const html = `<p>Before.</p>${block("mermaid", "A --&gt; B[&quot;x &amp;lt; y&quot;]")}<p>After.</p>`;
  assert.deepEqual(splitOutputs(html), [
    { type: "html", html: "<p>Before.</p>" },
    { type: "output", kind: "diagram", source: 'A --> B["x &lt; y"]', sourceIndex: 0 },
    { type: "html", html: "<p>After.</p>" },
  ]);
});

test("text that only looks like a closing tag cannot end a block early", () => {
  const escaped = "&lt;svg&gt;&lt;/code&gt;&lt;/pre&gt;&lt;script&gt;x&lt;/script&gt;&lt;/svg&gt;";
  const [only] = splitOutputs(block("svg", escaped));
  assert.equal(only.kind, "drawing");
  assert.equal(only.source, "<svg></code></pre><script>x</script></svg>");
});

test("an output takes the name its author gave it, else its kind", () => {
  assert.equal(titleOf("diagram", "---\ntitle: Read path\n---\nflowchart LR\n A-->B"), "Read path");
  assert.equal(titleOf("diagram", "flowchart LR\n accTitle: Pool\n A-->B"), "Pool");
  assert.equal(titleOf("diagram", "flowchart LR\n A[title: not this]-->B"), "Diagram");
  assert.equal(titleOf("drawing", '<svg><title>Pool sizes</title><rect/></svg>'), "Pool sizes");
  assert.equal(titleOf("drawing", "<svg><rect/></svg>"), "Drawing");
  assert.equal(titleOf("drawing", `<svg><title>${"x".repeat(200)}</title></svg>`).length, 80);
});

test("the line being written is the last one with text", () => {
  assert.equal(lastLine("flowchart LR\n  A --> B\n\n"), "A --> B");
  assert.equal(lastLine(""), "");
});

test("a whole page is shown, a fragment stays code, and a body of sections is a deck", () => {
  const kinds = (body) => splitOutputs(block("html", body)).map((part) => part.kind ?? part.type);
  assert.deepEqual(kinds("&lt;div&gt;an example&lt;/div&gt;"), ["html"]);
  assert.deepEqual(kinds("&lt;!doctype html&gt;&lt;body&gt;&lt;h1&gt;Hi&lt;/h1&gt;&lt;/body&gt;"), ["page"]);
  const deck = "<!doctype html><head><style>s{}</style></head><body class=\"d\"><section>1<section>in</section></section>\n<section>2</section><script>x</script></body>";
  assert.deepEqual(kinds(deck.replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;")), ["slides"]);
  const read = slidesOf(deck);
  assert.deepEqual(read.slides, ["<section>1<section>in</section></section>", "<section>2</section>"]);
  assert.match(slidePage(read, 1), /<style>s\{\}<\/style>.*<body class="d"><section>2<\/section>\s*<script>x<\/script><\/body>/);
  assert.equal(slidesOf("<body><section>1</section><p>loose</p><section>2</section></body>").slides.length, 0);
  // A deck still being written shows the slides that have arrived.
  assert.equal(slidesOf("<body><section>1</section><section>unfinished").slides.length, 1);
});

test("file links and images remain manual references, never inferred outputs", () => {
  const image = (path, alt = "") => `<span class="md-image" data-src="${path}">${alt}</span>`;
  for (const kept of [
    `<p>${image("out/a%20b.png", "Load")}\n${image("run.mp4")}</p>`,
    '<p><a href="notes/read.pdf">notes</a></p>',
    `<p>See ${image("a.png")} here.</p>`,
    `<p>${image("a.png")}${image("main.rs")}</p>`,
    '<p><a href="https://x.example/a.png">a</a></p>',
    '<p><a href="src/main.rs">main</a></p>',
    `<p>${image("%E0%A4%A.png")}</p>`,
  ])
    assert.deepEqual(splitOutputs(kept), [{ type: "html", html: kept }]);
});

test("file mentions are clickable references without a creation claim", () => {
  const html =
    "<p>Saved at <code>out/report.pdf</code> and <code>deck.pptx</code>; see <code>src/main.rs</code>, " +
    "<code>cargo test</code>, <code>*.png</code>, <code>https://x.example/a.png</code> and <code>out/report.pdf</code>.</p>";
  assert.deepEqual(splitOutputs(html), [{ type: "html", html }]);
  assert.equal(fileReference("out/a%20b.pdf"), "out/a b.pdf");
  for (const value of ["*.png", "https://x.example/a.png", "%E0%A4%A.png", "src/main.rs"]) assert.equal(fileReference(value), undefined);
});

test("stable identities preserve all revisions without merging titles or input paths", () => {
  const diagram = (anchor, body) => ({ anchor, artifactId: "flow", kind: "diagram", source: `---\ntitle: Read path\n---\n${body}`, name: "Read path" });
  const file = (anchor) => ({ anchor, artifactId: "generated-file", kind: "image", source: "", name: "a.png", path: "a.png" });
  const plain = { anchor: "p", kind: "diagram", source: "A-->B", name: "Diagram" };
  const all = [diagram("1", "A"), file("2"), plain, diagram("4", "B"), file("5")];
  assert.deepEqual(versionsOf(all, all[3]).map((o) => o.anchor), ["1", "4"]);
  assert.deepEqual(versionsOf(all, plain), [plain]);
  assert.deepEqual(versionsOf(all, all[1]).map((o) => o.anchor), ["2", "5"]);
  assert.deepEqual(latestOf(all).map((o) => o.anchor), ["5", "4", "p"]);
  assert.equal(noteOf({ ...plain, source: "flowchart LR\n accDescr: Added the pool\n A-->B" }), "Added the pool");

  // Similar text and matching generic titles do not supply identity.
  const lines = ["flowchart TB", "  Task --> Localizer", "  Localizer --> Tools", "  Tools --> Logger", "  Logger --> Refiner"];
  const drawn = (anchor, source) => ({ anchor, kind: "diagram", source, name: "Diagram" });
  const first = drawn("a", lines.join("\n"));
  const redrawn = drawn("b", [...lines, "  Refiner --> Gate"].join("\n"));
  const unrelated = drawn("c", "sequenceDiagram\n  Alice->>Bob: hello\n  Bob->>Alice: hi\n  Alice->>Bob: bye");
  assert.deepEqual(versionsOf([first, unrelated, redrawn], redrawn).map((o) => o.anchor), ["b"]);
  assert.deepEqual(versionsOf([first, unrelated, redrawn], unrelated), [unrelated]);
  const revisions = [first, redrawn, unrelated].map((output) => ({ ...output, artifactId: "redraw" }));
  assert.deepEqual(versionsOf(revisions, revisions[2]).map((o) => o.anchor), ["a", "b", "c"], "a complete redraw remains reachable by explicit identity");
  assert.deepEqual(latestOf(revisions).map((o) => o.anchor), ["c"]);
});

test("source indices survive surrounding code blocks, and nothing a reply writes names an output", () => {
  const html = block("rust", "ignored") + block("html", "<html><body>hi</body></html>");
  const output = splitOutputs(html).find((part) => part.type === "output");
  assert.equal(output.sourceIndex, 1);
  assert.equal("artifactId" in output, false);
  // An ID written in the fence, on its opening line or inside, is only text.
  const named = '<pre data-lang="mermaid" data-artifact="other"><code>flowchart LR\n  A --&gt; B</code></pre>';
  assert.equal(splitOutputs(named).some((part) => part.type === "output"), false);
});

test("a diagram still being written is drawn from what has arrived", () => {
  const partial = 'flowchart TB\n  subgraph RUN["Runtime"]\n    A --> B\n    subgraph IN\n      C --> D\n    end\n    B --> C[Half a la';
  assert.equal(growing(partial), 'flowchart TB\n  subgraph RUN["Runtime"]\n    A --> B\n    subgraph IN\n      C --> D\n    end\nend');
  assert.equal(growing("flowchart LR\n  A --> B\nend\n  B -"), "flowchart LR\n  A --> B\nend");
});
