import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";

export type ProjectTrustReview = {
  root: string;
  folder: string;
  trusted: boolean;
  fingerprint: string;
  settings: { key: string; value: string; effect: string }[];
  ignored?: string[];
};

export function ProjectTrustNotice({ review: shown, session }: { review: ProjectTrustReview; session?: string }) {
  const api = useWorkspaceApi();
  const [review, setReview] = useState(shown);
  const key = `project-trust:${session}:${review.root}:${review.trusted ? review.fingerprint : "folder"}`;
  const [dismissed, setDismissed] = useState(() => localStorage.getItem(key) === "done");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  useEffect(() => {
    let active = true;
    api.settings("application.resources", { resource: "project_trust" }).then(reply => {
      const current = reply.data as ProjectTrustReview | undefined;
      if (active && current?.trusted && current.ignored?.length === 0) { localStorage.setItem(key, "done"); setDismissed(true); }
    }).catch(() => {});
    return () => { active = false; };
  }, [api, key]);
  function dismiss() {
    localStorage.setItem(key, "done");
    setDismissed(true);
  }
  async function trust() {
    setBusy(true);
    try {
      await api.settings("application.resources.change", { action: "trust_project", data: { root: review.root, folder: review.folder, fingerprint: review.fingerprint } });
      dismiss();
    } catch (reason) {
      setError(String(reason));
      const reply = await api.settings("application.resources", { resource: "project_trust" }).catch(() => undefined);
      if (reply?.data) setReview(reply.data as ProjectTrustReview);
    } finally { setBusy(false); }
  }
  if (dismissed) return null;
  return <div className="project-trust quiet" role="status">
    <p className="project-trust-title">{review.trusted ? "Changed project settings were ignored" : "Project content is off for this checkout"}</p>
    <code className="project-trust-folder">{review.root}</code>
    <p>{review.trusted ? "Accept the values below to apply them from your next message." : "Trust enables this checkout's project skills, prompts and language servers, and accepts the settings shown below."}</p>
    {review.settings.length > 0 && <ul className="project-trust-settings">{review.settings.map(setting => <li key={setting.key}><code>{setting.key} = {JSON.stringify(setting.value)}</code><span>{setting.effect}{review.ignored?.includes(setting.key) ? " (ignored until accepted)" : ""}</span></li>)}</ul>}
    <div className="project-trust-actions">
      <button className="btn-line" type="button" disabled={busy} onClick={trust}>{review.trusted ? "Accept shown settings" : "Trust checkout and shown settings"}</button>
      <button className="btn-line" type="button" onClick={dismiss}>Dismiss</button>
    </div>
    {error && <p role="alert">{error}</p>}
  </div>;
}
