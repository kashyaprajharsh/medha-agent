import { useEffect, useState, type ReactNode } from "react";
import { desktop } from "./api";
import { Icon } from "./Icon";
import { PeopleSheet, ProjectsSheet, type Asking, type Person } from "./MessagingLists";
import { PlatformSheet, Plate, cleaned, named, type Platform } from "./MessagingSetup";
import { Sheet } from "./Sheet";
import { useWorkspace, useWorkspaceApi } from "./Workspace";

export type Overview = {
  folder: string | null;
  /** Every folder a phone may work in, the default first. */
  folders: string[];
  /** Tool steps shown in the chat as the work goes on. */
  steps: boolean;
  running: boolean;
  installed: boolean;
  platforms: Platform[];
  people: Person[];
  waiting: Asking[];
};

/** What the side sheet shows; one at a time, as on the Extensions pages. */
type Open = { at: "platform"; name: string } | { at: "projects" } | { at: "people" };

export function Messaging() {
  const { workspace: here } = useWorkspace();
  const api = useWorkspaceApi();
  const [overview, setOverview] = useState<Overview>();
  const [problem, setProblem] = useState<string>();
  const [open, setOpen] = useState<Open>();
  const [switching, setSwitching] = useState(false);

  async function ask(method: string, params: Record<string, unknown>) {
    try {
      const answer = await api.settings(method, params);
      setProblem(undefined);
      return answer;
    } catch (cause) {
      setProblem(cleaned(cause));
      return undefined;
    }
  }
  const read = async () => {
    const answer = await ask("settings.messaging.status", {});
    if (answer) setOverview(answer as Overview);
  };
  useEffect(() => {
    void read();
    // Someone asking to talk to Medha shows up without a reload.
    const every = setInterval(() => void read(), 10_000);
    return () => clearInterval(every);
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  async function gateway(action: "start" | "stop") {
    setSwitching(true);
    const answer = await ask("settings.messaging.gateway", { action });
    if (answer) setOverview(answer as Overview);
    setSwitching(false);
  }
  async function showSteps(show: boolean) {
    setSwitching(true);
    const answer = await ask("settings.messaging.steps", { show });
    if (answer) setOverview(answer as Overview);
    setSwitching(false);
  }
  async function changeFolder(change: "default" | "add" | "remove", folder: string) {
    const answer = await ask("settings.messaging.folder", { change, folder });
    if (answer) setOverview(answer as Overview);
  }
  async function addFolder() {
    const folder = await desktop.chooseFolder("Choose a project your phone can use");
    if (folder) await changeFolder("add", folder);
  }
  async function approve(platform: string, code: string) {
    const answer = await ask("settings.messaging.approve", { platform, code });
    if (answer) await read();
    return Boolean(answer);
  }
  async function revoke(person: Person) {
    if (await ask("settings.messaging.revoke", { platform: person.platform, user: person.user })) await read();
  }

  const platforms = overview?.platforms ?? [];
  const ready = platforms.filter((platform) => platform.ready);
  const set = ready.length > 0 || Boolean(overview?.running);
  // Before the first setup, the open project; the Personal space holds chats, not a project.
  const suggested = overview?.folder ?? (here.personal ? null : here.path);
  const notReady = platforms.filter((platform) => !platform.ready);
  // As the Connectors page lists apps: the mark, the name, a line on where it stands.
  const appRow = (platform: Platform) => (
    <button
      key={platform.name}
      className={`cx-row${platform.ready ? " cx-ledger" : ""}`}
      onClick={() => setOpen({ at: "platform", name: platform.name })}
      aria-label={`${platform.label}: ${platform.ready ? "ready" : "set up"}`}
    >
      <Plate platform={platform} />
      <span className="cx-text">
        <span className="cx-name">{platform.label}</span>
        <span className="cx-desc">
          {platform.ready
            ? `${platform.account ?? "Set up"}${platform.owner_only ? ", only you can message it" : ""}`
            : platform.one_tap
              ? "Make a bot of your own by scanning a code"
              : "Connect a bot you made"}
        </span>
      </span>
      {platform.ready ? (
        <span className="cx-state">
          <span className="dot" />
          Ready
        </span>
      ) : (
        <span className="cx-fix">Set up</span>
      )}
    </button>
  );
  const opened = open?.at === "platform" ? platforms.find((platform) => platform.name === open.name) : undefined;
  const waiting = overview?.waiting.length ?? 0;

  const row = (label: string, summary: string, end: ReactNode, onOpen: () => void) => (
    <div className="ix-row ms-index">
      <button className="ix-main" onClick={onOpen}>
        <b>{label}</b>
        <span className="ix-sum">{summary}</span>
      </button>
      {end}
      <span className="ms-go" aria-hidden="true">
        <Icon name="chev" />
      </span>
    </div>
  );

  return (
    <div className="app-page">
      <div className="page-content ms-page">
        <div className="ms-top">
          <div>
            <h2>Messaging</h2>
            <p className="cx-lede">
              Talk to Medha from your phone. It answers from this computer, working only in the projects you choose.
            </p>
          </div>
          {overview && set && (
            <div className="ms-gateway" aria-live="polite">
              <span className={`ms-dot${overview.running ? " on" : ""}`} />
              <span>
                <b>{overview.running ? "Running" : "Stopped"}</b>
                <small>
                  {overview.running
                    ? overview.installed
                      ? "Starts at login"
                      : "In a terminal"
                    : "Messages wait until it starts"}
                </small>
              </span>
              <button
                type="button"
                className="btn-line"
                disabled={switching}
                onClick={() => void gateway(overview.running ? "stop" : "start")}
              >
                {overview.running ? "Stop" : "Start"}
              </button>
            </div>
          )}
        </div>
        {problem && (
          <p className="surface-error" role="alert">
            {problem}
          </p>
        )}
        {!overview && !problem && <p className="quiet">Reading what is set up…</p>}
        {ready.length > 0 && (
          <section className="cx-section">
            <h3>
              Connected <span>{ready.length}</span>
            </h3>
            <div className="cx-rows one">{ready.map(appRow)}</div>
          </section>
        )}
        {notReady.length > 0 && (
          <section className="cx-section">
            <h3>{ready.length ? "More apps" : "Apps"}</h3>
            <div className={`cx-rows${notReady.length === 1 ? " one" : ""}`}>{notReady.map(appRow)}</div>
          </section>
        )}
        {overview && set && (
          <section className="cx-section">
            <h3>Chats</h3>
            <div className="ix">
              {overview.folders.length > 0 &&
                row(
                  "Projects",
                  `${named(overview.folders[0])}, where new chats start${
                    overview.folders.length > 1 ? `, and ${overview.folders.length - 1} more` : ""
                  }`,
                  <span className="ix-note">{overview.folders.length}</span>,
                  () => setOpen({ at: "projects" }),
                )}
              {row(
                "People",
                overview.people.length ? overview.people.map((person) => person.name).join(", ") : "Nobody yet",
                waiting ? <span className="ix-chip ms-starts">{waiting} waiting</span> : null,
                () => setOpen({ at: "people" }),
              )}
              <div className="ix-row ms-index ms-static">
                <div className="ix-main">
                  <b>Tool steps</b>
                  <span className="ix-sum">
                    {overview.steps ? "Each step shows in the chat as it runs" : "Chats show replies and approvals only"}
                  </span>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={overview.steps}
                  aria-label="Show tool steps in chats"
                  className="switch"
                  disabled={switching}
                  onClick={() => void showSteps(!overview.steps)}
                />
              </div>
            </div>
          </section>
        )}
      </div>

      {opened && (
        <Sheet label={opened.label} onClose={() => setOpen(undefined)}>
          <PlatformSheet platform={opened} suggested={suggested} onChanged={() => void read()} />
        </Sheet>
      )}
      {overview && open?.at === "projects" && (
        <Sheet label="Projects" onClose={() => setOpen(undefined)}>
          <ProjectsSheet
            folders={overview.folders}
            current={overview.folder}
            onDefault={(folder) => void changeFolder("default", folder)}
            onRemove={(folder) => void changeFolder("remove", folder)}
            onAdd={() => void addFolder()}
          />
          {problem && (
            <p className="cx-problem" role="alert">
              {problem}
            </p>
          )}
        </Sheet>
      )}
      {overview && open?.at === "people" && (
        <Sheet label="People" onClose={() => setOpen(undefined)}>
          <PeopleSheet
            platforms={platforms}
            people={overview.people}
            waiting={overview.waiting}
            onApprove={approve}
            onRemove={(person) => void revoke(person)}
          />
          {problem && (
            <p className="cx-problem" role="alert">
              {problem}
            </p>
          )}
        </Sheet>
      )}
    </div>
  );
}
