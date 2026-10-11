// Finds the outputs Medha made inside a reply. The backend renders every fenced
// block as `<pre data-lang="…"><code>escaped</code></pre>` and names a local image as
// `<span class="md-image" data-src="…">`, so an escaped body can never contain the
// tags this splits on.

import type { ArtifactRevision, ArtifactSnapshot, ToolScreen } from "./api";

export type OutputKind =
  | "diagram"
  | "drawing"
  | "page"
  | "slides"
  | "image"
  | "video"
  | "audio"
  | "pdf"
  | "document"
  | "table"
  | "deck"
  | "app";

export type Segment =
  | { type: "html"; html: string }
  | { type: "output"; kind: OutputKind; source: string; sourceIndex: number };

const BLOCK = /<pre data-lang="([^"]*)"><code>([\s\S]*?)<\/code><\/pre>/g;
const ENTITIES: Record<string, string> = {
  "&lt;": "<",
  "&gt;": ">",
  "&quot;": '"',
  "&#39;": "'",
  "&amp;": "&",
};
const EXTENSIONS: Record<string, OutputKind> = {
  png: "image", jpg: "image", jpeg: "image", gif: "image", webp: "image", bmp: "image",
  svg: "drawing", mmd: "diagram", html: "page", htm: "page",
  mp4: "video", m4v: "video", mov: "video", webm: "video",
  mp3: "audio", wav: "audio", m4a: "audio", ogg: "audio", oga: "audio", flac: "audio",
  pdf: "pdf", docx: "document", csv: "table", tsv: "table", xlsx: "table",
  pptx: "deck", key: "deck",
};

