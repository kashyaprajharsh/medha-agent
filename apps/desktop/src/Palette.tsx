import { useEffect, useMemo, useRef, useState } from "react";
import type { Session } from "./api";
import { Icon, type IconName } from "./Icon";

export type Action = {
  id: string;
  icon: IconName;
  label: string;
  hint?: string;
  run: () => void;
};

type Item = Action & { group: string };

export function Palette({
  actions,
  sessions,
  onOpen,
  onClose,
}: {
  actions: Action[];
  sessions: Session[];
  onOpen: (id: string) => void;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => {
    input.current?.focus();
  }, []);

  const items = useMemo(() => {
    const term = query.trim().toLowerCase();
    const all: Item[] = [
      ...actions.map((action) => ({ ...action, group: "Actions" })),
      ...sessions.map((session) => ({
        id: `session:${session.id}`,
        icon: "chat" as IconName,
        label: session.title || "Untitled session",
        hint: new Date(session.last_ts * 1000).toLocaleDateString(undefined, {
          month: "short",
          day: "numeric",
        }),
        run: () => onOpen(session.id),
        group: "Sessions",
      })),
    ];
    return term
      ? all.filter((item) => item.label.toLowerCase().includes(term))
      : all;
  }, [query, actions, sessions, onOpen]);

  useEffect(() => {
    setIndex(0);
  }, [query]);
  useEffect(() => {
    list.current
      ?.querySelector('[aria-selected="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [index]);

  function run(item?: Item) {
    if (!item) return;
    onClose();
    item.run();
  }

  let group = "";
  return (
    <div
      className="scrim"
      onMouseDown={(event) => event.target === event.currentTarget && onClose()}
    >
      <div
        className="palette"
        role="dialog"
        aria-modal="true"
        aria-label="Search and commands"
      >
        <div className="pal-input">
          <Icon name="search" />
          <input
            ref={input}
            value={query}
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-list"
            placeholder="Search sessions and actions"
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                event.preventDefault();
                const step = event.key === "ArrowDown" ? 1 : items.length - 1;
                setIndex((value) => (value + step) % Math.max(1, items.length));
              } else if (event.key === "Enter") {
                event.preventDefault();
                run(items[index]);
              } else if (event.key === "Escape") {
                onClose();
              }
            }}
          />
          <kbd>esc</kbd>
        </div>
        <div className="pal-list" id="palette-list" role="listbox" ref={list}>
          {items.length === 0 && (
            <p className="pal-none">Nothing matches “{query}”.</p>
          )}
          {items.map((item, position) => {
            const heading = item.group !== group ? (group = item.group) : null;
            return (
              <div key={item.id}>
                {heading && <div className="pal-group">{heading}</div>}
                <button
                  type="button"
                  role="option"
                  className="pal-item"
                  aria-selected={position === index}
                  onMouseMove={() => position !== index && setIndex(position)}
                  onClick={() => run(item)}
                >
                  <Icon name={item.icon} />
                  <span className="lbl">{item.label}</span>
                  {item.hint && <span className="hint">{item.hint}</span>}
                </button>
              </div>
            );
          })}
        </div>
        <div className="pal-foot">
          <span>
            <kbd>↑</kbd>
            <kbd>↓</kbd> move
          </span>
          <span>
            <kbd>↵</kbd> open
          </span>
          <span>
            <kbd>esc</kbd> close
          </span>
        </div>
      </div>
    </div>
  );
}
