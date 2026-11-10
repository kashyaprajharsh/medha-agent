/** What a finished turn used and cost, as the backend sends it with `turn.done`. */
export type TurnSummary = {
  seconds: number;
  calls: number;
  input_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  cost_usd?: number;
  priced: "own" | "list" | "partly" | "none";
};

function duration(seconds: number) {
  const whole = Math.round(seconds);
  if (whole < 60) return `${whole}s`;
  if (whole < 3600) return `${Math.floor(whole / 60)}m ${whole % 60}s`;
  return `${Math.floor(whole / 3600)}h ${Math.floor((whole % 3600) / 60)}m`;
}

function tokens(count: number) {
  if (count < 1_000) return String(count);
  if (count < 10_000) return `${(count / 1_000).toFixed(1).replace(/\.0$/, "")}k`;
  if (count < 1_000_000) return `${Math.floor(count / 1_000)}k`;
  return `${(count / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
}

function dollars(cost: number) {
  return cost < 0.01 ? `$${cost.toFixed(4)}` : `$${cost.toFixed(2)}`;
}

/** The same line the terminal shows, e.g. `4m 12s · 9 calls · 182k in (151k cached) · 6.1k out · $0.41 list price`. */
export function summaryLine(summary: TurnSummary) {
  const input = tokens(summary.input_tokens);
  const cost =
    summary.cost_usd === undefined || summary.priced === "none"
      ? "not priced"
      : summary.priced === "list"
        ? `${dollars(summary.cost_usd)} list price`
        : summary.priced === "partly"
          ? `${dollars(summary.cost_usd)} for the priced calls`
          : dollars(summary.cost_usd);
  return [
    duration(summary.seconds),
    summary.calls === 1 ? "1 call" : `${summary.calls} calls`,
    summary.cached_tokens ? `${input} in (${tokens(summary.cached_tokens)} cached)` : `${input} in`,
    `${tokens(summary.output_tokens)} out`,
    cost,
  ].join(" · ");
}
