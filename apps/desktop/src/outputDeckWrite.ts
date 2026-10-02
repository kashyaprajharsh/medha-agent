// Writes a deck Medha made as a PowerPoint file. Each slide is laid out as it is
// shown and its text and pictures written where they were, as things PowerPoint
// can edit: with their size, weight, colour and the slide's background.

import JSZip from "jszip";
import { escapeMarkup, slidePage, type Deck } from "./outputBlocks";

const SHEET = { w: 12192000, h: 6858000 };
const INCH = 914400;
const MARGIN = 0.7 * INCH;
const PICTURE = /^data:image\/(png|jpeg|gif);base64,([A-Za-z0-9+/=]+)$/;
const NS =
  'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"';
const HEAD = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n';
const REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const TYPE = "application/vnd.openxmlformats-officedocument.presentationml";

type Line = { text: string; bullet: boolean };
type Picture = { kind: string; data: string };
type Slide = { title: string; lines: Line[]; pictures: Picture[] };
type Look = { size: number; bold?: boolean; italic?: boolean; colour?: string; align?: string };
/** One thing on a slide where it was measured to be, in the file's units. */
type Placed =
  | { box: number[]; lines: Line[]; look: Look }
  | { box: number[]; picture: Picture };
type Measured = { ground?: string; placed: Placed[] };

// A slide is laid out 960 wide, and a PowerPoint sheet is 12,192,000 units wide.
const UNITS = SHEET.w / 960;

// Control characters are not allowed in XML at all, escaped or not.
const legal = (code: number) =>
  code === 9 || code === 10 || code === 13 || (code >= 32 && code <= 0xd7ff) || (code >= 0xe000 && code !== 0xfffe && code !== 0xffff);
const escape = (text: string) =>
  escapeMarkup([...text].filter((char) => legal(char.codePointAt(0) ?? 0)).join(""));

const BLOCKS = "h1,h2,h3,h4,h5,h6,p,li,blockquote,td,th,figcaption,small";
const said = (node: Element) => (node.textContent ?? "").replace(/\s+/g, " ").trim();
const blocksOf = (page: ParentNode) =>
  [...page.querySelectorAll(BLOCKS)].filter((node) => said(node) && !node.querySelector("h1,h2,h3,h4,h5,h6,p,li"));
const pictureOf = (image: Element): Picture | undefined => {
  const found = PICTURE.exec(image.getAttribute("src") ?? "");
  return found ? { kind: found[1], data: found[2] } : undefined;
};

/** A colour the browser worked out, as PowerPoint writes one. Nothing for a clear one. */
function hex(colour: string): string | undefined {
  const parts = colour.match(/[\d.]+/g)?.map(Number);
  if (!parts || parts.length < 3 || (parts[3] ?? 1) < 0.5) return undefined;
  return parts.slice(0, 3).map((part) => Math.round(part).toString(16).padStart(2, "0")).join("").toUpperCase();
}

/**
 * Lays each slide out as the frame would, with scripts off, and reads where
 * everything landed: its place, size, weight and colour. Nothing where the
 * page cannot be laid out, as in a test without a screen.
 */
async function measured(deck: Deck): Promise<Measured[] | undefined> {
  if (!document.body.getBoundingClientRect().width) return undefined;
  const frame = document.createElement("iframe");
  // Same origin so it can be read, no scripts so nothing in it can run.
  frame.setAttribute("sandbox", "allow-same-origin");
  frame.setAttribute("aria-hidden", "true");
  frame.style.cssText = "position:fixed;left:-10000px;top:0;width:960px;height:540px;border:0;visibility:hidden";
  document.body.append(frame);
  try {
    const slides: Measured[] = [];
    for (let index = 0; index < deck.slides.length; index++) {
      const loaded = new Promise<void>((resolve) => frame.addEventListener("load", () => resolve(), { once: true }));
      frame.srcdoc = slidePage(deck, index);
      await Promise.race([loaded, new Promise((resolve) => setTimeout(resolve, 3000))]);
      const page = frame.contentDocument;
      const view = frame.contentWindow;
      const sheet = page?.querySelector("section") ?? page?.body;
      if (!page || !view || !sheet || !sheet.getBoundingClientRect().width) return undefined;
      const origin = sheet.getBoundingClientRect();
      const boxOf = (node: Element, grow = 0) => {
        const at = node.getBoundingClientRect();
        return [at.left - origin.left, at.top - origin.top, at.width + grow, at.height + grow].map((px) => px * UNITS);
      };
      const placed: Placed[] = blocksOf(sheet).map((node) => {
        const style = view.getComputedStyle(node);
        const look: Look = {
          size: Math.max(6, Math.round(parseFloat(style.fontSize) || 18)),
          bold: Number(style.fontWeight) >= 600,
          italic: style.fontStyle === "italic",
          colour: hex(style.color),
          align: { center: "ctr", right: "r", end: "r", justify: "just" }[style.textAlign],
        };
        // A little room to spare: PowerPoint's letters are never quite the browser's width.
        return { box: boxOf(node, 10), lines: [{ text: said(node), bullet: node.tagName === "LI" }], look };
      });
      for (const image of sheet.querySelectorAll("img")) {
        const picture = pictureOf(image);
        if (picture && image.getBoundingClientRect().width) placed.push({ box: boxOf(image), picture });
      }
      const ground = [sheet, page.body, page.documentElement]
        .map((node) => hex(view.getComputedStyle(node).backgroundColor))
        .find(Boolean);
      slides.push({ ground, placed });
    }
    return slides;
  } finally {
    frame.remove();
  }
}

