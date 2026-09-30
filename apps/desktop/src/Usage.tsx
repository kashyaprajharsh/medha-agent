import { useEffect, useMemo, useState } from "react";
import { Select } from "./Select";
import { SLOTS, UsageChart, type Day, type Series } from "./UsageChart";
import { useWorkspaceApi } from "./Workspace";

type Tally = {
  calls: number;
  prompt_tokens: number;
  completion_tokens: number;
  cached_tokens: number;
  cost_usd: number | null;
  unpriced_calls: number;
};
type ByModel = { model: string; usage: Tally }[];
type Summary = {
  total: Tally;
  models: ByModel;
  sessions: { id: string; title: string; agents: number; usage: Tally; models?: ByModel }[];
  by_day: { day: string; usage: Tally; models?: ByModel }[];
};

const RANGES = [7, 30, 90];
const ALL = "";
const OTHER = "Other models";
const DAILY_TOTAL = "Daily total";

function tokens(count: number) {
  if (count < 1000) return String(Math.round(count));
  if (count < 1_000_000) return `${(count / 1000).toFixed(1)}k`;
  return `${(count / 1_000_000).toFixed(2)}M`;
}

function cost(usage: Tally) {
  if (usage.cost_usd === null) return "—";
  const text = usage.cost_usd < 0.01 && usage.cost_usd > 0 ? "<$0.01" : `$${usage.cost_usd.toFixed(2)}`;
  return usage.unpriced_calls ? `${text}+` : text;
}

const used = (usage: Tally) => usage.prompt_tokens + usage.completion_tokens;

function sum(rows: Tally[]): Tally {
  const priced = rows.filter((row) => row.cost_usd !== null);
  return {
    calls: rows.reduce((total, row) => total + row.calls, 0),
    prompt_tokens: rows.reduce((total, row) => total + row.prompt_tokens, 0),
    completion_tokens: rows.reduce((total, row) => total + row.completion_tokens, 0),
    cached_tokens: rows.reduce((total, row) => total + row.cached_tokens, 0),
    cost_usd: priced.length ? priced.reduce((total, row) => total + (row.cost_usd ?? 0), 0) : null,
    unpriced_calls: rows.reduce((total, row) => total + row.unpriced_calls, 0),
  };
}

/** Every local day in the window, oldest first, so empty days still take their place. */
function localDays(days: number) {
  const today = new Date();
  return Array.from({ length: days }, (_, index) => {
    const day = new Date(today.getFullYear(), today.getMonth(), today.getDate() - (days - 1 - index));
    return `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, "0")}-${String(day.getDate()).padStart(2, "0")}`;
  });
}

