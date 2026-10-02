// Reads a PowerPoint file into a deck Medha can show: each slide becomes a
// section of plain, script-free markup that the locked frame draws. Every word
// and colour from the file is escaped or checked; nothing in it is run.

import JSZip from "jszip";
import { escapeMarkup, type Deck } from "./outputBlocks";
import { chartMarkup } from "./outputDeckChart";

const RELS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const MAX_SLIDES = 200;
const MAX_PART = 4 * 1024 * 1024;
const MAX_PICTURE = 3 * 1024 * 1024;
const FRAME = { width: 960, height: 540 };
const PICTURES: Record<string, string> = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif" };
const SIZES: Record<string, number> = { title: 40, ctrTitle: 44, subTitle: 24, body: 24, obj: 24 };
const DEFAULT_INKS: Record<string, string> = { dk1: "000000", lt1: "ffffff", dk2: "1f1f1f", lt2: "eeeeee" };
const ALIASES: Record<string, string> = { tx1: "dk1", bg1: "lt1", tx2: "dk2", bg2: "lt2" };

type Box = { x: number; y: number; w: number; h: number };
type Parts = { zip: JSZip; cache: Map<string, Promise<Document | undefined>> };

const child =(node: Element | undefined, name: string) =>
  node ? [...node.children].find((other) => other.localName === name) : undefined;
const children = (node: Element | undefined, name: string) =>
  node ? [...node.children].filter((other) => other.localName === name) : [];
const within = (node: Element | undefined, ...names: string[]) =>
  names.reduce<Element | undefined>((at, name) => child(at, name), node);

/** One file from the archive, refused before it is unpacked when it claims to be too large. */
async function unpacked(zip: JSZip, path: string, limit: number, as: "string" | "base64") {
  const file = zip.file(path) as (JSZip.JSZipObject & { _data?: { uncompressedSize?: number } }) | null;
  if (!file || (file._data?.uncompressedSize ?? 0) > limit) return undefined;
  const data = await file.async(as);
  return data.length > limit * (as === "base64" ? 1.4 : 1) ? undefined : data;
}

function part({ zip, cache }: Parts, path: string): Promise<Document | undefined> {
  if (!cache.has(path))
    cache.set(
      path,
      (async () => {
        const text = await unpacked(zip, path, MAX_PART, "string");
        if (!text) return undefined;
        const parsed = new DOMParser().parseFromString(text, "application/xml");
        return parsed.querySelector("parsererror") ? undefined : parsed;
      })(),
    );
  return cache.get(path)!;
}

/** Where a part's relationships point, by id, as paths inside the file. */
async function links(parts: Parts, path: string): Promise<Map<string, string>> {
  const at = path.lastIndexOf("/");
  const rels = await part(parts, `${path.slice(0, at)}/_rels/${path.slice(at + 1)}.rels`);
  const found = new Map<string, string>();
  for (const rel of rels?.documentElement.children ?? []) {
    const target = rel.getAttribute("Target") ?? "";
    const from = target.startsWith("/") ? [] : path.slice(0, at).split("/");
    for (const step of target.split("/")) {
      if (step === "..") from.pop();
      else if (step && step !== ".") from.push(step);
    }
    found.set(rel.getAttribute("Id") ?? "", from.join("/"));
  }
  return found;
}

function colour(fill: Element | undefined, inks: Record<string, string>): string | undefined {
  const exact = child(fill, "srgbClr")?.getAttribute("val");
  const named = child(fill, "schemeClr")?.getAttribute("val") ?? "";
  const hex = exact ?? inks[ALIASES[named] ?? named];
  return hex && /^[0-9a-f]{6}$/i.test(hex) ? `#${hex}` : undefined;
}

function boxOf(shape: Element | undefined): Box | undefined {
  const frame = within(shape, "spPr", "xfrm") ?? child(shape, "xfrm");
  const [off, ext] = [child(frame, "off"), child(frame, "ext")];
  if (!off || !ext) return undefined;
  const box = {
    x: Number(off.getAttribute("x")),
    y: Number(off.getAttribute("y")),
    w: Number(ext.getAttribute("cx")),
    h: Number(ext.getAttribute("cy")),
  };
  return Object.values(box).every(Number.isFinite) ? box : undefined;
}

