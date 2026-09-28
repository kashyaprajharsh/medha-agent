import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";

type Setting = { preset: string; from: string; env_override: boolean; minimal: string[] };

/** Which tools new chats in this project get: all of them, or the five that
 * can still reach everything. Saved in the project's `medha.lock`. */
export function ToolPreset({ available, locked }: { available: number; locked: boolean }) {
  const api = useWorkspaceApi();
  const [setting, setSetting] = useState<Setting>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    let active = true;
    void api
      .settings("settings.tools")
      .then((result) => active && setSetting(result as unknown as Setting))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [refresh]);

  async function choose(preset: string) {
    if (!setting || preset === setting.preset) return;
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      await api.settings("settings.tools.save", { preset });
      setSaved(true);
      setRefresh((value) => value + 1);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }

  if (!setting) return error ? <p className="surface-error">{error}</p> : null;
  const options = [
    {
      preset: "full",
      title: "All tools",
      text: `Every tool Medha has${available ? ` (${available} in this chat)` : ""}. Specialised tools are faster and safer for each task.`,
    },
    {
      preset: "minimal",
      title: "Core tools only",
      text: `${setting.minimal.join(", ")}. Smaller requests, which suits small or local models.`,
    },
  ];
  return (
    <section className="tool-preset" aria-label="Tools for new chats">
      <div className="tool-preset-options" role="radiogroup">
        {options.map((option) => (
          <button
            type="button"
            role="radio"
            key={option.preset}
            aria-checked={setting.preset === option.preset}
            disabled={busy || locked || setting.env_override}
            onClick={() => void choose(option.preset)}
          >
            <b>{option.title}</b>
            <span>{option.text}</span>
          </button>
        ))}
      </div>
      <p className="quiet">
        {setting.env_override
          ? `Set by the ${setting.from}, so it can’t be changed here.`
          : `Applies to new chats in this project. Saved in ${setting.from === "default" ? "this project’s medha.lock" : setting.from}, which the terminal app uses too.`}
        {saved && " Saved."}
      </p>
      {error && <p className="surface-error" role="alert">{error}</p>}
    </section>
  );
}
