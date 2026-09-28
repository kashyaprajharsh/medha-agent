import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";

type Tally = {
  calls: number;
  prompt_tokens: number;
  completion_tokens: number;
  cached_tokens: number;
  cost_usd: number | null;
  unpriced_calls: number;
};
type Summary = {
  total: Tally;
  models: { model: string; usage: Tally }[];
  sessions: { id: string; title: string; agents: number; usage: Tally }[];
  by_day: { day: string; usage: Tally }[];
};

const RANGES = [7, 30, 90];

function tokens(count: number) {
  if (count < 1000) return String(count);
  if (count < 1_000_000) return `${(count / 1000).toFixed(1)}k`;
  return `${(count / 1_000_000).toFixed(2)}M`;
}

function cost(usage: Tally) {
  if (usage.cost_usd === null) return "—";
  const text = usage.cost_usd < 0.01 && usage.cost_usd > 0 ? "<$0.01" : `$${usage.cost_usd.toFixed(2)}`;
  return usage.unpriced_calls ? `${text}+` : text;
}

/** Tokens and cost for this workspace, from each model call Medha recorded. */
export function Usage() {
  const api = useWorkspaceApi();
  const [days, setDays] = useState(30);
  const [summary, setSummary] = useState<Summary>();
  const [error, setError] = useState("");

  useEffect(() => {
    let active = true;
    setError("");
    void api
      .usage(days)
      .then((result) => active && setSummary(result as unknown as Summary))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [days]);

  const total = summary?.total;
  const peak = Math.max(1, ...(summary?.by_day ?? []).map((row) => row.usage.prompt_tokens + row.usage.completion_tokens));
  return (
    <div className="usage-page">
      <div className="page-heading">
        <div>
          <h2>Usage</h2>
          <p>Tokens and cost from each model call in this workspace, sub-agents included.</p>
        </div>
        <div className="segmented small">
          {RANGES.map((range) => (
            <button key={range} aria-pressed={days === range} onClick={() => setDays(range)}>
              {range} days
            </button>
          ))}
        </div>
      </div>
      {error && <p className="surface-error" role="alert">{error}</p>}
      {!summary && !error && <p className="quiet">Reading usage…</p>}
      {total && !total.calls && <p className="quiet">No model calls recorded in the last {days} days.</p>}
      {total && total.calls > 0 && (
        <>
          <dl className="usage-totals">
            <div><dt>Input</dt><dd>{tokens(total.prompt_tokens)}</dd></div>
            <div><dt>Cached</dt><dd>{tokens(total.cached_tokens)}</dd></div>
            <div><dt>Output</dt><dd>{tokens(total.completion_tokens)}</dd></div>
            <div><dt>Cost</dt><dd>{cost(total)}</dd></div>
            <div><dt>Model calls</dt><dd>{total.calls}</dd></div>
          </dl>
          {total.unpriced_calls > 0 && (
            <p className="quiet">
              {total.unpriced_calls} {total.unpriced_calls === 1 ? "call has" : "calls have"} no known price — local models, or calls from before cost was recorded.
            </p>
          )}
          <div className="usage-days" aria-label="Tokens per day">
            {summary.by_day.map((row) => {
              const used = row.usage.prompt_tokens + row.usage.completion_tokens;
              return (
                <span key={row.day} title={`${row.day}: ${tokens(used)} tokens · ${cost(row.usage)}`}>
                  <i style={{ height: `${Math.max(4, (used / peak) * 100)}%` }} />
                </span>
              );
            })}
          </div>
          <section className="usage-block">
            <h3>By model</h3>
            <table className="usage-table">
              <thead>
                <tr><th>Model</th><th>Calls</th><th>Input</th><th>Output</th><th>Cost</th></tr>
              </thead>
              <tbody>
                {summary.models.map((row) => (
                  <tr key={row.model}>
                    <td><code>{row.model || "unknown"}</code></td>
                    <td>{row.usage.calls}</td>
                    <td>{tokens(row.usage.prompt_tokens)}</td>
                    <td>{tokens(row.usage.completion_tokens)}</td>
                    <td>{cost(row.usage)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </section>
          <section className="usage-block">
            <h3>Chats that used the most</h3>
            <ul className="usage-sessions">
              {summary.sessions.map((row) => (
                <li key={row.id}>
                  <span>{row.title || "Untitled chat"}</span>
                  <small>
                    {tokens(row.usage.prompt_tokens + row.usage.completion_tokens)} tokens · {cost(row.usage)}
                    {row.agents > 0 && ` · ${row.agents} ${row.agents === 1 ? "sub-agent" : "sub-agents"}`}
                  </small>
                </li>
              ))}
            </ul>
          </section>
          <p className="quiet">Calls from before usage was recorded count prompt tokens only.</p>
        </>
      )}
    </div>
  );
}