const holderOf = (shape: Element) =>
  [...shape.children].flatMap((holder) => [...holder.children]).flatMap((props) => children(props, "ph"))[0];

/** A placeholder with no place of its own takes the one its layout, then its master, gives it. */
function inherited(holder: Element, sheets: (Document | undefined)[]): Box | undefined {
  const [index, kind] = [holder.getAttribute("idx"), holder.getAttribute("type") ?? "body"];
  const family = kind === "ctrTitle" ? "title" : kind === "subTitle" || kind === "obj" ? "body" : kind;
  for (const sheet of sheets) {
    const shapes = children(within(sheet?.documentElement, "cSld", "spTree"), "sp");
    const held = shapes.map((shape) => ({ shape, holder: holderOf(shape) })).filter((entry) => entry.holder);
    const match =
      held.find((entry) => index && entry.holder.getAttribute("idx") === index) ??
      held.find((entry) => (entry.holder.getAttribute("type") ?? "body") === kind) ??
      held.find((entry) => (entry.holder.getAttribute("type") ?? "body") === family);
    const box = boxOf(match?.shape);
    if (box) return box;
  }
  return undefined;
}

function words(body: Element | undefined, kind: string | undefined, inks: Record<string, string>, pt: number): string {
  const listed = kind === "body" || kind === "obj";
  return children(body, "p")
    .map((line) => {
      const props = child(line, "pPr");
      const level = Math.min(8, Number(props?.getAttribute("lvl") ?? 0) || 0);
      const base = (SIZES[kind ?? ""] ?? 18) - (listed ? level * 3 : 0);
      const runs = [...line.children]
        .map((run) => {
          if (run.localName === "br") return "<br>";
          if (run.localName !== "r" && run.localName !== "fld") return "";
          const style = child(run, "rPr");
          const size = Number(style?.getAttribute("sz")) / 100 || base;
          const ink = colour(child(style, "solidFill"), inks);
          const css = [
            `font-size:${(size * pt).toFixed(1)}px`,
            style?.getAttribute("b") === "1" ? "font-weight:700" : "",
            style?.getAttribute("i") === "1" ? "font-style:italic" : "",
            ink ? `color:${ink}` : "",
          ];
          return `<span style="${css.filter(Boolean).join(";")}">${escapeMarkup(child(run, "t")?.textContent ?? "")}</span>`;
        })
        .join("");
      const align = { ctr: "center", r: "right", just: "justify" }[props?.getAttribute("algn") ?? ""];
      const bullet = listed && runs && !child(props, "buNone");
      const css = [
        `font-size:${(base * pt).toFixed(1)}px`,
        align ? `text-align:${align}` : "",
        bullet ? `margin-left:${(1.2 + level * 1.4).toFixed(1)}em;display:list-item` : "",
      ];
      return `<p style="${css.filter(Boolean).join(";")}">${runs || "&nbsp;"}</p>`;
    })
    .join("");
}

function table(frame: Element, inks: Record<string, string>, pt: number): string {
  const rows = children(frame.getElementsByTagNameNS("*", "tbl")[0], "tr");
  const cells = rows.map(
    (row, at) =>
      `<tr>${children(row, "tc")
        .map((cell) => `<${at ? "td" : "th"}>${words(child(cell, "txBody"), undefined, inks, pt * 0.8)}</${at ? "td" : "th"}>`)
        .join("")}</tr>`,
  );
  return `<table>${cells.join("")}</table>`;
}

