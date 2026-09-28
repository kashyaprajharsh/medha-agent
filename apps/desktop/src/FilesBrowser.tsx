import { useEffect, useState } from "react";
import type { FileEntry } from "./api";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
export function FilesBrowser({
  onFile,
  version,
}: {
  onFile: (path: string) => void;
  version: string;
}) {
  const api = useWorkspaceApi();
  const [directory, setDirectory] = useState("");
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [query, setQuery] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [refresh, setRefresh] = useState(0);
  useEffect(() => {
    setDirectory("");
    setQuery("");
  }, [version]);
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError("");
    void api
      .files(directory)
      .then((rows) => active && setEntries(rows))
      .catch((cause) => active && setError(String(cause)))
      .finally(() => active && setLoading(false));
    return () => {
      active = false;
    };
  }, [directory, refresh, version]);
  return (
    <>
      <div className="browser-toolbar">
        <button
          className="icon-btn"
          aria-label="Parent folder"
          disabled={!directory}
          onClick={() =>
            setDirectory(directory.split(/[\\/]/).slice(0, -1).join("/"))
          }
        >
          <Icon name="up" />
        </button>
        <span title={directory}>{directory || "Files"}</span>
        <button
          className="icon-btn"
          aria-label="Refresh files"
          onClick={() => setRefresh(refresh + 1)}
        >
          <Icon name="refresh" />
        </button>
      </div>
      <input
        className="filter-input"
        placeholder="Filter this folder"
        aria-label="Filter files"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
      />
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      {loading ? (
        <p className="quiet">Reading folder…</p>
      ) : (
        <div className="file-list">
          {entries
            .filter((entry) =>
              entry.name.toLowerCase().includes(query.toLowerCase()),
            )
            .map((entry) => (
              <button
                key={entry.path}
                className="file-row"
                onClick={() => {
                  if (entry.directory) {
                    setDirectory(entry.path);
                    setQuery("");
                  } else onFile(entry.path);
                }}
              >
                <Icon name={entry.directory ? "folder" : "file"} />
                <span>{entry.name}</span>
                {entry.directory && <Icon name="chev" />}
              </button>
            ))}
          {!entries.length && (
            <p className="quiet">No files in this folder yet.</p>
          )}
        </div>
      )}
    </>
  );
}
