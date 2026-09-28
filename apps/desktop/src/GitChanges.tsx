import { useEffect, useState } from "react";
import type { GitFile } from "./api";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
export function GitChanges({
  onFile,
  version,
}: {
  onFile: (path: string) => void;
  version: string;
}) {
  const api = useWorkspaceApi();
  const [files, setFiles] = useState<GitFile[]>([]),
    [repository, setRepository] = useState(true),
    [refresh, setRefresh] = useState(0),
    [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    setError("");
    void api
      .gitStatus()
      .then((result) => {
        if (active) {
          setRepository(result.repository);
          setFiles(result.files);
        }
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [refresh, version]);
  const groups = [
    {
      name: "Changes",
      staged: false,
      files: files.filter((file) => file.untracked || file.working !== " "),
    },
    {
      name: "Staged",
      staged: true,
      files: files.filter((file) => !file.untracked && file.index !== " "),
    },
  ];
  return (
    <>
      <div className="browser-toolbar">
        <b>Working tree</b>
        <button
          className="icon-btn"
          aria-label="Refresh changes"
          onClick={() => setRefresh(refresh + 1)}
        >
          <Icon name="refresh" />
        </button>
      </div>
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      {!repository ? (
        <p className="quiet">
          This folder isn’t a Git repository. Created documents are in Files.
        </p>
      ) : !files.length ? (
        <p className="quiet">No uncommitted changes.</p>
      ) : (
        groups.map(
          (group) =>
            group.files.length > 0 && (
              <section key={group.name} className="change-group">
                <h3>
                  {group.name}
                  <span>{group.files.length}</span>
                </h3>
                {group.files.map((file, index) => (
                  <ChangedFile
                    key={`${version}:${group.staged}:${file.path}`}
                    file={file}
                    staged={group.staged}
                    refresh={refresh}
                    initiallyOpen={index === 0}
                    onFile={onFile}
                  />
                ))}
              </section>
            ),
        )
      )}
    </>
  );
}
function ChangedFile({
  file,
  staged,
  refresh,
  initiallyOpen,
  onFile,
}: {
  file: GitFile;
  staged: boolean;
  refresh: number;
  initiallyOpen: boolean;
  onFile: (path: string) => void;
}) {
  const api = useWorkspaceApi();
  const [open, setOpen] = useState(initiallyOpen),
    [diff, setDiff] = useState<string>(),
    [error, setError] = useState("");
  useEffect(() => {
    if (!open) return;
    let active = true;
    setDiff(undefined);
    setError("");
    void api
      .gitDiff(file.path, staged)
      .then((result) => active && setDiff(result.text))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [open, refresh, file.path, staged]);
  const lines = diff?.split("\n") || [];
  const added = lines.filter(
      (line) => line.startsWith("+") && !line.startsWith("+++"),
    ).length,
    removed = lines.filter(
      (line) => line.startsWith("-") && !line.startsWith("---"),
    ).length;
  return (
    <section className="changed-file">
      <button
        className="changed-file-head"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <Icon name={open ? "down" : "chev"} />
        <code title={file.path}>{file.path}</code>
        {diff !== undefined ? (
          <span className="change-count">
            <i>+{added}</i>
            <em>−{removed}</em>
          </span>
        ) : (
          <small>
            {file.untracked ? "New" : staged ? file.index : file.working}
          </small>
        )}
      </button>
      {open && (
        <>
          {error ? (
            <p className="surface-error" role="alert">
              {error}
            </p>
          ) : diff === undefined ? (
            <p className="quiet">Reading diff…</p>
          ) : (
            <Diff text={diff} />
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
export function Diff({ text }: { text: string }) {
  let oldLine = 1,
    newLine = 1;
  const rows = text.split("\n").map((line) => {
    const hunk = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
      return { text: line, kind: "hunk", old: "", next: "", sign: "" };
    }
    if (
      /^(diff |index |--- |\+\+\+ |new file|deleted file|similarity |rename |Binary |\\)/.test(
        line,
      )
    )
      return { text: line, kind: "header", old: "", next: "", sign: "" };
    if (line.startsWith("+"))
      return {
        text: line.slice(1),
        kind: "add",
        old: "",
        next: String(newLine++),
        sign: "+",
      };
    if (line.startsWith("-"))
      return {
        text: line.slice(1),
        kind: "delete",
        old: String(oldLine++),
        next: "",
        sign: "−",
      };
    if (line.startsWith(" "))
      return {
        text: line.slice(1),
        kind: "context",
        old: String(oldLine++),
        next: String(newLine++),
        sign: "",
      };
    return { text: line, kind: "header", old: "", next: "", sign: "" };
  });
  return (
    <div className="git-diff" aria-label="Diff">
      {text ? (
        rows.map((row, index) =>
          row.kind === "header" || row.kind === "hunk" ? (
            <div key={index} className={`diff-${row.kind}`}>
              {row.text || "\u00a0"}
            </div>
          ) : (
            <div key={index} className={`diff-row diff-${row.kind}`}>
              <span className="diff-number">{row.old}</span>
              <span className="diff-number">{row.next}</span>
              <span className="diff-sign">{row.sign}</span>
              <code>{row.text || "\u00a0"}</code>
            </div>
          ),
        )
      ) : (
        <p className="quiet">No diff to show.</p>
      )}
    </div>
  );
}
