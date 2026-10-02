import { useEffect, useState } from "react";
import { Add, PageTop, ViewBar } from "./ExtensionParts";
import { Icon } from "./Icon";
import { Select } from "./Select";
import { useWorkspaceApi } from "./Workspace";

type Event = { name: string; description: string; tools: boolean };
type Tool = { matcher: string; label: string };
type Hook = { event: string; file: string; matcher: string; command: string };
type Project = {
  id: string;
  scope: string;
  activation: string;
  hash: string;
  grant: Record<string, unknown> | null;
  blocked?: string;
};
type Listing = { events: Event[]; tools: Tool[]; hooks: Hook[]; project: Project | null };

const sentence = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);

/** Scripts in `.medha/hooks` that run at set moments of Medha's work. New or
 * changed scripts stay off until the person allows the folder again. */
export function Hooks({ locked }: { locked: boolean }) {
  const api = useWorkspaceApi();
  const [listing, setListing] = useState<Listing>();
  const [adding, setAdding] = useState(false);
  // The moment the form opens on, when it was opened from that moment's row.
  const [startAt, setStartAt] = useState("");
  const [reviewing, setReviewing] = useState(false);
  const [removing, setRemoving] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    let active = true;
    void api
      .extensions("extensions.hooks.list")
      .then((result) => active && setListing(result as unknown as Listing))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [refresh]);

  async function run(method: string, params: Record<string, unknown>, done: string) {
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await api.extensions(method, params);
      setNotice(done);
      setRefresh((value) => value + 1);
      window.dispatchEvent(new CustomEvent("medha-preferences"));
      return true;
    } catch (cause) {
      setError(String(cause));
      return false;
    } finally {
      setBusy(false);
    }
  }

  const project = listing?.project;
  const on = project?.activation === "enabled";
  const events = listing?.events ?? [];
  const describe = (name: string) => events.find((event) => event.name === name)?.description ?? name;
  const toolLabel = (matcher: string) => listing?.tools.find((tool) => tool.matcher === matcher)?.label ?? matcher;
  return (
    <div className="hooks-page">
      <PageTop title="Hooks">
        Your own scripts, run at set moments of Medha’s work. They live in this project’s <code>.medha/hooks</code>{" "}
        folder.
      </PageTop>
      <ViewBar>
        <Add label="Add hook" open={adding} onClick={() => !locked && setAdding(!adding)} />
      </ViewBar>
      {error && <p className="surface-error" role="alert">{error}</p>}
      {notice && <p className="quiet" role="status">{notice}</p>}
      {adding && listing && (
        <HookForm
          key={startAt}
          initial={startAt}
          events={events}
          tools={listing.tools}
          busy={busy || locked}
          onSave={async (event, matcher, command) => {
            if (await run("extensions.hooks.add", { event, matcher, command }, "Added. Allow the project’s hooks to turn it on."))
              setAdding(false);
          }}
        />
      )}
      {project && listing.hooks.length > 0 && (
        <div className={`hooks-state ${on ? "on" : "off"}`}>
          <span>
            {on
              ? "On for this project"
              : project.activation.startsWith("changed")
                ? "Off — the hooks changed since you allowed them"
                : "Off until you allow them"}
          </span>
          {project.blocked ? (
            <small>{project.blocked}</small>
          ) : on ? (
            <button className="btn-line" disabled={busy || locked} onClick={() => void run("extensions.disable", { id: project.id, scope: project.scope }, "Hooks turned off.")}>
              Turn off
            </button>
          ) : (
            <button className="btn-gold" disabled={busy || locked} onClick={() => setReviewing(true)}>
              Review and allow
            </button>
          )}
        </div>
      )}
      {listing && listing.hooks.length > 0 && (
        <section className="ix-group">
          <h3>
            In this project <span>{listing.hooks.length}</span>
          </h3>
          <ul className="ix hook-list">
            {listing.hooks.map((hook) => {
              const key = `${hook.event}/${hook.file}`;
              return (
                <li className="ix-row" key={key}>
                  <span className="ix-main">
                    <b>{sentence(describe(hook.event).split(" — ")[0])}</b>
                    <code className="ix-sum mono" title={hook.command}>
                      {hook.command}
                    </code>
                  </span>
                  <small className="ix-note">{hook.matcher === "*" ? hook.event : `${hook.event} · ${toolLabel(hook.matcher)}`}</small>
                  {removing === key ? (
                    <span className="hook-confirm">
                      <button className="btn-line" onClick={() => setRemoving("")}>Keep</button>
                      <button
                        className="btn-line danger"
                        disabled={busy || locked}
                        onClick={() => void run("extensions.hooks.remove", { event: hook.event, file: hook.file }, "Hook removed.").then(() => setRemoving(""))}
                      >
                        Remove
                      </button>
                    </span>
                  ) : (
                    <button className="link-btn" disabled={locked} onClick={() => setRemoving(key)}>
                      Remove
                    </button>
                  )}
                </li>
              );
            })}
          </ul>
        </section>
      )}
      {/* An empty page says what could be here: each moment a script can run at, and what it can do then. */}
      {listing && (
        <section className="ix-group">
          <h3>
            Moments you can use {!listing.hooks.length && <span>none in use yet</span>}
          </h3>
          <div className="ix">
            {events.map((event) => {
              const [when, can] = event.description.split(" — ");
              return (
                <button
                  className="ix-row moment"
                  key={event.name}
                  disabled={locked}
                  aria-label={`Add a hook ${when}`}
                  onClick={() => {
                    setStartAt(event.name);
                    setAdding(true);
                  }}
                >
                  <span className="ix-main">
                    <b>{sentence(when)}</b>
                    <span className="ix-sum">{can ? sentence(can) : ""}</span>
                  </span>
                  <Icon name="plus" />
                </button>
              );
            })}
          </div>
        </section>
      )}
      {reviewing && project && listing && (
        <div className="dialog-backdrop">
          <section className="confirm-dialog" role="alertdialog" aria-modal="true" aria-labelledby="hooks-review">
            <h2 id="hooks-review">Allow this project’s hooks?</h2>
            <p>They run on your machine, in Medha’s sandbox, at these moments:</p>
            <ul className="hook-review">
              {listing.hooks.map((hook) => (
                <li key={`${hook.event}/${hook.file}`}>
                  <b>{sentence(describe(hook.event))}</b>
                  <pre className="io">{hook.command}</pre>
                </li>
              ))}
            </ul>
            <p className="quiet">If a script changes later, the hooks turn off until you allow them again.</p>
            <div className="row-actions">
              <button className="btn-line" onClick={() => setReviewing(false)}>Cancel</button>
              <button
                className="btn-gold"
                disabled={busy || locked}
                onClick={() =>
                  void run("extensions.enable", { id: project.id, scope: project.scope, hash: project.hash, grant: project.grant }, "Hooks are on.").then(() => setReviewing(false))
                }
              >
                Allow
              </button>
            </div>
          </section>
        </div>
      )}
    </div>
  );
}

