import type { Ref } from "react";
import { useState } from "react";
import { Find, summaryOf, ViewBar } from "./ExtensionParts";
import type { McpServer } from "./McpServers";

export type Tool = {
  name: string;
  description: string;
  category: string;
  /** How far it reaches, as a person would put it; a web tool reaches outside though it only reads. */
  reach: string;
  /** Why new chats leave it out; empty when it is on. */
  off: string[];
};
type Scope = "user" | "project";

// How far a tool reaches, in the words a person would use for it.
const REACH: Record<string, string> = {
  read: "Looks only",
  reversible_local: "Changes you can undo",
  irreversible_local: "Changes that stay",
  external: "Reaches outside",
};
// Each reason a tool is off: what the row says, and the scope whose switch can undo it.
const REASON: Record<string, { text: string; scope?: Scope; hint: string }> = {
  user_deny: { text: "Off for you in every project", scope: "user", hint: "Switch it on under Just me" },
  server_deny: { text: "Off for you in every project", scope: "user", hint: "Switch it on under Just me" },
  project_deny: { text: "Off in this project, from medha.lock", scope: "project", hint: "Switch it on under This project" },
  core: { text: "Not part of Core tools", hint: "Choose All tools to use it" },
  project_allow: { text: "Left out by this project’s allow list", hint: "Change it in medha.lock" },
  user_allow: { text: "Left out by your allow list", hint: "Change it in config.toml" },
};
const SCOPE_LINE: Record<Scope, string> = {
  user: "Switches apply to new chats in every project, for you only. Saved in your settings.",
  project: "Switches apply to new chats in this project, for everyone who works on it. Saved in medha.lock, which the terminal app uses too.",
};

/** The server an MCP tool belongs to, split the way the backend names it: `mcp__<server>__<tool>`. */
function serverOf(name: string): [string, string] | undefined {
  if (!name.startsWith("mcp__")) return undefined;
  const rest = name.slice(5);
  const at = rest.indexOf("__");
  return at > 0 ? [rest.slice(0, at), rest.slice(at + 2)] : undefined;
}

/** Every tool new chats could have, each with a switch for the chosen scope and, when off, why. */
export function ToolAccess({
  tools,
  servers,
  findBox,
  query,
  onQuery,
  matches,
  busy,
  onSave,
}: {
  tools: Tool[];
  servers: McpServer[];
  findBox: Ref<HTMLInputElement>;
  query: string;
  onQuery: (query: string) => void;
  matches: (name: string, description?: string) => boolean;
  busy: boolean;
  onSave: (method: string, params: Record<string, unknown>) => void;
}) {
  const [scope, setScope] = useState<Scope>("user");
  const groups = new Map<string, Tool[]>([["", []]]);
  for (const tool of tools) {
    const server = serverOf(tool.name)?.[0] ?? "";
    groups.set(server, [...(groups.get(server) ?? []), tool]);
  }

  const flip = (tool: Tool, on: boolean) => {
    const mcp = serverOf(tool.name);
    const saved = mcp && servers.find((server) => server.id === mcp[0]);
    // Just me, for an MCP tool, is its server's own setting, the one the MCP page edits too.
    if (scope === "user" && saved) {
      const deny = on ? saved.deny_tools.filter((name) => name !== mcp[1]) : [...saved.deny_tools, mcp[1]];
      onSave("settings.mcp.update", { id: saved.id, deny_tools: deny });
    } else {
      onSave("settings.tools.switch", { tool: tool.name, on, scope });
    }
  };
  const row = (tool: Tool) => {
    const on = tool.off.length === 0;
    const blocking = tool.off.map((reason) => REASON[reason]).find((reason) => reason?.scope !== scope);
    const why = tool.off.map((reason) => REASON[reason]?.text).find(Boolean);
    const name = serverOf(tool.name)?.[1] ?? tool.name;
    return (
      <details className={`ix-row tool ${on ? "" : "off"}`} key={tool.name}>
        <summary>
          <span className="ix-main">
            <b className="mono">{name}</b>
            <span className="ix-sum">
              {on ? summaryOf(tool.description) : <span className="tool-why">{blocking ? `${why}. ${blocking.hint}` : why}</span>}
            </span>
          </span>
          <small className={`ix-note reach-${tool.reach}`}>{REACH[tool.reach] ?? tool.reach.replaceAll("_", " ")}</small>
          <button
            type="button"
            className="switch quiet-on"
            role="switch"
            aria-checked={on}
            aria-label={`${on ? "Turn off" : "Turn on"} ${name}`}
            disabled={busy || (!on && Boolean(blocking))}
            onClick={(event) => {
              // The switch acts on its own; only the rest of the row opens the description.
              event.preventDefault();
              flip(tool, !on);
            }}
          />
        </summary>
        {tool.description && <p>{tool.description}</p>}
      </details>
    );
  };

  return (
    <>
      {[...groups].filter(([, list]) => list.length).map(([server, list], index) => {
        const shown = list.filter((tool) => matches(tool.name, tool.description));
        return (
          <section className="ix-group" key={server}>
            {index === 0 && (
              <>
                <ViewBar
                  views={[
                    { id: "user", label: "Just me" },
                    { id: "project", label: "This project" },
                  ]}
                  view={scope}
                  onView={setScope}
                >
                  <Find box={findBox} label="Find a tool" value={query} onChange={onQuery} />
                </ViewBar>
                <p className="quiet tool-scope">{SCOPE_LINE[scope]}</p>
              </>
            )}
            <h3>
              {server || "Medha’s tools"} <span>{list.filter((tool) => !tool.off.length).length} of {list.length} on</span>
            </h3>
            <div className="ix">{shown.map(row)}</div>
          </section>
        );
      })}
    </>
  );
}