/** What a slide says, in the order it says it. */
export function outline(html: string): Slide {
  const page = new DOMParser().parseFromString(html, "text/html").body;
  const blocks = blocksOf(page);
  const heading = blocks.find((node) => /^H[1-6]$/.test(node.tagName)) ?? blocks[0];
  const pictures = [...page.querySelectorAll("img")]
    .map(pictureOf)
    .filter((found): found is Picture => !!found);
  return {
    title: heading ? said(heading) : "",
    lines: blocks.filter((node) => node !== heading).map((node) => ({ text: said(node), bullet: node.tagName === "LI" })),
    pictures: pictures.slice(0, 2),
  };
}

/** A slide's words in a plain layout: a title, the rest as text, up to two pictures beside it. */
function plain(html: string): Measured {
  const slide = outline(html);
  const wide = SHEET.w - 2 * MARGIN;
  const column = slide.pictures.length ? wide * 0.54 : wide;
  const tall = (SHEET.h - 2.6 * INCH) / Math.max(1, slide.pictures.length);
  return {
    placed: [
      { box: [MARGIN, 0.5 * INCH, wide, 1.2 * INCH], lines: [{ text: slide.title, bullet: false }], look: { size: 34, bold: true } },
      { box: [MARGIN, 1.9 * INCH, column, SHEET.h - 2.6 * INCH], lines: slide.lines, look: { size: 20 } },
      ...slide.pictures.map((picture, at) => ({
        box: [MARGIN + wide * 0.58, 1.9 * INCH + at * tall, wide * 0.42, tall - 0.15 * INCH],
        picture,
      })),
    ],
  };
}

function text(id: number, name: string, box: number[], lines: Line[], look: Look, inset = true): string {
  const [x, y, w, h] = box.map(Math.round);
  const ink = look.colour ? `<a:solidFill><a:srgbClr val="${look.colour}"/></a:solidFill>` : "";
  const run = `lang="en-US" sz="${look.size * 100}"${look.bold ? ' b="1"' : ""}${look.italic ? ' i="1"' : ""} dirty="0"`;
  const rows = lines.map((line) => {
    const bullet = line.bullet ? '<a:buFont typeface="Arial"/><a:buChar char="&#8226;"/>' : "";
    const para = `<a:pPr${look.align ? ` algn="${look.align}"` : ""}${line.bullet ? ' marL="285750" indent="-285750"' : ""}>${bullet}</a:pPr>`;
    return `<a:p>${para}<a:r><a:rPr ${run}>${ink}</a:rPr><a:t>${escape(line.text)}</a:t></a:r></a:p>`;
  });
  // A measured box already holds the text exactly, so it is given no padding of PowerPoint's own.
  const body = inset ? '<a:bodyPr wrap="square" rtlCol="0"><a:normAutofit/></a:bodyPr>' : '<a:bodyPr wrap="square" lIns="0" tIns="0" rIns="0" bIns="0" rtlCol="0"/>';
  return `<p:sp><p:nvSpPr><p:cNvPr id="${id}" name="${name}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="${x}" y="${y}"/><a:ext cx="${w}" cy="${h}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody>${body}<a:lstStyle/>${rows.join("") || "<a:p/>"}</p:txBody></p:sp>`;
}

function image(id: number, rel: string, box: number[]): string {
  const [x, y, w, h] = box.map(Math.round);
  return `<p:pic><p:nvPicPr><p:cNvPr id="${id}" name="Picture ${id}"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="${rel}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="${x}" y="${y}"/><a:ext cx="${w}" cy="${h}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>`;
}

const tree = (shapes: string) =>
  `<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>${shapes}</p:spTree></p:cSld>`;
const rels = (entries: string[]) =>
  `${HEAD}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">${entries.join("")}</Relationships>`;
const rel = (id: string, kind: string, target: string) =>
  `<Relationship Id="${id}" Type="${REL}/${kind}" Target="${target}"/>`;

