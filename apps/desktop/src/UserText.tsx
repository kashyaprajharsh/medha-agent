import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Icon } from "./Icon";

// Past this, a message reads as a paste: a card in the thread, the text on demand.
const PASTE_CHARS = 1500;
const PASTE_LINES = 25;

export function UserText({ text }: { text: string }) {
  const lines = text.split("\n").length;
  return text.length > PASTE_CHARS || lines > PASTE_LINES ? (
    <PastedText text={text} lines={lines} />
  ) : (
    <FoldedText text={text} />
  );
}

/** Folded to a few lines when long. Overflow is measured, not guessed, so the
 * toggle appears only when the window actually hides text. */
function FoldedText({ text }: { text: string }) {
  const body = useRef<HTMLParagraphElement>(null);
  const [open, setOpen] = useState(false);
  const [long, setLong] = useState(false);

  useLayoutEffect(() => {
    const node = body.current;
    if (!node || open) return;
    const measure = () => setLong(node.scrollHeight > node.clientHeight + 1);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, [text, open]);

  return (
    <div className="you-text">
      <p
        ref={body}
        className={open ? undefined : long ? "folded faded" : "folded"}
      >
        {text}
      </p>
      {(long || open) && (
        <button
          type="button"
          className="you-toggle"
          aria-expanded={open}
          onClick={() => setOpen((value) => !value)}
        >
          {open ? "Show less" : "Show more"}
        </button>
      )}
    </div>
  );
}

function size(text: string, lines: number) {
  const chars = `${text.length.toLocaleString()} characters`;
  return lines > 1 ? `${lines.toLocaleString()} lines · ${chars}` : chars;
}

function PastedText({ text, lines }: { text: string; lines: number }) {
  const [open, setOpen] = useState(false);
  const preview = text
    .split("\n")
    .filter((line) => line.trim())
    .slice(0, 2)
    .join("\n");
  return (
    <div className="you-text">
      <button
        type="button"
        className="pasted"
        aria-haspopup="dialog"
        onClick={() => setOpen(true)}
      >
        <span className="pasted-mark" aria-hidden="true">
          <Icon name="file" />
        </span>
        <span className="pasted-title">Pasted content</span>
        <span className="pasted-size">{size(text, lines)}</span>
        <span className="pasted-open" aria-hidden="true">
          <Icon name="maximize" />
        </span>
        <span className="pasted-preview">{preview}</span>
      </button>
      {open && (
        <PastedSheet
          text={text}
          lines={lines}
          onClose={() => setOpen(false)}
        />
      )}
    </div>
  );
}

function PastedSheet({
  text,
  lines,
  onClose,
}: {
  text: string;
  lines: number;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    const close = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      // Closing the sheet is the whole gesture; app-level Escape stays put.
      event.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", close, true);
    return () => window.removeEventListener("keydown", close, true);
  }, [onClose]);

  return createPortal(
    <div className="dialog-backdrop" onMouseDown={onClose}>
      <section
        className="pasted-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="pasted-sheet-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <header>
          <span className="pasted-mark" aria-hidden="true">
            <Icon name="file" />
          </span>
          <div>
            <h2 id="pasted-sheet-title">Pasted content</h2>
            <span className="pasted-size">{size(text, lines)}</span>
          </div>
          <button
            type="button"
            className="btn-line"
            onClick={() =>
              void navigator.clipboard
                .writeText(text)
                .then(() => setCopied(true))
                .catch(() => {})
            }
          >
            <Icon name={copied ? "check" : "copy"} />
            {copied ? "Copied" : "Copy"}
          </button>
          <button
            type="button"
            className="icon-btn"
            aria-label="Close"
            autoFocus
            onClick={onClose}
          >
            <Icon name="x" />
          </button>
        </header>
        <div className="pasted-body">{text}</div>
      </section>
    </div>,
    document.body,
  );
}
