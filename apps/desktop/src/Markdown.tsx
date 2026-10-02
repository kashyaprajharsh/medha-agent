import { memo, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { highlightCode } from "./highlight";
import { Icon } from "./Icon";
import { useWorkspace } from "./Workspace";
import { mentionsOf, splitOutputs } from "./outputBlocks";
import { Mentioned, Plate } from "./OutputPlate";
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
  // Files the reply only names are looked for once it is whole, not on every word.
  const named = useMemo(() => {
    if (streaming) return [];
    const shown = new Set(segments.map((segment) => (segment.type === "output" ? segment.path : undefined)));
    return mentionsOf(html).filter((mention) => !shown.has(mention.path));
  }, [html, segments, streaming]);
  const prose = streaming ? `${className} streaming` : className;
  if (segments.length === 1 && segments[0].type === "html" && !named.length)
    return <Prose html={html} className={prose} onFile={onFile} />;
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
          path={next.path}
          name={next.name}
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
  if (named.length)
    rows.push(
      <div className="out-grid" key="named">
        {named.map((mention, index) => (
          <Mentioned
            key={mention.path}
            anchor={`${scope}:named:${mention.path}`}
            kind={mention.kind}
            source=""
            path={mention.path}
            name={mention.name}
            writing={false}
            order={at * 1000 + 900 + index}
            fresh={fresh}
          />
        ))}
      </div>,
    );
  return <>{rows}</>;
});

const Prose = memo(function Prose({
  html,
  className = "",
  onFile,
}: Pick<MarkdownProps, "html" | "className" | "onFile">) {
  const root = useRef<HTMLDivElement>(null);
  const workspace = useWorkspace();
  useEffect(() => {
    const controller = new AbortController();
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
