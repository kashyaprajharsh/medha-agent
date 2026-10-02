// Prepares a page Medha wrote for its frame. The frame has no network, so the
// libraries a page asks a CDN for are swapped for the copies bundled here.

const LIBRARIES = [
  {
    asked: /cdn\.tailwindcss\.com|@tailwindcss\/browser|\btailwind(css)?(\.min)?\.js\b/i,
    load: () => import("../node_modules/@tailwindcss/browser/dist/index.global.js?raw"),
  },
  {
    asked: /\bchart(\.umd)?(\.min)?\.js\b|\/chart\.js(@[\w.^~-]+)?\/?$/i,
    load: () => import("../node_modules/chart.js/dist/chart.umd.min.js?raw"),
  },
  {
    asked: /\bd3(\.v\d+)?(\.min)?\.js\b|\/d3(@[\w.^~-]+)?\/?$/i,
    load: () => import("../node_modules/d3/dist/d3.min.js?raw"),
  },
];
const EXTERNAL = /<script\b[^>]*\bsrc=["']([^"']+)["'][^>]*>\s*<\/script>/gi;
const REMOTE = /<(?:script|link|img|iframe|video|audio|source)\b[^>]*\b(?:src|href)=["'](https?:)?\/\/([^/"']+)[^"']*["']/gi;

/** The sites a page reaches for that it cannot have here, for the person to be told. */
export function unreachable(source: string): string[] {
  const hosts = new Set<string>();
  for (const [tag, , host] of source.matchAll(REMOTE))
    if (!LIBRARIES.some((library) => library.asked.test(tag))) hosts.add(host.toLowerCase());
  return [...hosts];
}

function tone(): string {
  const style = getComputedStyle(document.body);
  const dark = document.documentElement.dataset.theme !== "light";
  // Named fonts live with the app, so the frame is given the family's fallbacks.
  return `<style>:root{color-scheme:${dark ? "dark" : "light"}}html{color:${style.color};font-family:system-ui,sans-serif}</style>`;
}

/** The document as its frame will be served it. */
export async function framed(source: string, run: boolean, online = false): Promise<string> {
  let page = source;
  // Allowed the web, a page loads what it asked for itself; otherwise the bundled copies stand in.
  if (run && !online) {
    const asked = [...page.matchAll(EXTERNAL)];
    for (const [tag, url] of asked) {
      const library = LIBRARIES.find((entry) => entry.asked.test(url));
      if (!library) continue;
      const code = (await library.load()).default.replace(/<\/script/gi, "<\\/script");
      page = page.replace(tag, () => `<script>${code}</script>`);
    }
  }
  return opening(page, tone());
}

/** Puts markup at the very start of a page, ahead of anything the page itself brings. */
function opening(page: string, markup: string): string {
  const after =
    /<head[^>]*>/i.exec(page) ?? /<html[^>]*>/i.exec(page) ?? /<!doctype[^>]*>/i.exec(page);
  const cut = after ? after.index + after[0].length : 0;
  return page.slice(0, cut) + markup + page.slice(cut);
}

// A screen's frame has an origin of its own that nothing else shares, so the
// browser gives it no storage and reading it throws. Screens written for a
// browser tab expect storage to exist, so each gets one that lasts as long as
// the frame and is seen by nothing else.
const STORAGE = `<script>(()=>{for(const name of["localStorage","sessionStorage"]){try{window[name].length}catch{const kept=new Map();Object.defineProperty(window,name,{value:{getItem:k=>kept.has(String(k))?kept.get(String(k)):null,setItem:(k,v)=>{kept.set(String(k),String(v))},removeItem:k=>{kept.delete(String(k))},clear:()=>kept.clear(),key:i=>[...kept.keys()][i]??null,get length(){return kept.size}}})}}try{document.cookie}catch{Object.defineProperty(document,"cookie",{get:()=>"",set:()=>{}})}})()</script>`;

/** A server's screen as its frame is served it. */
export const screened = (page: string) => opening(page, STORAGE);
