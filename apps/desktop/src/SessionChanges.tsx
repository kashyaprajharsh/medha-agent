import { useEffect, useRef, useState } from "react";
import type { FileChange, FileEdit } from "./api";
import { Diff } from "./GitChanges";
import { Icon } from "./Icon";
import { clock } from "./timeline";
import { useWorkspaceApi } from "./Workspace";

type Props = {
  sessionId: string | null;
  version: string;
  focus?: { path: string; at: number };
  onFile: (path: string) => void;
  onCount?: (count: number) => void;
};

/** Every file this session wrote or edited, as one net diff per file. */
export function SessionChanges({ sessionId, version, focus, onFile, onCount }: Props) {
  const api = useWorkspaceApi();
  const [files, setFiles] = useState<FileChange[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    if (!sessionId) {
      setFiles([]);
      onCount?.(0);
      return;
    }
    let active = true;
    setLoading(true);
    api
      .changes(sessionId)
      .then((result) => {
        if (!active) return;
        setFiles(result.files);
        setError("");
        onCount?.(result.files.length);
      })
      .catch((cause) => active && setError(String(cause)))
      .finally(() => active && setLoading(false));
    return () => {
      active = false;
    };
  }, [sessionId, version, refresh]);

  const added = files.reduce((sum, file) => sum + file.added, 0);
  const removed = files.reduce((sum, file) => sum + file.removed, 0);
  return (
    <>
      <div className="browser-toolbar">
        <b>{files.length ? `${files.length} ${files.length === 1 ? "file" : "files"} changed` : "This session"}</b>
        {files.length > 0 && (
          <span className="change-count">
            <i>+{added}</i>
            <em>−{removed}</em>
          </span>
        )}
        <button className="icon-btn" aria-label="Refresh changes" onClick={() => setRefresh(refresh + 1)}>
          <Icon name="refresh" />
        </button>
      </div>
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      {!files.length && !error && (
        <p className="quiet">{loading ? "Reading this session’s changes…" : "Files Medha writes or edits in this session show up here."}</p>
      )}
      {files.map((file, index) => (
        <ChangedFile
          key={file.path}
          file={file}
          initiallyOpen={index === 0}
          focus={focus?.path === file.path ? focus.at : undefined}
          onFile={onFile}
        />
      ))}
    </>
  );
}

const EDIT_LABELS: Record<FileEdit["kind"], string> = {
  created: "Created",
  wrote: "Rewrote",
  edited: "Edited",
};

function EditRow({ edit, initiallyOpen }: { edit: FileEdit; initiallyOpen: boolean }) {
  const [open, setOpen] = useState(initiallyOpen);
  return (
    <li className="edit-row">
      <button className="edit-head" aria-expanded={open} onClick={() => setOpen(!open)}>
        <Icon name={open ? "down" : "chev"} />
        <b>{EDIT_LABELS[edit.kind]}</b>
        <span className="change-count">
          <i>+{edit.added}</i>
          <em>−{edit.removed}</em>
        </span>
        <time>{clock(edit.ts)}</time>
      </button>
      {open && <Diff text={edit.diff} />}
    </li>
  );
}

function shortPath(path: string) {
  const parts = path.split("/").filter(Boolean);
  return parts.length > 3 ? `…/${parts.slice(-3).join("/")}` : path;
}

function ChangedFile({
  file,
  initiallyOpen,
  focus,
  onFile,
}: {
  file: FileChange;
  initiallyOpen: boolean;
  focus?: number;
  onFile: (path: string) => void;
}) {
  const [open, setOpen] = useState(initiallyOpen);
  const [whole, setWhole] = useState(false);
  const row = useRef<HTMLElement>(null);
  useEffect(() => {
    if (focus === undefined) return;
    setOpen(true);
    row.current?.scrollIntoView({ block: "start", behavior: "smooth" });
  }, [focus]);
  const latest = file.history[0];
  return (
    <section className={`changed-file ${focus !== undefined ? "focused" : ""}`} ref={row}>
      <button className="changed-file-head" aria-expanded={open} onClick={() => setOpen(!open)}>
        <Icon name={open ? "down" : "chev"} />
        <code title={file.path}>{shortPath(file.path)}</code>
        {file.created && <small>New</small>}
        {file.edits > 1 && <small>{file.edits} edits</small>}
        <span className="change-count" title="The latest edit">
          <i>+{latest?.added ?? file.added}</i>
          <em>−{latest?.removed ?? file.removed}</em>
        </span>
      </button>
      {open && (
        <>
          {file.edits > 1 && (
            <div className="segmented small">
              <button aria-pressed={!whole} onClick={() => setWhole(false)}>
                Each edit
              </button>
              <button aria-pressed={whole} onClick={() => setWhole(true)}>
                Whole session +{file.added} −{file.removed}
              </button>
            </div>
          )}
          {whole || file.edits <= 1 ? (
            <Diff text={file.diff} />
          ) : (
            <ol className="edit-history">
              {file.history.map((edit, index) => (
                <EditRow key={`${edit.ts}-${index}`} edit={edit} initiallyOpen={index === 0} />
              ))}
              {file.edits > file.history.length && (
                <li className="quiet">{file.edits - file.history.length} earlier edits are in Whole session</li>
              )}
            </ol>
          )}
          <div className="change-file-actions">
            <button className="btn-line" onClick={() => onFile(file.path)}>
              <Icon name="file" />
              Preview file
            </button>
          </div>
        </>
      )}
    </section>
  );
}
