import { useEffect, useState } from "react";
import { PageTop } from "./ExtensionParts";
import { useWorkspaceApi } from "./Workspace";

type Waits = Record<string, number | null>;

/** One wait: shown in `unit`, saved as the whole number `config.toml` holds (seconds, or a count). */
type Wait = { key: string; label: string; text: string; unit: string; fallback: number; scale: number; min: number };

const SECTIONS: { title: string; hint: string; waits: Wait[] }[] = [
  {
    title: "Waiting for you",
    hint: "How long Medha waits before it stops asking.",
    waits: [
      { key: "approval_timeout", label: "Approvals", text: "After this, the action is refused. 0 waits until you answer.", unit: "minutes", fallback: 5, scale: 60, min: 0 },
      { key: "question_timeout", label: "Questions", text: "After this, the question is set aside. 0 waits until you answer.", unit: "minutes", fallback: 60, scale: 60, min: 0 },
    ],
  },
  {
    title: "Autopilot",
    hint: "A reviewer answers for you, one action at a time.",
    waits: [{ key: "review_timeout", label: "Reviewer time", text: "After this, you're asked instead.", unit: "seconds", fallback: 30, scale: 1, min: 1 }],
  },
  {
    title: "Model server",
    hint: "When the model's server stops answering for a while.",
    waits: [
      { key: "server_waits", label: "Waits", text: "How many times a message waits for it, after retries and stand-in models. 0 never waits.", unit: "times", fallback: 5, scale: 1, min: 0 },
    ],
  },
];

/** The `[agent]` waits in the person's own `config.toml`, for people who do not edit that file by hand. */
export function Approvals() {
  const api = useWorkspaceApi();
  const [waits, setWaits] = useState<Waits>();
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [error, setError] = useState("");
  const [saved, setSaved] = useState("");

  async function load() {
    const result = (await api.settings("settings.agent")) as unknown as { waits: Waits };
    setWaits(result.waits);
  }

  useEffect(() => {
    load().catch((cause) => setError(String(cause)));
  }, []);

  async function save(wait: Wait) {
    const draft = drafts[wait.key];
    if (draft === undefined) return;
    setError("");
    setSaved("");
    try {
      const value = draft.trim() === "" ? null : Math.round(Number(draft) * wait.scale);
      await api.settings("settings.agent.save", { key: wait.key, value });
      await load();
      setDrafts(({ [wait.key]: _done, ...rest }) => rest);
      setSaved(`${wait.label} saved.`);
    } catch (cause) {
      setError(String(cause));
    }
  }

  if (!waits) return error ? <p className="surface-error">{error}</p> : null;
  const shown = (wait: Wait) => {
    const value = waits[wait.key];
    return value === null || value === undefined ? "" : String(value / wait.scale);
  };
  return (
    <>
      <PageTop title="Approvals">
        What happens when Medha needs your answer, and what Autopilot may answer for you. Applies to new chats.
      </PageTop>
      {SECTIONS.map((section) => (
        <section className="wait-section" key={section.title}>
          <h3>
            {section.title}
            <span>{section.hint}</span>
          </h3>
          <div className="limits-list">
            {section.waits.map((wait) => (
              <label key={wait.key}>
                <span>
                  <b>{wait.label}</b>
                  <small>{wait.text}</small>
                </span>
                <span className="wait-input">
                  <input
                    type="number"
                    min={wait.min}
                    step={wait.scale === 60 ? "any" : "1"}
                    placeholder={String(wait.fallback)}
                    aria-label={`${wait.label}, in ${wait.unit}`}
                    value={drafts[wait.key] ?? shown(wait)}
                    onChange={(event) => {
                      const value = event.target.value;
                      setDrafts((current) => ({ ...current, [wait.key]: value }));
                    }}
                    onBlur={() => void save(wait)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") event.currentTarget.blur();
                    }}
                  />
                  <small>{wait.unit}</small>
                </span>
              </label>
            ))}
          </div>
        </section>
      ))}
      <p className="quiet" role="status">
        Empty means Medha's own default. Saved in your settings, for every project.{saved && ` ${saved}`}
      </p>
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
    </>
  );
}