/** What a shape or a slide is filled with: one colour, or a blend of several along a line. */
function fillOf(props: Element | undefined, inks: Record<string, string>): string | undefined {
  const blend = child(props, "gradFill");
  if (!blend) return colour(child(props, "solidFill"), inks);
  const stops = children(child(blend, "gsLst"), "gs")
    .map((stop) => ({ at: Number(stop.getAttribute("pos")) / 1000, ink: colour(stop, inks) }))
    .filter((stop) => stop.ink && Number.isFinite(stop.at))
    .sort((a, b) => a.at - b.at);
  if (stops.length < 2) return stops[0]?.ink;
  // The file measures the angle from pointing right, clockwise, in 60,000ths of a degree.
  const turn = (Number(child(blend, "lin")?.getAttribute("ang")) || 0) / 60000 + 90;
  return `linear-gradient(${turn.toFixed(0)}deg,${stops.map((stop) => `${stop.ink} ${stop.at.toFixed(0)}%`).join(",")})`;
}

/** A frame on a slide holds a table or a chart; a chart is drawn from the numbers stored with it. */
async function framed(
  parts: Parts,
  shape: Element,
  rels: Map<string, string>,
  inks: Record<string, string>,
  accents: string[],
  pt: number,
): Promise<string> {
  const chart = shape.getElementsByTagNameNS("*", "chart")[0];
  if (!chart) return table(shape, inks, pt);
  const numbers = await part(parts, rels.get(chart.getAttributeNS(RELS, "id") ?? "") ?? "");
  return (numbers && chartMarkup(numbers, accents)) ?? '<div class="missing">Chart</div>';
}

async function picture(parts: Parts, shape: Element, rels: Map<string, string>): Promise<string> {
  const id = shape.getElementsByTagNameNS("*", "blip")[0]?.getAttributeNS(RELS, "embed");
  const path = rels.get(id ?? "") ?? "";
  const type = PICTURES[path.split(".").at(-1)?.toLowerCase() ?? ""];
  const data = type ? await unpacked(parts.zip, path, MAX_PICTURE, "base64") : undefined;
  if (!data) return '<div class="missing">Picture</div>';
  return `<img alt="" src="data:${type};base64,${data}">`;
}

