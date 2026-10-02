// Draws the charts a PowerPoint file holds, from the numbers stored with them:
// bars, lines and pies, as plain script-free SVG. Any other kind is left to
// the caller to name rather than drawn wrong.

import { escapeMarkup } from "./outputBlocks";

type Series = { name: string; values: number[] };
const MAX_POINTS = 60;
const MAX_SERIES = 12;
const [W, H, LEFT, TOP, BOTTOM] = [600, 400, 46, 16, 64];

const all = (node: Element | Document, name: string) => [...node.getElementsByTagNameNS("*", name)];
const points = (node: Element | undefined) =>
  node ? all(node, "pt").slice(0, MAX_POINTS).map((point) => all(point, "v")[0]?.textContent ?? "") : [];

const text = (x: number, y: number, words: string, anchor = "middle") =>
  `<text x="${x.toFixed(1)}" y="${y.toFixed(1)}" text-anchor="${anchor}" font-size="13">${escapeMarkup(words)}</text>`;

function legend(names: string[], colours: string[]): string {
  const step = W / Math.max(1, names.length);
  return names
    .map((name, at) => {
      const x = at * step + 12;
      return `<rect x="${x.toFixed(1)}" y="${H - 20}" width="12" height="12" rx="2" fill="${colours[at % colours.length]}"/>${text(x + 18, H - 9, name, "start")}`;
    })
    .join("");
}

/** A round number at or above the largest value, so the scale ends on a tidy line. */
function ceiling(most: number): number {
  const unit = 10 ** Math.floor(Math.log10(Math.max(most, 1e-9)));
  return [1, 2, 5, 10].map((step) => step * unit).find((top) => top >= most) ?? most;
}

function plot(kind: "bar" | "line", labels: string[], series: Series[], colours: string[]): string {
  const top = ceiling(Math.max(0, ...series.flatMap((one) => one.values)));
  const tall = H - TOP - BOTTOM;
  const band = (W - LEFT - 12) / Math.max(1, labels.length);
  const y = (value: number) => TOP + tall - (Math.max(0, value) / top) * tall;
  const lines = [0, 0.25, 0.5, 0.75, 1].map(
    (part) =>
      `<line x1="${LEFT}" x2="${W - 12}" y1="${y(top * part).toFixed(1)}" y2="${y(top * part).toFixed(1)}" stroke="currentColor" stroke-opacity=".15"/>${text(LEFT - 6, y(top * part) + 4, String(Number((top * part).toPrecision(3))), "end")}`,
  );
  const names = labels.map((label, at) => text(LEFT + band * (at + 0.5), H - BOTTOM + 18, label));
  const marks = series.map((one, index) => {
    const colour = colours[index % colours.length];
    if (kind === "line") {
      const path = one.values.map((value, at) => `${(LEFT + band * (at + 0.5)).toFixed(1)},${y(value).toFixed(1)}`);
      return `<polyline points="${path.join(" ")}" fill="none" stroke="${colour}" stroke-width="3"/>`;
    }
    const wide = (band * 0.7) / series.length;
    return one.values
      .map((value, at) => {
        const x = LEFT + band * at + band * 0.15 + wide * index;
        return `<rect x="${x.toFixed(1)}" y="${y(value).toFixed(1)}" width="${(wide - 2).toFixed(1)}" height="${(TOP + tall - y(value)).toFixed(1)}" rx="3" fill="${colour}"/>`;
      })
      .join("");
  });
  return lines.join("") + marks.join("") + names.join("") + legend(series.map((one) => one.name), colours);
}

function pie(labels: string[], values: number[], colours: string[]): string {
  const total = values.reduce((sum, value) => sum + Math.max(0, value), 0) || 1;
  const [cx, cy, radius] = [W / 2, (H - 40) / 2 + 8, (H - 70) / 2];
  let turned = -Math.PI / 2;
  const slices = values.map((value, at) => {
    const sweep = (Math.max(0, value) / total) * Math.PI * 2;
    const [from, to] = [turned, (turned += sweep)];
    const at2 = (angle: number) => `${(cx + radius * Math.cos(angle)).toFixed(1)} ${(cy + radius * Math.sin(angle)).toFixed(1)}`;
    // A whole circle has no two distinct ends for an arc to join.
    return sweep > Math.PI * 1.999
      ? `<circle cx="${cx}" cy="${cy}" r="${radius}" fill="${colours[at % colours.length]}"/>`
      : `<path d="M${cx} ${cy} L${at2(from)} A${radius} ${radius} 0 ${sweep > Math.PI ? 1 : 0} 1 ${at2(to)} Z" fill="${colours[at % colours.length]}"/>`;
  });
  return slices.join("") + legend(labels, colours);
}

/** The chart in a chart part, drawn; nothing when it is a kind this cannot draw. */
export function chartMarkup(part: Document, colours: string[]): string | undefined {
  const area = all(part, "plotArea")[0];
  const found = (["barChart", "bar3DChart", "lineChart", "pieChart", "doughnutChart"] as const)
    .map((name) => ({ name, node: area && all(area, name)[0] }))
    .find((entry) => entry.node);
  if (!found?.node) return undefined;
  const series: Series[] = all(found.node, "ser")
    .slice(0, MAX_SERIES)
    .map((one, at) => ({
      name: points(all(one, "tx")[0])[0] || `Series ${at + 1}`,
      values: points(all(one, "val")[0]).map((value) => Number(value) || 0),
    }));
  const labels = points(all(found.node, "cat")[0]);
  if (!series.length || !series[0].values.length) return undefined;
  const inside = found.name.includes("pie") || found.name.includes("doughnut")
    ? pie(labels, series[0].values, colours)
    : plot(found.name === "lineChart" ? "line" : "bar", labels, series, colours);
  return `<svg viewBox="0 0 ${W} ${H}" fill="currentColor" preserveAspectRatio="xMidYMid meet" style="width:100%;height:100%">${inside}</svg>`;
}
