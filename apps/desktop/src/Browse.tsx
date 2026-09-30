import type { ReactNode } from "react";
import { Icon } from "./Icon";

/** How many results a browse view paints before asking to show the rest. */
export const BROWSE_PAGE = 40;

/** The one prominent search a browse view leads with. */
export function BrowseSearch({ label, placeholder, value, onChange }: { label: string; placeholder: string; value: string; onChange: (value: string) => void }) {
  return (
    <label className="browse-search">
      <Icon name="search" />
      <input type="search" aria-label={label} placeholder={placeholder} value={value} autoFocus onChange={(event) => onChange(event.target.value)} />
    </label>
  );
}

/** A catalog entry: what it is, where it comes from, and the one thing to do with it. */
export function BrowseRow({ name, meta, description, action }: { name: string; meta?: string; description?: string; action: ReactNode }) {
  return (
    <li className="browse-row">
      <div>
        <b>{name}</b>
        {meta && <small>{meta}</small>}
        {description && <p>{description}</p>}
      </div>
      {action}
    </li>
  );
}

export function Installed() {
  return (
    <span className="market-installed">
      <Icon name="check" />
      Installed
    </span>
  );
}

/** Items matching every word of the query; name matches lead, then catalog order. */
export function ranked<T>(items: T[], query: string, text: (item: T) => { name: string; rest: string }): T[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return items;
  const scored = items.flatMap((item, index) => {
    const { name, rest } = text(item);
    const haystack = `${name} ${rest}`.toLowerCase();
    if (!words.every((word) => haystack.includes(word))) return [];
    const lower = name.toLowerCase();
    const score = lower.startsWith(words[0]) ? 0 : words.some((word) => lower.includes(word)) ? 1 : 2;
    return [{ item, score, index }];
  });
  return scored.sort((a, b) => a.score - b.score || a.index - b.index).map((entry) => entry.item);
}
