import { createRoot } from "react-dom/client";
import { ProjectTrustNotice, type ProjectTrustReview } from "../../src/ProjectTrustNotice";
import "@fontsource/anek-latin/latin-400.css";
import "@fontsource/anek-latin/latin-500.css";
import "@fontsource/fragment-mono/latin-400.css";
import "../../src/styles/index.css";

const untrusted: ProjectTrustReview = { root: "/Users/alex/Projects/atlas", folder: "/Users/alex/Projects/atlas", trusted: false, fingerprint: "visual-untrusted", settings: [], ignored: [] };
const changed: ProjectTrustReview = { ...untrusted, trusted: true, fingerprint: "visual-changed", settings: [
  { key: "memory.write_approval", value: "none", effect: "Allows personal memory writes without approval" },
  { key: "pricing.input_per_mtok", value: "0", effect: "Changes the cost meter's rates" },
], ignored: ["memory.write_approval", "pricing.input_per_mtok"] };
Object.assign(window, { fixtureWorkspace: { api: { settings: async () => ({ data: changed }) } } });

createRoot(document.getElementById("root")!).render(<main className="thread">
  <ProjectTrustNotice review={untrusted} session="visual-untrusted" />
  <ProjectTrustNotice review={changed} session="visual-changed" />
</main>);

Object.assign(window, { fixtureReport: () => {
  for (const animation of document.getAnimations()) {
    if (Number.isFinite(animation.effect?.getComputedTiming().endTime)) animation.finish();
  }
  return {
  kind: "project-trust", fontsLoaded: document.fonts.status === "loaded", theme: document.documentElement.dataset.theme,
  dialogs: document.querySelectorAll('[role="dialog"]').length,
  cards: [...document.querySelectorAll('[role="status"]')].map((node, index) => {
    const bounds = node.getBoundingClientRect();
    const title = node.querySelector("p")!;
    return { state: index === 0 ? "untrusted" : "changed", x: bounds.x, y: bounds.y, width: bounds.width, height: bounds.height,
      text: node.textContent, font: getComputedStyle(title).fontSize, background: getComputedStyle(node).backgroundColor,
      overflow: node.scrollWidth > node.clientWidth, buttons: node.querySelectorAll(".btn-line").length };
  }),
  };
} });