/** The deck as the bytes of a PowerPoint file. */
export async function writeDeck(deck: Deck, name: string): Promise<Uint8Array> {
  const zip = new JSZip();
  // Each slide as it was measured; where that cannot be done, as its words in a plain layout.
  const slides = (await measured(deck).catch(() => undefined)) ?? deck.slides.map(plain);
  const media = new Set<string>();
  slides.forEach((slide, index) => {
    const number = index + 1;
    const links = [rel("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")];
    const shapes = slide.placed.map((item, at) => {
      if ("lines" in item) return text(at + 2, `Text ${at + 1}`, item.box, item.lines, item.look, !slide.ground && !item.look.colour);
      const file = `image${number}-${links.length}.${item.picture.kind}`;
      media.add(item.picture.kind);
      zip.file(`ppt/media/${file}`, item.picture.data, { base64: true });
      links.push(rel(`rId${links.length + 1}`, "image", `../media/${file}`));
      return image(at + 2, `rId${links.length}`, item.box);
    });
    const ground = slide.ground
      ? `<p:bg><p:bgPr><a:solidFill><a:srgbClr val="${slide.ground}"/></a:solidFill><a:effectLst/></p:bgPr></p:bg>`
      : "";
    zip.file(
      `ppt/slides/slide${number}.xml`,
      `${HEAD}<p:sld ${NS}>${tree(shapes.join("")).replace("<p:cSld>", `<p:cSld>${ground}`)}<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>`,
    );
    zip.file(`ppt/slides/_rels/slide${number}.xml.rels`, rels(links));
  });

  const numbers = slides.map((_, index) => index + 1);
  zip.file(
    "[Content_Types].xml",
    `${HEAD}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>${[...media].map((kind) => `<Default Extension="${kind}" ContentType="image/${kind}"/>`).join("")}<Override PartName="/ppt/presentation.xml" ContentType="${TYPE}.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="${TYPE}.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="${TYPE}.slideLayout+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>${numbers.map((n) => `<Override PartName="/ppt/slides/slide${n}.xml" ContentType="${TYPE}.slide+xml"/>`).join("")}</Types>`,
  );
  zip.file(
    "_rels/.rels",
    rels([
      rel("rId1", "officeDocument", "ppt/presentation.xml"),
      '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>',
    ]),
  );
  zip.file(
    "docProps/core.xml",
    `${HEAD}<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>${escape(name)}</dc:title><dc:creator>Medha</dc:creator></cp:coreProperties>`,
  );
  zip.file(
    "ppt/presentation.xml",
    `${HEAD}<p:presentation ${NS}><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst>${numbers.map((n) => `<p:sldId id="${255 + n}" r:id="rId${n + 2}"/>`).join("")}</p:sldIdLst><p:sldSz cx="${SHEET.w}" cy="${SHEET.h}"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>`,
  );
  zip.file(
    "ppt/_rels/presentation.xml.rels",
    rels([
      rel("rId1", "slideMaster", "slideMasters/slideMaster1.xml"),
      rel("rId2", "theme", "theme/theme1.xml"),
      ...numbers.map((n) => rel(`rId${n + 2}`, "slide", `slides/slide${n}.xml`)),
    ]),
  );
  zip.file(
    "ppt/slideMasters/slideMaster1.xml",
    `${HEAD}<p:sldMaster ${NS}>${tree("").replace("<p:cSld>", '<p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>')}<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>`,
  );
  zip.file(
    "ppt/slideMasters/_rels/slideMaster1.xml.rels",
    rels([rel("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"), rel("rId2", "theme", "../theme/theme1.xml")]),
  );
  zip.file(
    "ppt/slideLayouts/slideLayout1.xml",
    `${HEAD}<p:sldLayout ${NS} type="blank" preserve="1">${tree("").replace("<p:cSld>", '<p:cSld name="Blank">')}<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>`,
  );
  zip.file("ppt/slideLayouts/_rels/slideLayout1.xml.rels", rels([rel("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")]));
  zip.file("ppt/theme/theme1.xml", HEAD + THEME);
  return zip.generateAsync({ type: "uint8array", compression: "DEFLATE" });
}

const INK = (name: string, hex: string) => `<a:${name}><a:srgbClr val="${hex}"/></a:${name}>`;
const FILL = '<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>';
const LINE = `<a:ln w="9525" cap="flat" cmpd="sng" algn="ctr">${FILL}<a:prstDash val="solid"/></a:ln>`;
const EFFECT = "<a:effectStyle><a:effectLst/></a:effectStyle>";
const FONT = '<a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/>';
const THEME = `<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Medha"><a:themeElements><a:clrScheme name="Medha">${INK("dk1", "1C1A16")}${INK("lt1", "FFFFFF")}${INK("dk2", "2B261E")}${INK("lt2", "F9F6EF")}${INK("accent1", "8C5C0E")}${INK("accent2", "B07A1E")}${INK("accent3", "5A7D5A")}${INK("accent4", "3A6EA5")}${INK("accent5", "A0522D")}${INK("accent6", "6B6253")}${INK("hlink", "3A6EA5")}${INK("folHlink", "6B6253")}</a:clrScheme><a:fontScheme name="Medha"><a:majorFont>${FONT}</a:majorFont><a:minorFont>${FONT}</a:minorFont></a:fontScheme><a:fmtScheme name="Medha"><a:fillStyleLst>${FILL.repeat(3)}</a:fillStyleLst><a:lnStyleLst>${LINE.repeat(3)}</a:lnStyleLst><a:effectStyleLst>${EFFECT.repeat(3)}</a:effectStyleLst><a:bgFillStyleLst>${FILL.repeat(3)}</a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>`;
