import { Select } from "./Select";
import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";
import type { Attachment } from "./api";
export type RewindResult = {
  source: string;
  session: string;
  prefill: string;
  images: Attachment[];
  code_only: boolean;
};
type Point = { id: string; text: string; files: number };
export function Rewind({
  sessionKey,
  at,
  ensureOpen,
  onClose,
  onComplete,
}: {
  sessionKey: string;
  at?: string;
  ensureOpen: () => Promise<void>;
  onClose: () => void;
  onComplete: (result: RewindResult) => void;
}) {
  const api = useWorkspaceApi();
  const [points, setPoints] = useState<Point[]>([]);
  const [selected, setSelected] = useState(at || "");
  const [scope, setScope] = useState("conversation");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    void ensureOpen()
      .then(() => api.liveCall(sessionKey, "session.rewind.points"))
      .then((result) => {
        if (!active) return;
        const rows = result.points as Point[];
        setPoints(rows);
        setSelected((value) =>
          rows.some((point) => point.id === value)
            ? value
            : rows.at(-1)?.id || "",
        );
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [sessionKey]);
  const point = points.find((item) => item.id === selected);
  async function rewind() {
    setBusy(true);
    setError("");
    try {
      const result = await api.liveCall(sessionKey, "session.rewind", {
        at: selected,
        scope,
      });
      const rewound = result as RewindResult;
      onComplete({
        ...rewound,
        images: rewound.images.map((image) => ({
          ...image,
          preview: `data:${image.mime};base64,${image.data}`,
        })),
      });
      onClose();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="dialog-backdrop">
      <section
        className="confirm-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="rewind-title"
      >
        <h2 id="rewind-title">Rewind</h2>
        <p>
          Return to before a message. The original conversation stays in
          history.
        </p>
        <label className="rewind-label">
          Message
          <Select
            value={selected}
            onChange={(event) => {
              setSelected(event.target.value);
              setScope("conversation");
            }}
          >
            {points.map((item) => (
              <option key={item.id} value={item.id}>
                {item.text.slice(0, 100) || "Image message"}
              </option>
            ))}
          </Select>
        </label>
        <div className="rewind-options">
          {[
            { id: "conversation", label: "Conversation" },
            {
              id: "both",
              label: `Conversation and ${point?.files || 0} files`,
            },
            { id: "code", label: `Files only (${point?.files || 0})` },
          ].map((option) => (
            <label key={option.id}>
              <input
                type="radio"
                name="rewind-scope"
                value={option.id}
                checked={scope === option.id}
                disabled={option.id !== "conversation" && !point?.files}
                onChange={() => setScope(option.id)}
              />
              {option.label}
            </label>
          ))}
        </div>
        {scope !== "conversation" && (
          <p>
            Only recorded file edits can be restored. Shell commands and
            external changes aren’t undone.
          </p>
        )}
        {error && (
          <p className="surface-error" role="alert">
            {error}
          </p>
        )}
        <div className="row-actions">
          <button className="btn-line" disabled={busy} onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn-gold"
            disabled={busy || !point}
            onClick={() => void rewind()}
          >
            {busy ? "Rewinding…" : "Rewind"}
          </button>
        </div>
      </section>
    </div>
  );
}
