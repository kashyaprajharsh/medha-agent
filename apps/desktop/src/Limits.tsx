import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";

type Values = Record<string, number | null>;

type Limit = { key: string; label: string; text: string; step?: string };

const MAIN: Limit[] = [
  { key: "max_cost_usd", label: "Cost, in US dollars", text: "Stop work on a message past this cost, when the model’s price is known.", step: "any" },
  { key: "max_wall_s", label: "Time, in seconds", text: "Stop work on a message after this long." },
];
const MORE: Limit[] = [
  { key: "max_turns", label: "Steps", text: "Stop work on a message after this many steps. A step is the model replying once, usually to use a tool." },
  { key: "max_tokens", label: "Tokens", text: "Stop work on a message after using this many tokens." },
  { key: "background_wakes", label: "Replies to finished commands", text: "How many times in a row Medha may reply by itself when a background command finishes." },
  { key: "max_parallel_tools", label: "Tools at once", text: "How many tools may run at the same time." },
  { key: "tool_result_bytes", label: "Largest tool result, in bytes", text: "Longer results are shortened for the model, which can read the rest if it needs to." },
  { key: "tool_round_bytes", label: "Largest set of results, in bytes", text: "The same, for all the results of one step together." },
];

/** The project's `[budget]` in `medha.lock`, for people who do not edit that file by hand. */
export function Limits() {
  const api = useWorkspaceApi();
  const [values, setValues] = useState<Values>();
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [error, setError] = useState("");
  const [saved, setSaved] = useState("");

  async function load() {
    const result = (await api.settings("settings.budget")) as unknown as { limits: Values };
    setValues(result.limits);
  }

  useEffect(() => {
    load().catch((cause) => setError(String(cause)));
  }, []);

  async function save(key: string, label: string) {
    const draft = drafts[key];
    if (draft === undefined) return;
    const number = Number(draft);
    setError("");
    setSaved("");
    try {
      await api.settings("settings.budget.save", { key, value: draft.trim() === "" ? null : number });
      await load();
      setDrafts(({ [key]: _done, ...rest }) => rest);
      setSaved(`${label} saved.`);
    } catch (cause) {
      setError(String(cause));
    }
  }

  if (!values) return error ? <p className="surface-error">{error}</p> : null;
  const row = (limit: Limit) => (
    <label key={limit.key}>
      <span>
        <b>{limit.label}</b>
        <small>{limit.text}</small>
      </span>
      <input
        type="number"
        min="0"
        step={limit.step ?? "1"}
        placeholder={MAIN.includes(limit) ? "No limit" : "Automatic"}
        value={drafts[limit.key] ?? values[limit.key] ?? ""}
        onChange={(event) => {
          const value = event.target.value;
          setDrafts((current) => ({ ...current, [limit.key]: value }));
        }}
        onBlur={() => void save(limit.key, limit.label)}
        onKeyDown={(event) => {
          if (event.key === "Enter") event.currentTarget.blur();
        }}
      />
    </label>
  );
  return (
    <details className="limits">
      <summary>
        <b>Limits</b>
        <span>Optional. Stop a long or costly job early.</span>
      </summary>
      <div className="limits-list">{MAIN.map(row)}</div>
      <details className="limits-more">
        <summary>More limits</summary>
        <div className="limits-list">{MORE.map(row)}</div>
      </details>
      <p className="quiet" role="status">
        Empty means Medha’s own default. Applies to new chats in this project.{saved && ` ${saved}`}
      </p>
      {error && <p className="surface-error" role="alert">{error}</p>}
    </details>
  );
}
