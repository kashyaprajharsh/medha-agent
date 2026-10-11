// Use production components, CSS, Mermaid and sanitization. Only the native
// transport/workspace context is substituted; no model or user state is used.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { Plate } from "../../src/OutputPlate";
import { Rendered } from "../../src/OutputRendered";
import { Preview } from "../../src/OutputPreview";
import { OutputsProvider, SHOWN_ONLY } from "../../src/outputContext";
import "@fontsource/anek-latin/latin-400.css";
import "@fontsource/anek-latin/latin-500.css";
import "@fontsource/anek-latin/latin-600.css";
import "@fontsource/anek-latin/latin-700.css";
import "@fontsource/anek-latin/latin-ext-400.css";
import "@fontsource/fragment-mono/latin-400.css";
import "../../src/styles/index.css";

const graph = 'flowchart TD\n A["Input records"] --> B["Process requests"]\n B --> C{"Valid?"}\n C -->|Yes| D["Completed"]\n C -->|No| B';
const cases = [
  ["Default SVG labels", graph],
  ["Authored colours", `${graph}\n style A fill:#e6f4ea,stroke:#238b45`],
  ["Root HTML-label override", `%%{init: {"htmlLabels": true}}%%\n${graph}`],
  ["Nested HTML-label override", `%%{init: {"flowchart":{"htmlLabels":true,"curve":"linear"}}}%%\n${graph}`],
  ["Frontmatter HTML-label override", `---\nconfig:\n  htmlLabels: true\n---\n${graph}`],
  ["ReAct loop with authored colours", 'flowchart TD\n subgraph Agent["ReAct Agent Loop"]\n Input[("User Request / Context")]\n Thought["Thought: reason about the next step"]\n Decision{Done?}\n Action["Action: Tool Call"]\n Observation["Observation: Tool Result"]\n Answer[("Final Answer")]\n end\n Input --> Thought\n Thought --> Decision\n Decision -- No --> Action\n Action --> Observation\n Observation --> Thought\n Decision -- Yes --> Answer\n classDef core fill:#f5f5f5,stroke:#333,stroke-width:2px;\n classDef term fill:#e8f5e9,stroke:#2e7d32,stroke-width:2px;\n class Thought,Action,Observation,Decision core;\n class Input,Answer term;'],
  ["Corrected agent flowchart", 'flowchart TD\n A[Start] --> B[Perceive / Observe Environment]\n B --> C[Reason: LLM thinks about what to do]\n C --> D{Decide Action?}\n D -->|Yes| E[Select Tool / Action]\n D -->|No| F[Finish / Return Answer]\n E --> G[Execute Action - call tool, API etc.]\n G --> H[Update Memory / State]\n H --> B\n F --> I[End]'],
  ["ReAct clusters, authored classes and emoji labels", String.raw`flowchart TD
    UserInput[("User Input / Question")]
    subgraph Agent["🤖 React Agent Loop"]
        direction TB
        Observe[["👁️ Observe\n(Read context, history,\ntool results)"]]
        Think[["🧠 Think / Reason\n(Analyze, plan, decide\nnext action)"]]
        Act[["⚡ Act\n(Call tool, produce\noutput, or respond)"]]
        Observe --> Think
        Think --> Act
        Act --> Observe
    end
    subgraph Tools["🔧 Available Tools"]
        direction TB
        Search[("🔍 Search / Lookup")]
        Compute[("🧮 Calculate / Code")]
        ReadFile[("📖 Read File")]
        WriteFile[("✍️ Write File")]
        API[("🌐 API Call")]
        Browser[("🌍 Browse Web")]
    end
    subgraph State["💾 Agent State"]
        direction TB
        History[("📜 Conversation\nHistory")]
        Scratchpad[("📝 Scratchpad /\nWorking Memory")]
        Knowledge[("🧠 Knowledge\nBase")]
    end
    UserInput --> Observe
    Act -->|"Tool Call"| Tools
    Tools -->|"Tool Result"| Observe
    Observe <-->|"Read/Write"| State
    Think <-->|"Read/Write"| State
    Act -->|"Final Answer"| FinalOutput[("✅ Final Response\nto User")]
    classDef agentNode fill:#d0bfff,stroke:#8b5cf6,stroke-width:2px,color:#1e1e1e
    classDef toolNode fill:#ffd8a8,stroke:#f59e0b,stroke-width:2px,color:#1e1e1e
    classDef stateNode fill:#c3fae8,stroke:#22c55e,stroke-width:2px,color:#1e1e1e
    classDef ioNode fill:#a5d8ff,stroke:#4a9eed,stroke-width:2px,color:#1e1e1e
    class Observe,Think,Act agentNode
    class Search,Compute,ReadFile,WriteFile,API,Browser toolNode
    class History,Scratchpad,Knowledge stateNode
    class UserInput,FinalOutput ioNode`],
];
const outputs = cases.map(([name, source], index) => ({ anchor: `case-${index}`, artifactId: `fixture-${index}`, revisionId: `case-${index}`, kind: "diagram" as const, name, source }));
const errors: string[] = [];
addEventListener("error", event => errors.push(event.message));
addEventListener("unhandledrejection", event => errors.push(String(event.reason)));

