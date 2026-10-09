import { useEffect, useId, useRef, useState, type ReactNode, type Ref } from "react";
import { desktop, type ExtensionSourceKind } from "./api";
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

/** Browse local sources or paste a path/URL, then explicitly install the selected source. */
export function InstallSource({
  kind,
  value,
  disabled,
  onChange,
  onInstall,
  onError,
}: {
  kind: "skill" | "plugin";
  value: string;
  disabled: boolean;
  onChange: (value: string) => void;
  onInstall: (source: string) => Promise<void>;
  onError: (message: string) => void;
}) {
  const [choices, setChoices] = useState(false);
  const [picking, setPicking] = useState(false);
  const active = useRef(true);
  const browse = useRef<HTMLButtonElement>(null);
  const choicesId = useId();
  useEffect(() => {
    active.current = true;
    return () => { active.current = false; };
  }, []);
  async function choose(selection: ExtensionSourceKind) {
    if (disabled || picking) return;
    setChoices(false);
    setPicking(true);
    onError("");
    try {
      const path = await desktop.chooseExtensionSource(selection);
      if (active.current && path !== null) onChange(path);
    } catch (cause) {
      if (active.current) onError(String(cause));
    } finally {
      if (active.current) setPicking(false);
    }
  }
  return (
    <div className="extension-source" aria-busy={picking} onKeyDown={(event) => {
      if (event.key === "Escape" && choices) {
        event.preventDefault();
        event.stopPropagation();
        setChoices(false);
        browse.current?.focus();
      }
    }}>
      <form className="plugin-install" onSubmit={(event) => {
        event.preventDefault();
        if (!disabled && !picking && value.trim()) void onInstall(value);
      }}>
        <button
          ref={browse}
          type="button"
          className="icon-btn"
          aria-label={`Choose local ${kind}`}
          title={`Choose ${kind === "skill" ? "a skill folder or SKILL.md" : "a plugin folder"}`}
          aria-expanded={kind === "skill" ? choices : undefined}
          aria-controls={kind === "skill" ? choicesId : undefined}
          disabled={disabled || picking}
          onClick={() => kind === "skill" ? setChoices(!choices) : void choose("plugin_folder")}
        >
          <Icon name="plus" />
        </button>
        <label>
          <input
            required
            autoFocus
            aria-label={kind === "skill" ? "Skill source" : "Plugin source"}
            value={value}
            disabled={disabled || picking}
            onChange={(event) => onChange(event.target.value)}
            placeholder={kind === "skill" ? "Skill folder, SKILL.md, or GitHub folder URL" : "owner/repo, name@marketplace, or a folder"}
          />
        </label>
        <button className="btn-line" disabled={disabled || picking || !value.trim()}>
          {kind === "skill" ? "Install skill" : "Install"}
        </button>
      </form>
      {kind === "skill" && choices && (
        <div id={choicesId} className="extension-source-choices" role="group" aria-label="Choose a skill from your files">
          <button type="button" className="btn-line" disabled={disabled || picking} onClick={() => void choose("skill_folder")}>
            Choose folder
          </button>
          <button type="button" className="btn-line" disabled={disabled || picking} onClick={() => void choose("skill_file")}>
            Choose SKILL.md
          </button>
          <p>Choose the folder to include its scripts and reference files.</p>
        </div>
      )}
    </div>
  );
}
