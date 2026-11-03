import { Select } from "./Select";
import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";
import type { Attachment, ImageAttachment } from "./api";
import { attachedFiles } from "./messageRequest";
export type RewindResult = {
  source: string;
  session: string;
  prefill: string;
  attachments: Attachment[];
  code_only: boolean;
};
// What keeps the dialog mounted: a running rewind, or the report of files it could not put back.
export type RewindHold = "busy" | "report" | null;
type Point = { id: string; text: string; files: number };
type Report = { restored: number; expired: string[]; codeOnly: boolean };
const files = (count: number) => `${count} ${count === 1 ? "file" : "files"}`;
export function Rewind({
  sessionKey,
  at,
  reveal = 0,
  ensureOpen,
  onHold = () => {},
  onClose,
  onComplete,
}: {
  sessionKey: string;
  at?: string;
  reveal?: number;
  ensureOpen: () => Promise<void>;
  onHold?: (hold: RewindHold) => void;
  onClose: () => void;
  onComplete: (result: RewindResult) => void;
}) {
  const api = useWorkspaceApi();
  const [points, setPoints] = useState<Point[]>([]);
  const [selected, setSelected] = useState(at || "");
  const [scope, setScope] = useState("conversation");
  const [busy, setBusy] = useState(false);
  const [hidden, setHidden] = useState(false);
  const [report, setReport] = useState<Report | null>(null);
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
      .catch((cause) => active && setError(message(cause)));
    return () => {
      active = false;
    };
  }, [sessionKey]);
  useEffect(() => setHidden(false), [reveal]);
  useEffect(() => {
    if (!busy && !report) return;
    const escape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (busy) setHidden(true);
      else onClose();
    };
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [busy, report, onClose]);
  const point = points.find((item) => item.id === selected);
  const count = point?.files || 0;
  async function rewind() {
    setBusy(true);
    setError("");
    onHold("busy");
    try {
      const result = await api.liveCall(
        sessionKey,
        "session.rewind",
        { at: selected, scope },
        null,
      );
      const rewound = result as Omit<RewindResult, "attachments"> & {
        images: Omit<ImageAttachment, "preview">[];
        restored?: number;
        expired?: string[];
      };
      const local = attachedFiles(rewound.prefill);
      onComplete({
        ...rewound,
        prefill: local.text,
        attachments: [...rewound.images.map((image) => ({
          ...image,
          preview: `data:${image.mime};base64,${image.data}`,
        })), ...local.files.map((file) => ({ ...file, id: crypto.randomUUID() }))],
      });
      const expired = rewound.expired ?? [];
      if (!expired.length) return onClose();
      setReport({ restored: rewound.restored ?? 0, expired, codeOnly: rewound.code_only });
      setHidden(false);
      onHold("report");
    } catch (cause) {
      setError(message(cause));
      setHidden(false);
      onHold(null);
    } finally {
      setBusy(false);
    }
  }
  if (hidden) return null;
  if (report) {
    const gone = report.expired.length;
    return (
      <div className="dialog-backdrop">
        <section
          className="confirm-dialog"
          role="dialog"
          aria-modal="true"
          aria-labelledby="rewind-title"
        >
          <h2 id="rewind-title">Rewound</h2>
          <p>
            {report.codeOnly
              ? `Put back ${files(report.restored)}.`
              : `Put back ${files(report.restored)} and started a new branch of the conversation from before that message.`}
          </p>
          <p>
            {gone === 1
              ? "1 file was left as it is, because its backup had expired:"
              : `${gone} files were left as they are, because their backups had expired:`}
          </p>
          <ul className="rewind-expired">
            {report.expired.map((path) => (
              <li key={path}>
                <code>{path}</code>
              </li>
            ))}
          </ul>
          <div className="row-actions">
            <button className="btn-gold" onClick={onClose}>
              Done
            </button>
          </div>
        </section>
      </div>
    );
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
            disabled={busy}
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
            { id: "both", label: `Conversation and ${files(count)}` },
            { id: "code", label: `Files only (${count})` },
          ].map((option) => (
            <label key={option.id}>
              <input
                type="radio"
                name="rewind-scope"
                value={option.id}
                checked={scope === option.id}
                disabled={busy || (option.id !== "conversation" && !count)}
                onChange={() => setScope(option.id)}
              />
              {option.label}
            </label>
          ))}
        </div>
        {busy ? (
          <p role="status">
            {scope === "conversation"
              ? "Starting a new branch from before that message."
              : `Putting back ${files(count)}.`}{" "}
            You can close this; the rewind carries on.
          </p>
        ) : (
          scope !== "conversation" &&
          !error && (
            <p>
              Puts back the {files(count)} Medha changed in this project. Git's
              changes and files outside the project stay as they are.
            </p>
          )
        )}
        {error && (
          <p className="surface-error" role="alert">
            {error}
          </p>
        )}
        <div className="row-actions">
          <button
            className="btn-line"
            onClick={busy ? () => setHidden(true) : onClose}
          >
            {busy ? "Close" : "Cancel"}
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
const message = (cause: unknown) =>
  cause instanceof Error ? cause.message : String(cause);