function App() {
  const [theme, setTheme] = useState("dark");
  const [open, setOpen] = useState(outputs[outputs.length - 1]);
  document.documentElement.dataset.theme = theme;
  Object.assign(window, { fixtureWorkspace: { theme, look: "soft", active: true, api: { openFile: async () => {}, saveOutput: async () => false }, scope: { current: { chatKey: null } } } });
  return <OutputsProvider value={{ ...SHOWN_ONLY, all: outputs, open, viewing: true, show: setOpen, follow: setOpen }}>
    <header className="fixture-head"><b>Production artifact components</b><button id="theme" onClick={() => setTheme(theme === "dark" ? "light" : "dark")}>Change theme</button></header>
    <main className="fixture-grid">
      <section className="fixture-cases">{outputs.map((output, index) => <article key={output.anchor}>
        <h2>{output.name}</h2><Plate {...output} writing={false} order={index} />
        <div className="out-thumb fixture-thumb"><Rendered output={output} size="thumb" /></div>
      </article>)}</section>
      <section className="fixture-viewer"><Preview /></section>
    </main>
  </OutputsProvider>;
}
createRoot(document.querySelector("#checks")!).render(<App />);

function surface(root: Element | null) {
  const svg = root?.querySelector("svg");
  const labels = [...(svg?.querySelectorAll("text") ?? [])].filter(node => node.textContent?.trim()).map(node => {
    const bounds = node.getBBox();
    const style = getComputedStyle(node);
    const visible = [node, ...node.querySelectorAll("tspan")].every(part => {
      const ink = getComputedStyle(part).fill;
      if (ink === "none" || ink === "transparent" || /rgba\([^)]*,\s*0\)$/.test(ink)) return false;
      for (let ancestor: Element | null = part; ancestor; ancestor = ancestor.parentElement) {
        const css = getComputedStyle(ancestor);
        if (css.display === "none" || css.visibility !== "visible" || Number(css.opacity) === 0) return false;
      }
      return true;
    });
    const rect = node.getBoundingClientRect();
    return { text: node.textContent, width: bounds.width, height: bounds.height, screenWidth: rect.width, screenHeight: rect.height, visible, fill: style.fill, opacity: style.opacity, font: style.fontSize };
  });
  return { id: svg?.id, labels, foreign: svg?.querySelectorAll("foreignObject").length ?? 0 };
}
const incidentLabels = ["React Agent Loop", "Available Tools", "Agent State", "User Input / Question", "Observe", "Think / Reason", "⚡ Act", "Search / Lookup", "Calculate / Code", "Read File", "Write File", "API Call", "Browse Web", "Conversation", "Scratchpad", "Knowledge", "Final Response", "Tool Call", "Tool Result", "Read/Write", "Final Answer"];
Object.assign(window, { fixtureReport: () => {
  // A hidden WKWebView does not advance paint animations reliably. Inspect the
  // settled view explicitly, including all ancestors and nested SVG text spans.
  for (const animation of document.getAnimations()) {
    if (Number.isFinite(animation.effect?.getComputedTiming().endTime)) animation.finish();
  }
  return {
    fontsLoaded: document.fonts.status === "loaded",
    theme: document.documentElement.dataset.theme, errors,
    checks: [...document.querySelectorAll("article")].map(article => ({ name: article.querySelector("h2")?.textContent, plate: surface(article.querySelector(".out-plate")), thumbnail: surface(article.querySelector(".fixture-thumb")) })),
    incidentLabels,
    viewer: surface(document.querySelector(".out-viewer .out-diagram")),
  };
} });
