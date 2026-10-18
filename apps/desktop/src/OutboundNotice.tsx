import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";

export type OutboundReview = { id: string; destination: string; reason: string; preview: string; allowable?: boolean };

export function OutboundNotice({ review: candidate, kind = "outbound" }: { review: { id: string }; kind?: "outbound" | "protected" }) {
  const api = useWorkspaceApi();
  const [answered, setAnswered] = useState(false);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [review, setReview] = useState<OutboundReview>();
  useEffect(() => {
    let active = true;
    if (!/^[0-9a-f]{64}$/.test(candidate.id)) return;
    api.settings("session.inspect", { query: kind }).then(reply => {
      const reviews: OutboundReview[] = JSON.parse(String(reply.data ?? "[]"));
      if (active) setReview(reviews.find(review => review.id === candidate.id));
    }).catch(error => { if (active) setError(String(error)); });
    return () => { active = false; };
  }, [api, candidate.id, kind]);
  if (answered || !review || review.id !== candidate.id) return null;
  async function allow() {
    setBusy(true);
    try {
      await api.settings("session.change", { action: `allow_${kind}`, data: { id: candidate.id } });
      setAnswered(true);
    } catch (error) { setError(String(error)); }
    finally { setBusy(false); }
  }
  async function dismiss() {
    setBusy(true);
    try {
      await api.settings("session.change", { action: `dismiss_${kind}`, data: { id: candidate.id } });
      setAnswered(true);
    } catch (error) { setError(String(error)); }
    finally { setBusy(false); }
  }
  return <div className="project-trust quiet" role="status">
    <p className="project-trust-title">{kind === "protected" ? "Protected change held" : "Outbound request blocked"}</p>
    <p>{kind === "protected" ? review.reason : `This request ${review.reason}. It has not been sent.`}</p>
    <code>{review.destination}</code>
    <details><summary>{kind === "protected" ? "Review diff or command" : "Review payload"}</summary><pre>{review.preview}</pre><small>{review.id}</small></details>
    <div className="project-trust-actions">
      {review.allowable !== false && <button type="button" className="btn-line" disabled={busy} onClick={allow}>{kind === "protected" ? "Allow once" : "Allow this exact payload once"}</button>}
      <button type="button" className="btn-line" disabled={busy} onClick={dismiss}>Dismiss</button>
    </div>
    {review.allowable !== false && <p>{kind === "protected" ? "Applies only this exact change, if its original content still matches." : "Approval permits one retry of this exact payload. It does not allow other requests."}</p>}
    {error && <p role="alert">{error}</p>}
  </div>;
}
