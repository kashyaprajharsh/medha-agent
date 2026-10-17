import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";

export type OutboundReview = { id: string; destination: string; reason: string; preview: string };

export function OutboundNotice({ review: candidate }: { review: { id: string } }) {
  const api = useWorkspaceApi();
  const [answered, setAnswered] = useState(false);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [review, setReview] = useState<OutboundReview>();
  useEffect(() => {
    let active = true;
    if (!/^[0-9a-f]{64}$/.test(candidate.id)) return;
    api.settings("session.inspect", { query: "outbound" }).then(reply => {
      const reviews: OutboundReview[] = JSON.parse(String(reply.data ?? "[]"));
      if (active) setReview(reviews.find(review => review.id === candidate.id));
    }).catch(error => { if (active) setError(String(error)); });
    return () => { active = false; };
  }, [api, candidate.id]);
  if (answered || !review) return null;
  async function allow() {
    setBusy(true);
    try {
      await api.settings("session.change", { action: "allow_outbound", data: { id: candidate.id } });
      setAnswered(true);
    } catch (error) { setError(String(error)); }
    finally { setBusy(false); }
  }
  async function dismiss() {
    setBusy(true);
    try {
      await api.settings("session.change", { action: "dismiss_outbound", data: { id: candidate.id } });
      setAnswered(true);
    } catch (error) { setError(String(error)); }
    finally { setBusy(false); }
  }
  return <div className="project-trust quiet" role="status">
    <p className="project-trust-title">Outbound request blocked</p>
    <p>This request {review.reason}. It has not been sent.</p>
    <code>{review.destination}</code>
    <details><summary>Review payload</summary><pre>{review.preview}</pre><small>{review.id}</small></details>
    <div className="project-trust-actions">
      <button type="button" className="btn-line" disabled={busy} onClick={allow}>Allow this exact payload once</button>
      <button type="button" className="btn-line" disabled={busy} onClick={dismiss}>Dismiss</button>
    </div>
    <p>Approval permits one retry of this exact payload. It does not allow other requests.</p>
    {error && <p role="alert">{error}</p>}
  </div>;
}
