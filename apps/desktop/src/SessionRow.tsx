import { useEffect, useState, type ReactNode } from "react";
import type { Session } from "./api";
import { Icon } from "./Icon";

/** Each letter of a deleted chat's name lifts this long after the one before it, up to a cap. */
const LETTER_MS = 9;
const LETTERS_MS = 160;
const LIFT_MS = 210;

/** One chat in the sidebar. A deleted one lifts away letter by letter and leaves a way back in its place. */
export function SessionRow({
  session,
  active,
  pinned,
  deleting,
  when,
  meta,
  onSelect,
  onTogglePin,
  onDelete,
  onUndo,
}: {
  session: Session;
  active: boolean;
  pinned: boolean;
  deleting: boolean;
  when: ReactNode;
  meta: ReactNode;
  onSelect: () => void;
  onTogglePin: () => void;
  /** Absent while the chat works or waits on the person: it can't be deleted then. */
  onDelete?: () => void;
  onUndo: () => void;
}) {
  const title = session.title || "Untitled session";
  const [lifted, setLifted] = useState(false);
  useEffect(() => {
    if (!deleting) {
      setLifted(false);
      return;
    }
    const timer = setTimeout(
      () => setLifted(true),
      Math.min(title.length * LETTER_MS, LETTERS_MS) + LIFT_MS,
    );
    return () => clearTimeout(timer);
  }, [deleting, title]);
  return (
    <div className={`sess-row ${deleting ? "leaving" : ""}`} data-id={session.id}>
      <button
        type="button"
        className={`sess ${active ? "active" : ""}`}
        onClick={onSelect}
        aria-current={active ? "page" : undefined}
        disabled={deleting}
        style={lifted ? { visibility: "hidden" } : undefined}
      >
        <span className="sess-title">
          {deleting
            ? [...title].map((letter, index) => (
                <span
                  key={index}
                  className="ink-letter"
                  style={{ animationDelay: `${Math.min(index * LETTER_MS, LETTERS_MS)}ms` }}
                >
                  {letter}
                </span>
              ))
            : title}
        </span>
        <span className="sess-when">{when}</span>
        {meta}
      </button>
      {!deleting && (
        <>
          {onDelete && (
            <button
              type="button"
              className="del"
              aria-label={`Delete ${title}`}
              title="Delete"
              onClick={onDelete}
            >
              <Icon name="x" />
            </button>
          )}
          <button
            type="button"
            className="pin"
            aria-pressed={pinned}
            aria-label={`${pinned ? "Unpin" : "Pin"} ${title}`}
            title={pinned ? "Unpin" : "Pin"}
            onClick={onTogglePin}
          >
            <Icon name="pin" />
          </button>
        </>
      )}
      {lifted && (
        <div className="ghost-line" role="status">
          <span className="ghost-text">
            <b>"{title}"</b>
            <span>deleted</span>
          </span>
          <button type="button" onClick={onUndo}>
            Undo
          </button>
        </div>
      )}
    </div>
  );
}
