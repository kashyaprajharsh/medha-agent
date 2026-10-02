import type { ReactNode, Ref } from "react";
import { Icon } from "./Icon";

/** The first sentence of a description, without the lead-in its author wrote for the model. */
export function summaryOf(description: string): string {
  const first = description.split(/(?<=[.!?])\s+/)[0] ?? "";
  const told = first
    .replace(/^use this skill (whenever|when) the user wants to\s+/i, "")
    .replace(/^use this skill\s+/i, "");
  return told === first || !told ? first : told[0].toUpperCase() + told.slice(1);
}

/** What follows the first sentence. */
export function restOf(description: string): string {
  return description.split(/(?<=[.!?])\s+/).slice(1).join(" ");
}

/** A tab's name and the one line that says what it holds. */
export function PageTop({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="ext-top">
      <h2>{title}</h2>
      <p>{children}</p>
    </div>
  );
}

type View<V extends string> = { id: V; label: string; count?: number };

/** One row of controls for a tab: its views on the left, whatever acts on the list on the right. */
export function ViewBar<V extends string>({
  views,
  view,
  onView,
  children,
}: {
  views?: View<V>[];
  view?: V;
  onView?: (view: V) => void;
  children?: ReactNode;
}) {
  return (
    <div className="ext-bar">
      {views && (
        <div className="ext-seg">
          {views.map((each) => (
            <button key={each.id} aria-pressed={view === each.id} onClick={() => onView?.(each.id)}>
              {each.label}
              {each.count !== undefined && <i>{each.count}</i>}
            </button>
          ))}
        </div>
      )}
      <span className="ext-bar-end">{children}</span>
    </div>
  );
}

/** The list's own search, focused by `/`. */
export function Find({
  box,
  label,
  value,
  onChange,
}: {
  box: Ref<HTMLInputElement>;
  label: string;
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <label className="ext-find">
      <Icon name="search" />
      <input ref={box} aria-label={label} placeholder={label} value={value} onChange={(event) => onChange(event.target.value)} />
      <kbd>/</kbd>
    </label>
  );
}

/** The quiet button that starts adding one more. */
export function Add({ label, open, onClick }: { label: string; open: boolean; onClick: () => void }) {
  return (
    <button className="ext-add" aria-expanded={open} onClick={onClick}>
      <Icon name={open ? "x" : "plus"} />
      {label}
    </button>
  );
}