/** The slides of a PowerPoint file as a deck: one section each, drawn to the file's own proportions. */
export async function readDeck(bytes: Uint8Array): Promise<Deck> {
  const parts: Parts = { zip: await JSZip.loadAsync(bytes), cache: new Map() };
  const show = await part(parts, "ppt/presentation.xml");
  if (!show) throw new Error("This is not a PowerPoint file Medha can read.");
  const size = child(show.documentElement, "sldSz");
  const sheet = { w: Number(size?.getAttribute("cx")) || 12192000, h: Number(size?.getAttribute("cy")) || 6858000 };
  const fit = Math.min(FRAME.width / sheet.w, FRAME.height / sheet.h);
  // A point is 12,700 of the file's units; type keeps its size relative to the sheet.
  const pt = fit * 12700;
  const showLinks = await links(parts, "ppt/presentation.xml");
  const order = children(child(show.documentElement, "sldIdLst"), "sldId")
    .map((slide) => showLinks.get(slide.getAttributeNS(RELS, "id") ?? "") ?? "")
    .filter(Boolean)
    .slice(0, MAX_SLIDES);

  const slides: string[] = [];
  for (const path of order) {
    const slide = await part(parts, path);
    if (!slide) continue;
    const rels = await links(parts, path);
    const layoutPath = [...rels.values()].find((target) => target.includes("slideLayout")) ?? "";
    const layout = await part(parts, layoutPath);
    const masterPath = [...(await links(parts, layoutPath)).values()].find((target) => target.includes("slideMaster")) ?? "";
    const master = await part(parts, masterPath);
    const themePath = [...(await links(parts, masterPath)).values()].find((target) => target.includes("theme")) ?? "";
    const scheme = (await part(parts, themePath))?.getElementsByTagNameNS("*", "clrScheme")[0];
    const inks = { ...DEFAULT_INKS };
    for (const entry of scheme?.children ?? []) {
      const value = entry.firstElementChild;
      const hex = value?.getAttribute("val") ?? "";
      inks[entry.localName] = /^[0-9a-f]{6}$/i.test(hex) ? hex : (value?.getAttribute("lastClr") ?? inks[entry.localName] ?? "000000");
    }
    const sheets = [slide, layout, master];
    const ground = sheets.map((doc) => fillOf(within(doc?.documentElement, "cSld", "bg", "bgPr"), inks)).find(Boolean);
    const accents = [1, 2, 3, 4, 5, 6].map((at) => `#${inks[`accent${at}`] ?? "888888"}`);

    const boxes: string[] = [];
    for (const shape of within(slide.documentElement, "cSld", "spTree")?.children ?? []) {
      if (!["sp", "pic", "graphicFrame"].includes(shape.localName)) continue;
      const holder = holderOf(shape);
      const kind = holder ? (holder.getAttribute("type") ?? "body") : undefined;
      if (kind && ["dt", "ftr", "sldNum"].includes(kind)) continue;
      const box = boxOf(shape) ?? (holder ? inherited(holder, [layout, master]) : undefined);
      if (!box) continue;
      const props = child(shape, "spPr");
      const fill = fillOf(props, inks);
      // A shape's style names the ink its words take when they name none themselves.
      const ink = colour(within(shape, "style", "fontRef"), inks);
      const round = child(props, "prstGeom")?.getAttribute("prst");
      const anchor = child(child(shape, "txBody"), "bodyPr")?.getAttribute("anchor") ?? (kind?.includes("itle") ? "ctr" : "t");
      const css = [
        `left:${((box.x / sheet.w) * 100).toFixed(3)}%`,
        `top:${((box.y / sheet.h) * 100).toFixed(3)}%`,
        `width:${((box.w / sheet.w) * 100).toFixed(3)}%`,
        `height:${((box.h / sheet.h) * 100).toFixed(3)}%`,
        `justify-content:${{ ctr: "center", b: "flex-end" }[anchor] ?? "flex-start"}`,
        fill ? `background:${fill}` : "",
        ink ? `color:${ink}` : "",
        round === "ellipse" ? "border-radius:50%" : round === "roundRect" ? "border-radius:12px" : "",
        kind === "ctrTitle" || kind === "subTitle" ? "text-align:center" : "",
      ];
      const inside =
        shape.localName === "pic"
          ? await picture(parts, shape, rels)
          : shape.localName === "graphicFrame"
            ? await framed(parts, shape, rels, inks, accents, pt)
            : words(child(shape, "txBody"), kind, inks, pt);
      const heading = kind === "title" || kind === "ctrTitle";
      const role = shape.localName === "sp" ? (heading ? " title" : "") : " whole";
      boxes.push(`<div class="box${role}" style="${css.filter(Boolean).join(";")}">${inside}</div>`);
    }
    const width = sheet.w * fit;
    const height = sheet.h * fit;
    slides.push(
      `<section style="background:${ground ?? `#${inks.lt1}`};color:#${inks.dk1}"><div class="sheet" style="width:${width.toFixed(1)}px;height:${height.toFixed(1)}px">${boxes.join("")}</div></section>`,
    );
  }
  if (!slides.length) throw new Error("This file has no slides Medha can read.");
  return { head: STYLE, body: "", slides };
}

const STYLE = `<style>
section{display:grid;place-items:center;height:100%;overflow:hidden;font-family:system-ui,-apple-system,"Segoe UI",sans-serif;line-height:1.2}
.sheet{position:relative}
.box{position:absolute;display:flex;flex-direction:column;box-sizing:border-box;padding:.3em .5em;overflow:hidden}
.box p{margin:0 0 .25em}
.box.title{font-weight:600}
.box.whole{padding:0}
.box img{width:100%;height:100%;object-fit:contain}
.box table{width:100%;height:100%;border-collapse:collapse}
.box :is(td,th){border:1px solid #8886;padding:.2em .5em;text-align:left;vertical-align:middle}
.box th{background:#8882}
.missing{display:grid;place-items:center;height:100%;border:1px dashed #8888;opacity:.6}
</style>`;
