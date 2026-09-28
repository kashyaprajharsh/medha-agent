import { memo, useEffect, useMemo, useRef, useState } from "react";
import hljs from "highlight.js/lib/common";
import { Icon } from "./Icon";
import { useWorkspace } from "./Workspace";
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
  const html = highlight(text, language);
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
function highlight(text: string, language?: string) {
  if (language && hljs.getLanguage(language))
    return hljs.highlight(text, { language, ignoreIllegals: true }).value;
  return hljs.highlightAuto(text).value;
}
export const Markdown = memo(function Markdown({
  html,
  className = "",
  onFile,
}: {
  html: string;
  className?: string;
  onFile?: (path: string) => void;
}) {
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