/** Tokens and cost for this workspace, from each model call Medha recorded. */
export function Usage() {
  const api = useWorkspaceApi();
  const [days, setDays] = useState(30);
  const [model, setModel] = useState(ALL);
  const [summary, setSummary] = useState<Summary>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    let active = true;
    setError("");
    setLoading(true);
    void api
      .usage(days)
      .then((result) => active && setSummary(result as unknown as Summary))
      .catch((cause) => active && setError(String(cause)))
      .finally(() => active && setLoading(false));
    return () => {
      active = false;
    };
  }, [days]);

  const view = useMemo(() => {
    if (!summary) return undefined;
    const complete = (row: { usage: Tally; models?: ByModel }) =>
      row.usage.calls === 0 || (row.models?.reduce((count, entry) => count + entry.usage.calls, 0) === row.usage.calls);
    const dailyBreakdown = summary.by_day.every(complete);
    const canFilter = dailyBreakdown && summary.sessions.every(complete);
    const selected = canFilter && summary.models.some((row) => row.model === model) ? model : ALL;
    const picked = (rows: ByModel | undefined) => (rows ?? []).filter((row) => !selected || row.model === selected);
    const series: Series[] = dailyBreakdown ? summary.models
      .map((row, rank) => ({ key: row.model, label: row.model || "unknown", slot: rank }))
      .filter((item) => !selected || item.key === selected)
      .map((item) => (item.slot < SLOTS ? item : { key: OTHER, label: OTHER, slot: SLOTS }))
      .filter((item, index, all) => all.findIndex((other) => other.key === item.key) === index)
      : [{ key: DAILY_TOTAL, label: DAILY_TOTAL, slot: SLOTS }];
    const byDay = new Map(summary.by_day.map((row) => [row.day, row]));
    const chart: Day[] = localDays(days).map((day) => {
      const entry = byDay.get(day);
      if (!dailyBreakdown) {
        return { day, values: entry ? { [DAILY_TOTAL]: used(entry.usage) } : {}, cost: entry ? cost(entry.usage) : "—" };
      }
      const rows = picked(entry?.models);
      const values: Record<string, number> = {};
      for (const row of rows) {
        const rank = summary.models.findIndex((entry) => entry.model === row.model);
        const key = rank >= SLOTS ? OTHER : row.model;
        values[key] = (values[key] ?? 0) + used(row.usage);
      }
      return { day, values, cost: cost(sum(rows.map((row) => row.usage))) };
    });
    const models = picked(summary.models);
    const sessions = selected
      ? summary.sessions
          .map((row) => ({ ...row, usage: picked(row.models)[0]?.usage }))
          .filter((row): row is typeof row & { usage: Tally } => Boolean(row.usage))
          .sort((a, b) => used(b.usage) - used(a.usage))
      : summary.sessions;
    return { series, chart, models, sessions, canFilter, dailyBreakdown, selected, total: selected ? sum(models.map((row) => row.usage)) : summary.total };
  }, [summary, model, days]);

  const total = view?.total;
  return (
    <div className="usage-page">
      <div className="page-heading">
        <div>
          <h2>Usage</h2>
          <p>Tokens and cost from each model call in this workspace, sub-agents included.</p>
        </div>
      </div>
      <div className="usage-filters">
        <div className="segmented small" aria-label="Time range">
          {RANGES.map((range) => (
            <button key={range} aria-pressed={days === range} onClick={() => setDays(range)}>
              {range} days
            </button>
          ))}
        </div>
        {summary && summary.models.length > 1 && (
          <Select value={view?.selected ?? ALL} disabled={!view?.canFilter} onChange={(event) => setModel(event.target.value)} aria-label="Model">
            <option value={ALL}>All models</option>
            {summary.models.map((row) => (
              <option key={row.model} value={row.model}>{row.model || "unknown"}</option>
            ))}
          </Select>
        )}
      </div>
      {view && !view.canFilter && (
        <p className="quiet">Showing all models. Some recorded usage has no per-model breakdown.</p>
      )}
      {error && <p className="surface-error" role="alert">{error}</p>}
      {!summary && !error && <p className="quiet">Reading usage…</p>}
      {total && !total.calls && <p className="quiet">No model calls recorded in the last {days} days.</p>}
      {view && total && total.calls > 0 && (
        <div className={loading ? "usage-body refreshing" : "usage-body"}>
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
          <UsageChart days={view.chart} series={view.series} format={tokens} />
          <details className="usage-daily">
            <summary>Daily numbers</summary>
            <table className="usage-table">
              <thead>
                <tr><th>Day</th>{view.series.map((item) => <th key={item.key}>{item.label}</th>)}<th>Cost</th></tr>
              </thead>
              <tbody>
                {view.chart.filter((day) => Object.keys(day.values).length).reverse().map((day) => (
                  <tr key={day.day}>
                    <td>{day.day}</td>
                    {view.series.map((item) => <td key={item.key}>{tokens(day.values[item.key] ?? 0)}</td>)}
                    <td>{day.cost}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </details>
          <section className="usage-block">
            <h3>By model</h3>
            <table className="usage-table">
              <thead>
                <tr><th>Model</th><th>Calls</th><th>Input</th><th>Output</th><th>Cost</th></tr>
              </thead>
              <tbody>
                {view.models.map((row) => (
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
              {view.sessions.map((row) => (
                <li key={row.id}>
                  <span>{row.title || "Untitled chat"}</span>
                  <small>
                    {tokens(used(row.usage))} tokens · {cost(row.usage)}
                    {row.agents > 0 && ` · ${row.agents} ${row.agents === 1 ? "sub-agent" : "sub-agents"}`}
                  </small>
                </li>
              ))}
            </ul>
          </section>
          <p className="quiet">Calls from before usage was recorded count prompt tokens only.</p>
        </div>
      )}
    </div>
  );
}
