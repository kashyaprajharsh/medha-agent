import { useState } from "react";
import { restOf, summaryOf } from "./ExtensionParts";
import { Sheet } from "./Sheet";

export type Plugin = {
  id: string;
  name: string;
  description?: string;
  version: string;
  scope: string;
  activation: string;
  hash: string;
  grant: Record<string, unknown> | null;
  can_rollback: boolean;
  blocked?: string;
  components: { id: string; kind: string }[];
  permissions: {
    network_hosts: string[];
    read_paths: string[];
    write_paths: string[];
    secrets: string[];
  };
  health: { id: string; state: string; error?: string }[];
};

const SCOPE: Record<string, string> = { user: "Every project on this computer", project: "This project only" };
const many = (count: number, one: string) => `${count} ${one}${count === 1 ? "" : "s"}`;
const place = (path: string) => (!path || path === "." ? "the project you are in" : path);
const scripts = (plugin: Plugin) => plugin.components.filter((part) => part.kind === "Hook").length;

/** What a plugin can do beyond being read: worth seeing before it is opened, let alone turned on. */
function reach(plugin: Plugin): { label: string; warn: boolean }[] {
  return [
    plugin.permissions.network_hosts.length ? { label: "Internet", warn: false } : null,
    plugin.permissions.write_paths.length ? { label: "Changes files", warn: true } : null,
    plugin.permissions.secrets.length ? { label: "Reads secrets", warn: true } : null,
    scripts(plugin) ? { label: "Runs on its own", warn: true } : null,
  ].filter((chip) => chip !== null);
}

function state(plugin: Plugin): { label: string; tone: string } | undefined {
  if (plugin.health.some((part) => part.state === "failed")) return { label: "Needs attention", tone: "bad" };
  if (plugin.activation.startsWith("changed")) return { label: "Needs review", tone: "warn" };
}

type Props = {
  plugins: Plugin[];
  disabled: boolean;
  /** Turning one on goes through the review of what it asks for. */
  onReview: (plugin: Plugin) => void;
  onTurnOff: (plugin: Plugin) => void;
  onCheckUpdate: (plugin: Plugin) => void;
  onRollback: (plugin: Plugin) => void;
  onRemove: (plugin: Plugin) => void;
};

/** The installed plugins, one line each; a row opens the plugin in full. */
export function PluginList({ plugins, disabled, onReview, onTurnOff, onCheckUpdate, onRollback, onRemove }: Props) {
  const [openKey, setOpenKey] = useState("");
  const keyOf = (plugin: Plugin) => `${plugin.scope}:${plugin.id}`;
  const open = plugins.find((plugin) => keyOf(plugin) === openKey);
  const on = (plugin: Plugin) => plugin.activation === "enabled";
  // A question that follows is asked on the page, so the sheet steps aside for it.
  const aside = (then: (plugin: Plugin) => void) => (plugin: Plugin) => {
    setOpenKey("");
    then(plugin);
  };
  const control = (plugin: Plugin) => (
    <button
      className="switch quiet-on"
      role="switch"
      aria-checked={on(plugin)}
      aria-label={on(plugin) ? `Turn off ${plugin.name}` : `Review and enable ${plugin.name}`}
      disabled={disabled || (!on(plugin) && !plugin.grant)}
      onClick={() => (on(plugin) ? onTurnOff(plugin) : aside(onReview)(plugin))}
    />
  );
  if (!plugins.length) return <p className="quiet">No plugins discovered in this workspace.</p>;
  return (
    <>
      <div className="ix">
        {plugins.map((plugin) => (
          <div className={`ix-row ${on(plugin) ? "" : "off"}`} key={keyOf(plugin)}>
            <button className="ix-main" onClick={() => setOpenKey(keyOf(plugin))}>
              <b>{plugin.name}</b>
              <span className="ix-sum">{summaryOf(plugin.description ?? "")}</span>
            </button>
            {state(plugin) && <span className={`ix-chip ${state(plugin)!.tone}`}>{state(plugin)!.label}</span>}
            {reach(plugin).map((chip) => (
              <span className={`ix-chip ${chip.warn ? "warn" : ""}`} key={chip.label}>
                {chip.label}
              </span>
            ))}
            {control(plugin)}
          </div>
        ))}
      </div>
      {open && (
        <Sheet label={open.name} onClose={() => setOpenKey("")}>
          <div className="sheet-title">
            <h2>
              {open.name} <small>{open.version}</small>
            </h2>
            {control(open)}
          </div>
          {open.description && <p className="sheet-lead">{summaryOf(open.description)}</p>}
          {open.description && restOf(open.description) && <p className="sheet-more">{restOf(open.description)}</p>}
          {open.blocked && <p className="surface-error">{open.blocked}</p>}
          {open.health
            .filter((part) => part.error)
            .map((part) => (
              <p className="surface-error" key={part.id}>
                {part.id}: {part.error}
              </p>
            ))}
          <h3 className="sheet-h">{on(open) ? "What it can touch" : "Before you turn it on"}</h3>
          <dl className="sheet-facts">
            <dt>Internet</dt>
            <dd>{open.permissions.network_hosts.length ? `Only ${open.permissions.network_hosts.join(", ")}` : "None"}</dd>
            <dt>Reads</dt>
            <dd>{open.permissions.read_paths.length ? `Files in ${open.permissions.read_paths.map(place).join(", ")}` : "Nothing of yours"}</dd>
            <dt>Changes</dt>
            <dd className={open.permissions.write_paths.length ? "warn" : ""}>
              {open.permissions.write_paths.length ? `Files in ${open.permissions.write_paths.map(place).join(", ")}` : "Nothing"}
            </dd>
            {open.permissions.secrets.length > 0 && (
              <>
                <dt>Secrets</dt>
                <dd className="warn">{open.permissions.secrets.join(", ")}</dd>
              </>
            )}
            <dt>On its own</dt>
            <dd className={scripts(open) ? "warn" : ""}>
              {scripts(open)
                ? `${many(scripts(open), "script")} that ${scripts(open) === 1 ? "runs" : "run"} without being asked`
                : "Nothing. It acts only when Medha uses it."}
            </dd>
            <dt>Used in</dt>
            <dd>{SCOPE[open.scope] ?? open.scope}</dd>
          </dl>
          <h3 className="sheet-h">What it adds</h3>
          <ul className="sheet-parts">
            {open.components.map((part) => (
              <li key={part.id}>
                <span>{part.kind}</span>
                {part.id}
              </li>
            ))}
          </ul>
          <div className="sheet-actions">
            <button
              className="btn-gold"
              disabled={disabled || (!on(open) && !open.grant)}
              onClick={() => (on(open) ? onTurnOff(open) : aside(onReview)(open))}
            >
              {on(open) ? "Turn off" : "Review and enable"}
            </button>
            {open.scope === "user" && (
              <>
                <button className="btn-line" disabled={disabled} onClick={() => aside(onCheckUpdate)(open)}>
                  Check for updates
                </button>
                {open.can_rollback && (
                  <button className="btn-line" disabled={disabled} onClick={() => aside(onRollback)(open)}>
                    Previous version
                  </button>
                )}
                <button className="sheet-quiet" disabled={disabled} onClick={() => aside(onRemove)(open)}>
                  Remove
                </button>
              </>
            )}
          </div>
        </Sheet>
      )}
    </>
  );
}
