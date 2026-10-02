import DOMPurify from "dompurify";

export type Drawn = { svg: string; paper?: boolean } | { problem: string };

type Mermaid = typeof import("mermaid").default;
let loading: Promise<Mermaid> | undefined;
let serial = 0;

const TOKENS = ["overlay", "text", "dim", "line", "gold", "gold-wash", "hover"] as const;

// A diagram that brings its own colours was drawn for a light page, so in every
// theme it is set on paper with these inks, as a drawing is.
const PAPER: Record<string, string> = {
  canvas: "#fffdf8",
  overlay: "#fffdf8",
  text: "#2b261e",
  dim: "#6b6253",
  line: "#cfc7b6",
  gold: "#8c5c0e",
  "gold-wash": "#f3e9d3",
  hover: "#f4efe6",
};
const OWN_COLOURS = /^\s*(classDef|style)\b[^\n]*\bfill\s*:|%%\{\s*init/m;

function channels(css: string): number[] | undefined {
  const numbers = css.match(/-?\d*\.?\d+(?:e-?\d+)?/g)?.map(Number);
  if (!numbers || numbers.length < 3) return undefined;
  const scale = css.startsWith("color(") ? 255 : 1;
  return [numbers[0] * scale, numbers[1] * scale, numbers[2] * scale, numbers[3] ?? 1];
}

// Mermaid's theme cannot read `color-mix()` or translucent tokens, so each one
// is resolved by the browser and flattened over the canvas colour.
function palette(paper: boolean): Record<string, string> {
  const probe = document.createElement("span");
  probe.hidden = true;
  document.body.append(probe);
  const read = (token: string) => {
    probe.style.color = `var(--${token})`;
    return channels(getComputedStyle(probe).color);
  };
  const base = read("canvas") ?? [128, 128, 128, 1];
  const hex = (token: string) => {
    const [r, g, b, a] = read(token) ?? base;
    return `#${[r, g, b]
      .map((value, i) => Math.round(value * a + base[i] * (1 - a)))
      .map((value) => Math.min(255, Math.max(0, value)).toString(16).padStart(2, "0"))
      .join("")}`;
  };
  const colour = paper
    ? PAPER
    : Object.fromEntries([...TOKENS, "canvas"].map((token) => [token, hex(token)]));
  const font = getComputedStyle(document.body).fontFamily;
  probe.remove();
  return {
    background: colour.canvas,
    primaryColor: colour.overlay,
    mainBkg: colour.overlay,
    primaryTextColor: colour.text,
    textColor: colour.text,
    titleColor: colour.text,
    primaryBorderColor: colour.line,
    nodeBorder: colour.line,
    lineColor: colour.dim,
    secondaryColor: colour.hover,
    tertiaryColor: colour.canvas,
    clusterBkg: colour.canvas,
    clusterBorder: colour.line,
    edgeLabelBackground: colour.canvas,
    noteBkgColor: colour["gold-wash"],
    noteTextColor: colour.text,
    noteBorderColor: colour.gold,
    fontFamily: font,
    fontSize: "14px",
  };
}

/** Keeps only rules confined to the diagram: its source may carry CSS of its own. */
function confine(svg: string, id: string, accent: string, inks: string[]): string {
  const parsed = new DOMParser().parseFromString(svg, "image/svg+xml");
  const scope = `#${id}`;
  const scoped = (selector: string) =>
    selector.startsWith(scope) && !/[\w-]/.test(selector[scope.length] ?? "");
  for (const style of parsed.querySelectorAll("style")) {
    const sheet = new CSSStyleSheet();
    try {
      sheet.replaceSync(style.textContent ?? "");
    } catch {
      style.remove();
      continue;
    }
    style.textContent = [...sheet.cssRules]
      .filter((rule) =>
        rule instanceof CSSStyleRule
          ? rule.selectorText.split(",").every((part) => scoped(part.trim()))
          : rule instanceof CSSKeyframesRule,
      )
      .map((rule) => rule.cssText)
      .concat(
        `${scope} .edgeLabel rect { opacity: 1; }`,
        `${scope} .edgeLabel.above rect { opacity: 0; }`,
        // A node's first line is its name; any line under it is a quieter note.
        `${scope} .node .label .text-outer-tspan:first-child .text-inner-tspan { font-weight: 600; }`,
        `${scope} .node .label .text-outer-tspan ~ .text-outer-tspan { font-size: 0.85em; }`,
        `${shapes(`${scope} .node`)} { stroke-width: 1.2px; }`,
        `${shapes(`${scope} .node.accent`)} { stroke: ${accent}; stroke-width: 1.6px; }`,
        `${scope} .flowchart-link { stroke-width: 1.4px; }`,
        `${scope} .flowchart-link.accent { stroke: ${accent}; }`,
      )
      .join("\n");
  }
  for (const node of parsed.querySelectorAll(".node rect")) {
    node.setAttribute("rx", "8");
    node.setAttribute("ry", "8");
  }
  lift(parsed);
  mark(parsed, accent);
  legible(parsed, inks);
  natural(parsed);
  return new XMLSerializer().serializeToString(parsed.documentElement);
}

function brightness(colour: string): number | undefined {
  const probe = document.createElement("span");
  probe.hidden = true;
  probe.style.color = colour;
  if (!probe.style.color) return undefined;
  document.body.append(probe);
  const found = channels(getComputedStyle(probe).color);
  probe.remove();
  if (!found || found[3] < 0.5) return undefined;
  return (0.299 * found[0] + 0.587 * found[1] + 0.114 * found[2]) / 255;
}

/** A node its author coloured keeps that colour; its words take whichever ink can be read on it. */
function legible(svg: Document, inks: string[]) {
  const tones = inks.map((ink) => ({ ink, light: brightness(ink) ?? 0 }));
  for (const node of svg.querySelectorAll(".node")) {
    const shape = node.querySelector("rect, polygon, path, circle, ellipse");
    const fill = /(?:^|;)\s*fill:\s*([^;!]+)/.exec(shape?.getAttribute("style") ?? "")?.[1];
    const light = fill ? brightness(fill.trim()) : undefined;
    if (light === undefined) continue;
    const best = tones.reduce((a, b) => (Math.abs(b.light - light) > Math.abs(a.light - light) ? b : a));
    for (const words of node.querySelectorAll("text, tspan"))
      words.setAttribute("style", `${words.getAttribute("style") ?? ""};fill: ${best.ink} !important`);
  }
}

/** Gives the drawing its natural size, so it can be fitted like a picture instead of stretched. */
function natural(svg: Document) {
  const root = svg.documentElement;
  const [, , width, height] = (root.getAttribute("viewBox") ?? "").split(/[\s,]+/).map(Number);
  if (!width || !height) return;
  root.setAttribute("width", String(Math.ceil(width)));
  root.setAttribute("height", String(Math.ceil(height)));
  root.removeAttribute("style");
}

const shapes = (node: string) =>
  ["rect", "path", "polygon", "circle"].map((shape) => `${node} ${shape}`).join(", ");

/** A label on a line that runs sideways sits above it, clear of the stroke. */
function lift(svg: Document) {
  const links = [...svg.querySelectorAll("path.flowchart-link")];
  const labels = [...svg.querySelectorAll(".edgeLabels > .edgeLabel")];
  if (links.length !== labels.length) return;
  labels.forEach((label, index) => {
    let points: { x: number; y: number }[];
    try {
      points = JSON.parse(atob(links[index].getAttribute("data-points") ?? ""));
    } catch {
      return;
    }
    const [from, to] = [points[0], points.at(-1)];
    if (!from || !to || Math.abs(to.x - from.x) <= Math.abs(to.y - from.y)) return;
    label.classList.add("above");
    label.setAttribute("transform", `${label.getAttribute("transform") ?? ""} translate(0 -12)`);
  });
}

/** `:::accent` marks the part of a diagram that matters; the lines touching it follow. */
function mark(svg: Document, colour: string) {
  const marked = [...svg.querySelectorAll(".node.accent")].map((node) =>
    (node.getAttribute("id") ?? "").replace(/^.*?flowchart-/, "").replace(/-\d+$/, ""),
  );
  if (!marked.length) return;
  for (const link of svg.querySelectorAll("path.flowchart-link")) {
    // A link is named for its ends: `L_<from>_<to>_<n>`.
    const name = (link.getAttribute("id") ?? "").replace(/^.*?-L_/, "L_");
    const touches = marked.some(
      (id) => name.startsWith(`L_${id}_`) || new RegExp(`_${id}_\\d+$`).test(name),
    );
    const head = /url\(#([^)]+)\)/.exec(link.getAttribute("marker-end") ?? "")?.[1];
    if (!touches) continue;
    link.classList.add("accent");
    const marker = head && svg.getElementById(head);
    if (!marker) continue;
    const gold = marker.cloneNode(true) as Element;
    gold.setAttribute("id", `${head}-accent`);
    for (const shape of gold.querySelectorAll("path, circle"))
      shape.setAttribute("style", `fill: ${colour}; stroke: ${colour};`);
    if (!svg.getElementById(`${head}-accent`)) marker.after(gold);
    link.setAttribute("marker-end", `url(#${head}-accent)`);
  }
}

/** The caption carries the title, so the drawing does not repeat it; blanked in place to keep line numbers. */
function untitled(source: string): string {
  const front = /^\s*---[^\n]*\n[\s\S]*?\n\s*---/.exec(source)?.[0];
  if (!front) return source;
  return front.replace(/^(\s*title:).*$/m, '$1 ""') + source.slice(front.length);
}

function problemOf(cause: unknown, source: string): string {
  const text = cause instanceof Error ? cause.message : String(cause);
  const line = Number(/line (\d+)/i.exec(text)?.[1]);
  if (!line) return "Its source could not be read.";
  return line > source.trimEnd().split("\n").length
    ? "Its last line is unfinished."
    : `Line ${line} could not be read.`;
}

export async function drawDiagram(source: string, dark: boolean): Promise<Drawn> {
  loading ??= import("mermaid").then((module) => module.default);
  const mermaid = await loading.catch(() => undefined);
  if (!mermaid) {
    loading = undefined;
    return { problem: "The diagram engine could not be loaded." };
  }
  const paper = OWN_COLOURS.test(source);
  const colours = palette(paper);
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    theme: "base",
    darkMode: dark,
    htmlLabels: false,
    flowchart: { htmlLabels: false, padding: 18, nodeSpacing: 44, rankSpacing: 56 },
    themeVariables: colours,
  });
  const id = `out-diagram-${++serial}`;
  try {
    const body = untitled(source);
    await mermaid.parse(body);
    const { svg } = await mermaid.render(id, body);
    const clean = DOMPurify.sanitize(svg, { USE_PROFILES: { svg: true, svgFilters: true } });
    const inks = [colours.textColor, colours.background];
    return { svg: confine(clean, id, colours.noteBorderColor, inks), paper };
  } catch (cause) {
    return { problem: problemOf(cause, source) };
  } finally {
    document.getElementById(id)?.remove();
    document.getElementById(`d${id}`)?.remove();
  }
}

/** An SVG shown as an image can run no script, load nothing, and style nothing outside itself. */
export function drawingUrl(source: string): string | undefined {
  const start = source.search(/<svg[\s>]/i);
  if (start < 0) return undefined;
  const body = source.slice(start);
  const named = /^<svg[^>]*\sxmlns=/i.test(body)
    ? body
    : body.replace(/^<svg/i, '<svg xmlns="http://www.w3.org/2000/svg"');
  return URL.createObjectURL(new Blob([named], { type: "image/svg+xml" }));
}
