import { useEffect, useState } from "react";
import { Icon } from "./Icon";
import { forgetSkillCatalog } from "./SkillBrowse";
import { Spinner } from "./Spinner";
import { useWorkspace } from "./Workspace";

type Source = { key: string; repo: string; path: string; ref: string | null; built_in: boolean };
type Outcome = { name: string; status: string; to?: string; detail?: string; caution?: boolean };
type Lockfile = { path: string; exists: boolean; entries: number };

const UPDATE_TEXT: Record<string, string> = {
  up_to_date: "Up to date",
  available: "Update available",
  updated: "Updated",
  modified: "Edited on this computer, so updates are skipped",
  current: "Already matches the shared set",
  installed: "Installed from the shared set",
  synced: "Restored to the shared version",
};

function describe(outcome: Outcome) {
  const text = UPDATE_TEXT[outcome.status] ?? outcome.detail ?? outcome.status;
  const said = outcome.status === "unmanaged" && outcome.detail ? outcome.detail[0].toUpperCase() + outcome.detail.slice(1) : text;
  return outcome.caution ? `${said}. Turned off until you review what the scan found.` : said;
}

/** Keeping installed skills current, choosing where to browse from, and sharing a set with a team. */
export function SkillManage({ busy, onChanged }: { busy: boolean; onChanged: () => void }) {
  const { api, workspace } = useWorkspace();
  const [sources, setSources] = useState<Source[]>([]);
  const [lockfile, setLockfile] = useState<Lockfile>();
  const [updates, setUpdates] = useState<Outcome[]>();
  const [synced, setSynced] = useState<Outcome[]>();
  const [spec, setSpec] = useState("");
  const [working, setWorking] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let active = true;
    void Promise.all([api.extensions("extensions.skills.sources"), api.extensions("extensions.skills.lockfile")])
      .then(([listed, lock]) => {
        if (!active) return;
        setSources(listed.sources as Source[]);
        setLockfile(lock as unknown as Lockfile);
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [reload]);

  async function run(label: string, method: string, params: Record<string, unknown> = {}) {
    setWorking(label);
    setError("");
    setNotice("");
    try {
      return await api.extensions(method, params);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setWorking("");
    }
  }

  async function check(apply: boolean, name?: string) {
    const result = await run(name ? `update:${name}` : apply ? "update-all" : "check", "extensions.skills.updates", { apply, ...(name && { name }) });
    if (!result) return;
    const fresh = result.results as Outcome[];
    setUpdates((before) => (name && before ? before.map((row) => fresh.find((next) => next.name === row.name) ?? row) : fresh));
    if (apply) onChanged();
  }

  const available = updates?.filter((row) => row.status === "available") ?? [];
  const disabled = busy || Boolean(working);
  return (
    <div className="skill-manage">
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      {notice && (
        <p className="quiet" role="status">
          {notice}
        </p>
      )}

      <section className="manage-section">
        <header>
          <div>
            <h2>Updates</h2>
            <p>Skills you installed from GitHub update from their source. Skills you edited here are never overwritten.</p>
          </div>
          <div className="manage-actions">
            {available.length > 1 && (
              <button className="btn-line" disabled={disabled} onClick={() => void check(true)}>
                {working === "update-all" ? <Spinner /> : null}
                Update all {available.length}
              </button>
            )}
            <button className="btn-line" disabled={disabled} onClick={() => void check(false)}>
              {working === "check" ? <Spinner /> : <Icon name="refresh" />}
              Check for updates
            </button>
          </div>
        </header>
        {updates && (
          <ul className="manage-list">
            {updates.map((row) => (
              <li key={row.name}>
                <b>{row.name}</b>
                <span className={`manage-status status-${row.status}`}>{describe(row)}</span>
                {row.status === "available" && (
                  <button className="btn-line" disabled={disabled} onClick={() => void check(true, row.name)}>
                    {working === `update:${row.name}` ? "Updating…" : "Update"}
                  </button>
                )}
              </li>
            ))}
            {!updates.length && <li className="quiet">You haven't installed any skills from a source yet.</li>}
          </ul>
        )}
      </section>

      <section className="manage-section">
        <header>
          <div>
            <h2>Sources</h2>
            <p>GitHub repositories Browse lists skills from.</p>
          </div>
        </header>
        <ul className="manage-list">
          {sources.map((source) => (
            <li key={source.key}>
              <b>{source.repo}</b>
              <span className="manage-status">
                {source.path !== "skills" && `${source.path} `}
                {source.ref && `@${source.ref} `}
                {source.built_in && "Comes with Medha"}
              </span>
              {!source.built_in && (
                <button
                  className="btn-line"
                  disabled={disabled}
                  onClick={() =>
                    void run(`remove:${source.key}`, "extensions.skills.sources.remove", { key: source.key }).then((done) => {
                      if (!done) return;
                      forgetSkillCatalog();
                      setReload((value) => value + 1);
                    })
                  }
                >
                  Remove
                </button>
              )}
            </li>
          ))}
        </ul>
        <form
          className="plugin-install"
          onSubmit={(event) => {
            event.preventDefault();
            void run("add-source", "extensions.skills.sources.add", { spec }).then((done) => {
              if (!done) return;
              forgetSkillCatalog();
              setSpec("");
              setNotice(done.added ? `Added ${done.key}. Its skills now appear in Browse.` : `Updated ${done.key}.`);
              setReload((value) => value + 1);
            });
          }}
        >
          <label>
            <Icon name="plus" />
            <input required aria-label="Skill source" value={spec} onChange={(event) => setSpec(event.target.value)} placeholder="owner/repo, optionally with a folder or @branch" />
          </label>
          <button className="btn-line" disabled={disabled || !spec.trim()}>
            {working === "add-source" ? "Adding…" : "Add source"}
          </button>
        </form>
      </section>

      <section className="manage-section">
        <header>
          <div>
            <h2>Share with your team</h2>
            {workspace.personal ? (
              <p>Open a project folder to save its skill set in a file your team can commit and restore.</p>
            ) : (
              <p>
                Saves your installed skills, pinned to their exact versions, in <code>medha-skills.lock</code> in this folder. Commit it, and teammates restore the same set.
              </p>
            )}
          </div>
          {!workspace.personal && (
            <div className="manage-actions">
              <button
                className="btn-line"
                disabled={disabled || !lockfile?.exists}
                title={lockfile?.exists ? undefined : "No medha-skills.lock in this folder yet"}
                onClick={() =>
                  void run("sync", "extensions.skills.sync").then((done) => {
                    if (!done) return;
                    setSynced(done.results as Outcome[]);
                    onChanged();
                  })
                }
              >
                {working === "sync" ? <Spinner /> : null}
                Restore from file
              </button>
              <button
                className="btn-line"
                disabled={disabled}
                onClick={() =>
                  void run("lock", "extensions.skills.lock").then((done) => {
                    if (!done) return;
                    setNotice(`Saved ${done.count} ${done.count === 1 ? "skill" : "skills"} to medha-skills.lock.`);
                    setReload((value) => value + 1);
                  })
                }
              >
                {working === "lock" ? <Spinner /> : null}
                Save this set
              </button>
            </div>
          )}
        </header>
        {!workspace.personal && lockfile?.exists && !synced && (
          <p className="manage-note">
            This folder has a shared set of {lockfile.entries} {lockfile.entries === 1 ? "skill" : "skills"}.
          </p>
        )}
        {synced && (
          <ul className="manage-list">
            {synced.map((row) => (
              <li key={row.name}>
                <b>{row.name}</b>
                <span className={`manage-status status-${row.status}`}>{describe(row)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
