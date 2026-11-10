import { useEffect, useState } from "react";
import { desktop } from "./api";
import { sizeText } from "./outputBlocks";
import { useWorkspace } from "./Workspace";

/** Asks before a project leaves Medha's list, and whether its chat history goes with it. */
export function RemoveWorkspace({
  working,
  onClose,
}: {
  working: boolean;
  onClose: () => void;
}) {
  const context = useWorkspace();
  const { id, name } = context.workspace;
  const [history, setHistory] = useState<{ chats: number; bytes: number }>();
  const [deleting, setDeleting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    void desktop
      .workspaceHistory(id)
      .then((found) => active && setHistory(found))
      .catch(() => {});
    return () => {
      active = false;
    };
  }, [id]);
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) onClose();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [busy, onClose]);
  const chats = history && (history.chats === 1 ? "1 chat" : `${history.chats} chats`);
  const keep = history?.chats
    ? `Keep its chat history. Adding the folder again brings its ${chats} back.`
    : "Keep its chat history.";
  const drop = history
    ? `Delete its chat history too: ${chats}, their backups and this folder's memory, ${sizeText(history.bytes)}.`
    : "Delete its chat history too, with its backups and this folder's memory.";
  async function remove() {
    setBusy(true);
    setError("");
    try {
      await context.remove(id, deleting);
    } catch (cause) {
      setError(String(cause));
      setBusy(false);
    }
  }
  return (
    <div className="dialog-backdrop">
      <section
        className="confirm-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="remove-title"
      >
        <h2 id="remove-title">Remove {name} from Medha?</h2>
        <p>
          It leaves Medha's list. The folder and its files stay on your
          computer, untouched.
        </p>
        <div className="rewind-options">
          <label>
            <input
              type="radio"
              name="remove-history"
              checked={!deleting}
              disabled={busy}
              onChange={() => setDeleting(false)}
            />
            {keep}
          </label>
          <label>
            <input
              type="radio"
              name="remove-history"
              checked={deleting}
              disabled={busy}
              onChange={() => setDeleting(true)}
            />
            {drop}
          </label>
        </div>
        {deleting && (
          <p className="dialog-warn">Deleted history can't be brought back.</p>
        )}
        {working && (
          <p className="dialog-warn" role="alert">
            A chat in this folder is still working. Stop it first, then remove
            the folder.
          </p>
        )}
        {error && (
          <p className="dialog-warn" role="alert">
            {error}
          </p>
        )}
        <div className="row-actions">
          <button className="btn-line" disabled={busy} onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn-gold"
            disabled={busy || working}
            onClick={() => void remove()}
          >
            {deleting ? "Remove and delete history" : "Remove"}
          </button>
        </div>
      </section>
    </div>
  );
}
