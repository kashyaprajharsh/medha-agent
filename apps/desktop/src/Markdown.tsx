import { memo, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { highlightCode } from "./highlight";
import { Icon } from "./Icon";
import { useWorkspace } from "./Workspace";
import { fileReference, splitOutputs } from "./outputBlocks";
import { Plate } from "./OutputPlate";
import { useOutputs } from "./outputContext";
import { highlightAsync } from "./syntax";

const WORKER_THRESHOLD = 8_000;

// Highlight only text from the escaped backend renderer. Never interpret raw
// model HTML or let an unknown language inject markup.
// Bound retained source + HTML, rather than keeping every code block forever.
const highlighted = new Map<string, string>();
let cacheSize = 0;
const CACHE_LIMIT = 1_000_000;
export function codeHtml(text: string, language?: string) {
  const key = `${language ?? ""}\0${text}`;
  const cached = highlighted.get(key);
  if (cached !== undefined) {
    highlighted.delete(key);
    highlighted.set(key, cached);
    return cached;
  }
  const html = highlightCode(text, language);
  const size = key.length + html.length;
  if (size <= CACHE_LIMIT) {
    while (cacheSize + size > CACHE_LIMIT) {
      const oldest = highlighted.keys().next().value!;
      cacheSize -= oldest.length + highlighted.get(oldest)!.length;
      highlighted.delete(oldest);
    }
    highlighted.set(key, html);
    cacheSize += size;
  }
  return html;
}
type MarkdownProps = {
  html: string;
  className?: string;
  onFile?: (path: string) => void;
  /** Names this reply's outputs, so an open one is found again as the reply grows. */
  scope?: string;
  /** When the reply was made, which orders its outputs among the chat's. */
  at?: number;
  streaming?: boolean;
  /** The reply was made in this sitting, so what it names may open by itself. */
  fresh?: boolean;
};

export const Markdown = memo(function Markdown({
  html,
  className = "",
  onFile,
  scope = "reply",
  at = 0,
  streaming = false,
  fresh = false,
}: MarkdownProps) {
  const segments = useMemo(() => splitOutputs(html), [html]);
  const prose = streaming ? `${className} streaming` : className;
  // Files the reply named are known once it is saved, never while it streams.
  const named = !streaming && !scope.startsWith("live-") && <Named scope={scope} at={at} fresh={fresh} />;
  if (segments.length === 1 && segments[0].type === "html")
    return <><Prose html={html} className={prose} onFile={onFile} />{named}</>;
  const last = segments.length - 1;
  const rows: ReactNode[] = [];
  for (let index = 0; index < segments.length; index++) {
    const segment = segments[index];
    if (segment.type === "html") {
      rows.push(
        <Prose
          key={index}
          html={segment.html}
          className={index === last ? prose : className}
          onFile={onFile}
        />,
      );
      continue;
    }
    // Outputs made together sit side by side; a lone one keeps the same parent as it gains a neighbour.
    const first = index;
    const plates: ReactNode[] = [];
    for (let next = segments[index]; next?.type === "output"; next = segments[++index])
      plates.push(
        <Plate
          key={index}
          anchor={`${scope}:${index}`}
          kind={next.kind}
          source={next.source}
          sourceEvent={scope.startsWith("live-") ? undefined : scope}
          sourceIndex={next.sourceIndex}
          fresh={fresh}
          writing={streaming && index === last}
          order={at * 1000 + index}
        />,
      );
    index--;
    rows.push(
      <div className="out-grid" key={first}>
        {plates}
      </div>,
    );
  }
  return <>{rows}{named}</>;
});

/** The files a saved reply named that its run changed, shown where they lie. */
function Named({ scope, at, fresh }: { scope: string; at: number; fresh: boolean }) {
  const { all } = useOutputs();
  const files = useMemo(() => all.filter((output) => output.sourceEvent === scope && output.path && !output.snapshot && !output.screen), [all, scope]);
  if (!files.length) return null;
  return (
    <div className="out-grid">
      {files.map((file) => (
        <Plate key={file.anchor} anchor={file.anchor} kind={file.kind} source="" path={file.path} name={file.name}
          sourceEvent={scope} sourceIndex={file.sourceIndex} fresh={fresh} writing={false} order={at * 1000 + (file.sourceIndex ?? 0)} />
      ))}
    </div>
  );
}

const Prose = memo(function Prose({
  html,
  className = "",
  onFile,
}: Pick<MarkdownProps, "html" | "className" | "onFile">) {
  const root = useRef<HTMLDivElement>(null);
  const workspace = useWorkspace();
  useEffect(() => {
    const controller = new AbortController();
    root.current?.querySelectorAll<HTMLElement>("code, .md-image").forEach((element) => {
      if (element.closest("pre, a")) return;
      const path = fileReference(element.dataset.src ?? element.textContent ?? "");
      if (!path) return;
      const link = document.createElement("a");
      link.href = encodeURI(path);
      link.title = "Preview referenced file";
      element.replaceWith(link);
      link.append(element);
    });
    root.current?.querySelectorAll<HTMLPreElement>("pre").forEach((pre) => {
      const code = pre.querySelector("code");
      if (!code) return;
      const text = code.textContent ?? "";
      if (text.length <= 100_000) {
        if (text.length > WORKER_THRESHOLD && typeof Worker !== "undefined") {
          void highlightAsync(text, pre.dataset.lang, controller.signal).then(
            (html) => {
              if (
                html !== undefined &&
                !controller.signal.aborted &&
                code.isConnected
              )
                code.innerHTML = html;
            },
          );
        } else code.innerHTML = codeHtml(text, pre.dataset.lang);
      }
      pre.querySelector(".code-copy")?.remove();
      const button = document.createElement("button");
      button.type = "button";
      button.className = "code-copy";
      button.textContent = "Copy";
      button.setAttribute("aria-label", "Copy code");
      button.onclick = () => {
        void navigator.clipboard
          .writeText(text)
          .then(() => {
            button.textContent = "Copied";
            setTimeout(() => {
              if (button.isConnected) button.textContent = "Copy";
            }, 1500);
          })
          .catch(() => {
            button.textContent = "Couldn’t copy";
          });
      };
      pre.append(button);
    });
    return () => controller.abort();
  }, [html]);
  return (
    <div
      ref={root}
      className={`prose md ${className}`}
      dangerouslySetInnerHTML={{ __html: html }}
      onClick={(event) => {
        const anchor = (event.target as Element).closest("a");
        if (!anchor) return;
        event.preventDefault();
        const path = anchor.getAttribute("href") || "";
        if (/^(https?:|mailto:)/i.test(path)) {
          void workspace.api.openLink(path).catch(() => {});
          return;
        }
        if (path.startsWith("#")) {
          root.current
            ?.querySelector(`#${CSS.escape(path.slice(1))}`)
            ?.scrollIntoView();
          return;
        }
        let decoded: string;
        try {
          decoded = decodeURIComponent(path);
        } catch {
          return;
        }
        if (onFile) onFile(decoded);
        else
          window.dispatchEvent(
            new CustomEvent("medha-file", {
              detail: { workspace: workspace.workspace.id, path: decoded },
            }),
          );
      }}
    />
  );
});
export const SourceCode = memo(function SourceCode({
  text,
  language,
}: {
  text: string;
  language?: string;
}) {
  const [copied, setCopied] = useState(false);
  const [highlighted, setHighlighted] = useState<{
    text: string;
    language?: string;
    html: string;
  }>();
  const asynchronous =
    text.length > WORKER_THRESHOLD && typeof Worker !== "undefined";
  useEffect(() => {
    if (!asynchronous || text.length > 100_000) return;
    const controller = new AbortController();
    void highlightAsync(text, language, controller.signal).then((html) => {
      if (html !== undefined && !controller.signal.aborted)
        setHighlighted({ text, language, html });
    });
    return () => controller.abort();
  }, [text, language, asynchronous]);
  const html = useMemo(
    () =>
      asynchronous
        ? highlighted?.text === text && highlighted.language === language
          ? highlighted.html
          : undefined
        : text.length <= 100_000
          ? codeHtml(text, language)
          : undefined,
    [text, language, asynchronous, highlighted],
  );
  return (
    <div className="source-code">
      <button
        className="code-copy"
        onClick={() =>
          void navigator.clipboard.writeText(text).then(() => setCopied(true))
        }
      >
        <Icon name="copy" />
        {copied ? "Copied" : "Copy"}
      </button>
      <pre>
        <code dangerouslySetInnerHTML={html ? { __html: html } : undefined}>
          {html ? undefined : text}
        </code>
      </pre>
    </div>
  );
});
