/** True on a Mac, where shortcuts use ⌘; everywhere else they use Ctrl. */
export const MAC = /Mac/.test(navigator.userAgent);

/** A shortcut as the host labels it: `⇧⌘L` on a Mac, `Ctrl+Shift+L` elsewhere. */
export function shortcut(key: string, shift = false): string {
  return MAC ? `${shift ? "⇧" : ""}⌘${key}` : `Ctrl+${shift ? "Shift+" : ""}${key}`;
}
