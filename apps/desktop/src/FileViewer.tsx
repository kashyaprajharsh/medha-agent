import { lazy, Suspense, useEffect, useMemo, useState } from "react";
import type { FilePreview } from "./api";
import { Icon } from "./Icon";
import { Markdown, SourceCode } from "./Markdown";
import { useWorkspaceApi } from "./Workspace";
import { OutputsProvider, SHOWN_ONLY } from "./outputContext";
// Outputs inside a previewed file are shown in place: opening one would close the file.

const PdfPreview = lazy(() => import("./PdfPreview"));
const DocumentPreview = lazy(() => import("./DocumentPreview"));
const TablePreview = lazy(() => import("./TablePreview"));
const LANGUAGES: Record<string, string> = {
  py: "python",
  rs: "rust",
  js: "javascript",
  ts: "typescript",
  jsx: "javascript",
  tsx: "typescript",
  sh: "bash",
  zsh: "bash",
  yml: "yaml",
  md: "markdown",
  h: "cpp",
  cs: "csharp",
  rb: "ruby",
  kt: "kotlin",
};
export function FileViewer({
  path,
  onClose,
}: {
  path: string;
  onClose: () => void;
}) {
  const api = useWorkspaceApi();
  const [file, setFile] = useState<FilePreview>();
  const [error, setError] = useState("");
  const [source, setSource] = useState(false);
  const [expanded, setExpanded] = useState(false);
  useEffect(() => {
    let active = true;
    setFile(undefined);
    setError("");
    setSource(false);
    void api
      .preview(path)
      .then((next) => active && setFile(next))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [path]);
  return (
    <aside
      className={`panel file-viewer ${expanded ? "preview-expanded" : ""}`}
      aria-label={`Preview ${path}`}
    >
      <header className="surface-head">
        <Icon name="file" />
        <b title={path}>{path.split(/[\\/]/).at(-1)}</b>
        <button
          className="icon-btn"
          aria-label={expanded ? "Reduce preview" : "Expand preview"}
          onClick={() => setExpanded(!expanded)}
        >
          <Icon name="maximize" />
        </button>
        <button
          className="icon-btn"
          aria-label="Close preview"
          onClick={onClose}
        >
          <Icon name="x" />
        </button>
      </header>
      <div className="preview-toolbar">
        <span title={path}>{path}</span>
        {file?.kind === "markdown" && (
          <button
            className="btn-line"
            aria-pressed={source}
            onClick={() => setSource(!source)}
          >
            {source ? "Preview" : "Source"}
          </button>
        )}
        <button
          className="btn-line"
          onClick={() =>
            void api.openFile(path).catch((cause) => setError(String(cause)))
          }
        >
          Open in app
        </button>
      </div>
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      <div className="preview-body">
        {!file && !error && <p className="quiet">Opening file…</p>}
        {file && (
          <Suspense fallback={<p className="quiet">Preparing preview…</p>}>
            {file.kind === "markdown" && !source ? (
              <OutputsProvider value={SHOWN_ONLY}>
                <Markdown html={file.html || ""} />
              </OutputsProvider>
            ) : file.kind === "pdf" ? (
              <PdfPreview bytes={file.bytes!} />
            ) : file.kind === "document" ? (
              <DocumentPreview bytes={file.bytes!} />
            ) : file.kind === "spreadsheet" || file.kind === "table" ? (
              <TablePreview file={file} />
            ) : file.kind === "image" ? (
              <ImagePreview file={file} path={path} />
            ) : (
              <SourceCode
                text={file.text || ""}
                language={LANGUAGES[file.extension] || file.extension}
              />
            )}
          </Suspense>
        )}
      </div>
    </aside>
  );
}
function ImagePreview({ file, path }: { file: FilePreview; path: string }) {
  const [scale, setScale] = useState(1);
  const blob = useMemo(
    () =>
      new Blob([new Uint8Array(file.bytes!)], {
        type: `image/${file.extension === "jpg" ? "jpeg" : file.extension}`,
      }),
    [file],
  );
  const [url, setUrl] = useState("");
  useEffect(() => {
    const value = URL.createObjectURL(blob);
    setUrl(value);
    return () => URL.revokeObjectURL(value);
  }, [blob]);
  return (
    <>
      <div className="preview-controls">
        <button
          className="btn-line"
          disabled={scale <= 0.5}
          onClick={() => setScale(scale - 0.25)}
        >
          −
        </button>
        <span>{Math.round(scale * 100)}%</span>
        <button
          className="btn-line"
          disabled={scale >= 3}
          onClick={() => setScale(scale + 0.25)}
        >
          +
        </button>
      </div>
      <div className="image-preview">
        <img
          src={url}
          alt={path}
          style={{ width: `${scale * 100}%`, maxWidth: "none" }}
        />
      </div>
    </>
  );
}
