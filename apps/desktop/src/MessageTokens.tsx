import { useLayoutEffect, useRef } from "react";
import { Icon } from "./Icon";

export type MessageToken = {
  id: string;
  kind: "skill" | "image";
  label: string;
  preview?: string;
  title?: string;
};

/** Where the typed text starts once the tokens are placed. */
export type TokenLayout = { indent: number; lift: number };

// Past this share of the line, tokens take a line of their own so the text
// is not squeezed into a sliver beside them.
const INLINE_SHARE = 0.6;
const GAP = 6;

/** Context that belongs to this one message, set in the message before the text. */
export function MessageTokens({
  tokens,
  armed,
  leaving,
  onRemove,
  onLayout,
}: {
  tokens: MessageToken[];
  armed: string | null;
  leaving: string | null;
  onRemove: (id: string) => void;
  onLayout: (layout: TokenLayout) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  const reported = useRef("");

  useLayoutEffect(() => {
    const node = box.current;
    // With the last token gone there is nothing to measure, and the text goes back to the margin.
    if (!node) {
      if (reported.current !== "0:0") {
        reported.current = "0:0";
        onLayout({ indent: 0, lift: 0 });
      }
      return;
    }
    const measure = () => {
      const items = [...node.children] as HTMLElement[];
      const last = items[items.length - 1];
      const oneLine = items.every((item) => item.offsetTop === items[0]?.offsetTop);
      const end = last ? last.offsetLeft + last.offsetWidth : 0;
      const layout =
        !last ? { indent: 0, lift: 0 }
        : oneLine && end <= node.clientWidth * INLINE_SHARE ? { indent: end + GAP, lift: 0 }
        : { indent: 0, lift: node.offsetHeight + GAP };
      const key = `${layout.indent}:${layout.lift}`;
      if (key !== reported.current) {
        reported.current = key;
        onLayout(layout);
      }
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, [tokens]);

  if (!tokens.length) return null;
  return (
    <div className="msg-tokens" ref={box}>
      {tokens.map((token) => (
        <span
          key={token.id}
          className={`msg-token token-${token.kind}${armed === token.id ? " armed" : ""}${leaving === token.id ? " leaving" : ""}`}
          title={token.title ?? token.label}
        >
          {token.kind === "image" && token.preview && <img src={token.preview} alt="" />}
          <span className="msg-token-label">{token.label}</span>
          <button type="button" tabIndex={-1} aria-label={`Remove ${token.label}`} onMouseDown={(event) => event.preventDefault()} onClick={() => onRemove(token.id)}>
            <Icon name="x" />
          </button>
        </span>
      ))}
    </div>
  );
}

/** The same token, fixed, at the start of a sent message. */
export function SentToken({ kind, label }: { kind: MessageToken["kind"]; label: string }) {
  return (
    <span className={`msg-token token-${kind} sent`} title={kind === "skill" ? `${label} guided this message` : label}>
      <span className="msg-token-label">{label}</span>
    </span>
  );
}