export function unescape(text: string): string {
  return text.replace(/&(?:lt|gt|quot|#39|amp);/g, (entity) => ENTITIES[entity]);
}

const MARKS: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

/** Text made safe to place inside markup, as content or as an attribute. */
export const escapeMarkup = (text: string) => text.replace(/[&<>"']/g, (mark) => MARKS[mark]);

export const extensionOf =(path: string) => /\.([a-z0-9]+)$/i.exec(path)?.[1].toLowerCase() ?? "";

/** A whole document is shown; a fragment in a reply is an example and stays code. */
const isDocument = (source: string) => /^\s*(<!doctype html|<html[\s>])/i.test(source);

export function fileReference(href: string): string | undefined {
  let path: string;
  try {
    path = decodeURIComponent(unescape(href));
  } catch {
    return undefined;
  }
  return !/[*:]/.test(path) && EXTENSIONS[extensionOf(path)] ? path : undefined;
}

function found(match: RegExpMatchArray, sourceIndex: number): Segment | undefined {
  const source = unescape(match[2]);
  const kind = match[1] === "mermaid" ? "diagram" : match[1] === "svg" ? "drawing" : match[1] === "html" && isDocument(source) ? (slidesOf(source).slides.length > 1 ? "slides" : "page") : undefined;
  if (!kind) return undefined;
  return { type: "output", kind, source, sourceIndex };
}

export function splitOutputs(html: string): Segment[] {
  const segments: Segment[] = [];
  let from = 0;
  let sourceIndex = 0;
  for (const match of html.matchAll(BLOCK)) {
    const outputs = found(match, sourceIndex++);
    if (!outputs) continue;
    const before = html.slice(from, match.index);
    if (before.trim()) segments.push({ type: "html", html: before });
    segments.push(outputs);
    from = match.index + match[0].length;
  }
  const rest = html.slice(from);
  if (rest.trim() || !segments.length) segments.push({ type: "html", html: rest });
  return segments;
}

export const KIND_LABEL: Record<OutputKind, string> = {
  diagram: "Diagram",
  drawing: "Drawing",
  page: "Page",
  slides: "Slides",
  image: "Image",
  video: "Video",
  audio: "Audio",
  pdf: "PDF",
  document: "Document",
  table: "Table",
  deck: "Slides",
  app: "App",
};

const titled = (text: string) =>
  text.replace(/[-_.]+/g, " ").trim().replace(/^./, (first) => first.toUpperCase());

/** What kind of thing an output is, in words. A server's screen is named for its server. */
export const labelOf = (output: Pick<Output, "kind" | "screen">) =>
  output.screen ? titled(output.screen.server) : KIND_LABEL[output.kind];

/** A saved output a message can be about: the IDs the backend checks, and what to call it. */
export type OutputAbout = { artifact_id: string; revision_id: string; label: string };

/**
 * Only a diagram, drawing or page the chat has saved can gain a version from a
 * message. The label is worded as the backend words it, which has the last say.
 */
export function aboutOf(output: Output | null | undefined): OutputAbout | undefined {
  if (!output?.revisionId || !output.artifactId?.startsWith("inline:")) return undefined;
  const kind = KIND_LABEL[output.kind === "slides" ? "page" : output.kind];
  return {
    artifact_id: output.artifactId,
    revision_id: output.revisionId,
    label: output.name && output.name !== kind ? `${kind}: ${output.name}` : kind,
  };
}

/** A server's screen as an output: named for the tool that drew it. */
export function screenOutput(anchor: string, screen: ToolScreen): Output {
  return { anchor, sourceEvent: screen.source_event, sourceIndex: 0, kind: "app", source: "", name: titled(screen.tool), screen };
}

const HTML_TITLE = [/<title[^>]*>([^<]+)<\/title>/i];
const TITLE: Partial<Record<OutputKind, RegExp[]>> = {
  diagram: [/^\s*---[\s\S]*?^\s*title:\s*(.+?)\s*$[\s\S]*?^\s*---/m, /^\s*accTitle:\s*(.+?)\s*$/m],
  drawing: HTML_TITLE,
  page: HTML_TITLE,
  slides: HTML_TITLE,
};

/** The name its author gave an output, else its kind. */
export function titleOf(kind: OutputKind, source: string): string {
  for (const pattern of TITLE[kind] ?? []) {
    const found = pattern.exec(source)?.[1]?.trim().replace(/^["']|["']$/g, "");
    if (found) return unescape(found).slice(0, 80);
  }
  return KIND_LABEL[kind];
}

/** The line being written, shown under a diagram while it grows. */
export function lastLine(source: string): string {
  const lines = source.split("\n").filter((line) => line.trim());
  return lines[lines.length - 1]?.trim() ?? "";
}

/**
 * A diagram still being written, made drawable: the unfinished last line is left
 * out and every group still open is closed, so it grows line by line.
 */
export function growing(source: string): string {
  const lines = source.split("\n").slice(0, -1);
  let open = 0;
  for (const line of lines) {
    const text = line.trim();
    if (/^subgraph\b/.test(text)) open++;
    else if (text === "end" && open > 0) open--;
  }
  return [...lines, ...Array<string>(open).fill("end")].join("\n");
}

export type Output = {
  anchor: string;
  kind: OutputKind;
  source: string;
  name: string;
  path?: string;
  screen?: ToolScreen;
  artifactId?: string;
  revisionId?: string;
  predecessor?: string | null;
  sourceEvent?: string;
  sourceIndex?: number;
  sourceDigest?: string;
  toolCall?: string;
  sessionId?: string;
  snapshot?: ArtifactSnapshot;
  bundle?: string;
  missing?: string[];
  storedSource?: boolean;
};

/** Backend identity and immutable content, adapted for the desktop renderer. */
export function recordedOutput(record: ArtifactRevision, sessionId: string): Output {
  let kind = record.kind;
  if (kind === "page" && record.source && slidesOf(record.source).slides.length > 1) kind = "slides";
  return { anchor: record.revision_id, artifactId: record.artifact_id, revisionId: record.revision_id, predecessor: record.predecessor,
    sourceEvent: record.source_event, sourceIndex: record.source_index, sourceDigest: record.source_digest, toolCall: record.tool_call, sessionId,
    kind: kind as OutputKind, name: record.source ? titleOf(kind as OutputKind, record.source) : record.name,
    source: record.source ?? "", storedSource: record.inline, snapshot: record.snapshot, path: kind === "app" ? undefined : record.snapshot?.path ?? record.path, screen: record.screen ? { ...record.screen, source_event: record.source_event } : undefined };
}

const DESCRIPTION = /<meta\s+name=["']description["']\s+content=["']([^"']+)["']/i;
const NOTE: Partial<Record<OutputKind, RegExp>> = {
  diagram: /^\s*accDescr:\s*(.+?)\s*$/m,
  drawing: /<desc[^>]*>([^<]+)<\/desc>/i,
  page: DESCRIPTION,
  slides: DESCRIPTION,
};

/** What its author said about this version, in the format's own description field. */
export function noteOf(output: Output): string {
  return unescape(NOTE[output.kind]?.exec(output.source)?.[1]?.trim() ?? "").slice(0, 120);
}

/** Explicit stable identity only. Labels and line similarity never join files. */
export function versionsOf(all: Output[], output: Output): Output[] {
  const chain = all.filter((other) => other.anchor === output.anchor || !!output.artifactId && other.artifactId === output.artifactId);
  return chain.some((other) => other.anchor === output.anchor) ? chain : [...chain, output];
}

/** The newest explicit revision of each output, newest first. */
export function latestOf(all: Output[]): Output[] {
  const seen = new Set<string>();
  return [...all].reverse().filter((output) => {
    const id = output.artifactId ?? output.anchor;
    if (seen.has(id)) return false;
    seen.add(id);
    return true;
  });
}

export const hasScripts = (source: string) => /<script[\s>]/i.test(source);

export type Deck = { head: string; body: string; slides: string[]; tail?: string; notices?: string[]; total?: number };

/** A page whose body is nothing but sections is a deck: one slide per section. */
export function slidesOf(source: string): Deck {
  const none = { head: "", body: "", slides: [] };
  const open = /<body([^>]*)>/i.exec(source);
  if (!open) return none;
  const start = open.index + open[0].length;
  const end = source.search(/<\/body>/i);
  const content = source.slice(start, end < 0 ? undefined : end);
  const slides: string[] = [];
  let depth = 0;
  let from = 0;
  let outside = "";
  let cursor = 0;
  for (const tag of content.matchAll(/<(\/?)section\b[^>]*>/gi)) {
    if (!tag[1] && depth++ === 0) {
      outside += content.slice(cursor, tag.index);
      from = tag.index;
    } else if (tag[1] && depth > 0 && --depth === 0) {
      cursor = tag.index + tag[0].length;
      slides.push(content.slice(from, cursor));
    }
  }
  outside += depth ? "" : content.slice(cursor);
  const loose = outside.replace(/<!--[\s\S]*?-->|<(script|style)\b[\s\S]*?<\/\1>/gi, "").trim();
  if (loose) return none;
  const head = /<head[^>]*>([\s\S]*?)<\/head>/i.exec(source)?.[1] ?? "";
  return { head, body: open[1], slides, tail: outside };
}

/** One slide as a page of its own, filling the frame it is shown in. */
export function slidePage(deck: Deck, index: number): string {
  const fill = "<style>html,body{height:100%;margin:0}body>section{box-sizing:border-box;min-height:100%}</style>";
  return `<!doctype html><html><head><meta charset="utf-8">${deck.head}${fill}</head><body${deck.body}>${deck.slides[index] ?? ""}${deck.tail ?? ""}</body></html>`;
}

export function timeText(seconds: number): string {
  if (!Number.isFinite(seconds)) return "";
  const whole = Math.round(seconds);
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, "0")}`;
}

export function sizeText(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1).replace(/\.0$/, "")} ${units[unit]}`;
}
