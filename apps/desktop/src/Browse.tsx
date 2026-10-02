import { useState, type ReactNode } from "react";
import { restOf, summaryOf } from "./ExtensionParts";
import { Icon } from "./Icon";
import { Sheet } from "./Sheet";

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

/**
 * A catalog entry on one line: what it is, where it comes from, and the one thing
 * to do with it. The row opens the entry in full, with the same thing to do.
 */
export function BrowseRow({
  name,
  meta,
  /** Whether where it comes from tells the rows apart; with one source it is said only in full. */
  several = true,
  description,
  action,
}: {
  name: string;
  meta?: string;
  several?: boolean;
  description?: string;
  action: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <li className="ix-row">
      <button className="ix-main" onClick={() => setOpen(true)}>
        <b>{name}</b>
        <span className="ix-sum">{description ? summaryOf(description) : ""}</span>
      </button>
      {meta && several && <small className="ix-note">{meta}</small>}
      <span className="ix-act">{action}</span>
      {open && (
        <Sheet label={name} onClose={() => setOpen(false)}>
          <div className="sheet-title">
            <h2>{name}</h2>
          </div>
          {meta && <p className="sheet-from">From {meta}</p>}
          {description && <p className="sheet-lead">{summaryOf(description)}</p>}
          {description && restOf(description) && <p className="sheet-more">{restOf(description)}</p>}
          <div className="sheet-actions">{action}</div>
        </Sheet>
      )}
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
