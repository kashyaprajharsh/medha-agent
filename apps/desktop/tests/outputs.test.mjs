import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../src/outputBlocks.ts", import.meta.url), "utf8");
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
});
const { splitOutputs, titleOf, lastLine, slidesOf, slidePage, versionsOf, latestOf, noteOf, growing, mentionsOf } = await import(
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
    { type: "output", kind: "diagram", source: 'A --> B["x &lt; y"]' },
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
  assert.match(slidePage(read, 1), /<style>s\{\}<\/style>.*<body class="d"><section>2<\/section><\/body>/);
  assert.equal(slidesOf("<body><section>1</section><p>loose</p><section>2</section></body>").slides.length, 0);
  // A deck still being written shows the slides that have arrived.
  assert.equal(slidesOf("<body><section>1</section><section>unfinished").slides.length, 1);
});

test("only a local file with a kind Medha can show becomes an output", () => {
  const image = (path, alt = "") => `<span class="md-image" data-src="${path}">${alt}</span>`;
  assert.deepEqual(splitOutputs(`<p>${image("out/a%20b.png", "Load")}\n${image("run.mp4")}</p>`), [
    { type: "output", kind: "image", source: "", path: "out/a b.png", name: "Load" },
    { type: "output", kind: "video", source: "", path: "run.mp4", name: "run.mp4" },
  ]);
  assert.deepEqual(splitOutputs('<p><a href="notes/read.pdf">notes</a></p>')[0].kind, "pdf");
  for (const kept of [
    `<p>See ${image("a.png")} here.</p>`,
    `<p>${image("a.png")}${image("main.rs")}</p>`,
    '<p><a href="https://x.example/a.png">a</a></p>',
    '<p><a href="src/main.rs">main</a></p>',
    `<p>${image("%E0%A4%A.png")}</p>`,
  ])
    assert.deepEqual(splitOutputs(kept), [{ type: "html", html: kept }]);
});

test("a file named in passing is a candidate only when Medha could show it", () => {
  const html =
    "<p>Saved at <code>out/report.pdf</code> and <code>deck.pptx</code>; see <code>src/main.rs</code>, " +
    "<code>cargo test</code>, <code>*.png</code>, <code>https://x.example/a.png</code> and <code>out/report.pdf</code>.</p>";
  assert.deepEqual(mentionsOf(html).map((mention) => [mention.kind, mention.path]), [
    ["pdf", "out/report.pdf"],
    ["deck", "deck.pptx"],
  ]);
  const many = Array.from({ length: 9 }, (_, i) => `<code>shot${i}.png</code>`).join(" ");
  assert.equal(mentionsOf(many).length, 4, "a reply that lists many files does not flood the chat");
});

test("a named output gathers its versions; a file and an unnamed one stand alone", () => {
  const diagram = (anchor, body) => ({ anchor, kind: "diagram", source: `---\ntitle: Read path\n---\n${body}`, name: "Read path" });
  const file = (anchor) => ({ anchor, kind: "image", source: "", name: "a.png", path: "a.png" });
  const plain = { anchor: "p", kind: "diagram", source: "A-->B", name: "Diagram" };
  const all = [diagram("1", "A"), file("2"), plain, diagram("4", "B"), file("5")];
  assert.deepEqual(versionsOf(all, all[3]).map((o) => o.anchor), ["1", "4"]);
  assert.deepEqual(versionsOf(all, plain), [plain]);
  assert.deepEqual(versionsOf(all, all[1]), [all[1]]);
  assert.deepEqual(latestOf(all).map((o) => o.anchor), ["5", "4", "p"]);
  assert.equal(noteOf({ ...plain, source: "flowchart LR\n accDescr: Added the pool\n A-->B" }), "Added the pool");

  // Medha rarely names a diagram; a redraw that keeps most of its lines is still a revision.
  const lines = ["flowchart TB", "  Task --> Localizer", "  Localizer --> Tools", "  Tools --> Logger", "  Logger --> Refiner"];
  const drawn = (anchor, source) => ({ anchor, kind: "diagram", source, name: "Diagram" });
  const first = drawn("a", lines.join("\n"));
  const redrawn = drawn("b", [...lines, "  Refiner --> Gate"].join("\n"));
  const unrelated = drawn("c", "sequenceDiagram\n  Alice->>Bob: hello\n  Bob->>Alice: hi\n  Alice->>Bob: bye");
  assert.deepEqual(versionsOf([first, unrelated, redrawn], redrawn).map((o) => o.anchor), ["a", "b"]);
  assert.deepEqual(versionsOf([first, unrelated, redrawn], unrelated), [unrelated]);
});

test("a diagram still being written is drawn from what has arrived", () => {
  const partial = 'flowchart TB\n  subgraph RUN["Runtime"]\n    A --> B\n    subgraph IN\n      C --> D\n    end\n    B --> C[Half a la';
  assert.equal(growing(partial), 'flowchart TB\n  subgraph RUN["Runtime"]\n    A --> B\n    subgraph IN\n      C --> D\n    end\nend');
  assert.equal(growing("flowchart LR\n  A --> B\nend\n  B -"), "flowchart LR\n  A --> B\nend");
});