function HookForm({
  initial,
  events,
  tools,
  busy,
  onSave,
}: {
  initial: string;
  events: Event[];
  tools: Tool[];
  busy: boolean;
  onSave: (event: string, matcher: string, command: string) => Promise<void>;
}) {
  const [event, setEvent] = useState(initial || (events[0]?.name ?? "pre-tool"));
  const [matcher, setMatcher] = useState("*");
  const [command, setCommand] = useState("");
  const chosen = events.find((candidate) => candidate.name === event);
  return (
    <form
      className="settings-form"
      onSubmit={(submit) => {
        submit.preventDefault();
        void onSave(event, chosen?.tools ? matcher : "*", command.trim());
      }}
    >
      <label>
        When
        <Select value={event} onChange={(change) => setEvent(change.target.value)}>
          {events.map((candidate) => (
            <option key={candidate.name} value={candidate.name}>
              {sentence(candidate.description)}
            </option>
          ))}
        </Select>
      </label>
      {chosen?.tools && (
        <label>
          For which tools
          <Select value={matcher} onChange={(change) => setMatcher(change.target.value)}>
            {tools.map((tool) => (
              <option key={tool.matcher} value={tool.matcher}>
                {tool.label}
              </option>
            ))}
          </Select>
        </label>
      )}
      <label>
        Command, or the path of a script to copy in
        <textarea
          required
          className="mono"
          value={command}
          onChange={(change) => setCommand(change.target.value)}
          placeholder="./scripts/check.sh"
          spellCheck={false}
        />
      </label>
      <p className="quiet">Exit 0 lets Medha continue. Exit 2 stops it and shows the script’s message.</p>
      <button className="btn-gold" disabled={busy || !command.trim()}>
        Save hook
      </button>
    </form>
  );
}
