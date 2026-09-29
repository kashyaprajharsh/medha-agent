import hljs from "highlight.js/lib/common";

// Guesses below this score are wrong often enough to hurt: prose comes back as
// CSS, file trees as Python. Plain text reads better than confident-looking noise.
const CONFIDENT_GUESS = 7;

const ESCAPES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

/** Escaped HTML for a code block. A named language is trusted; an unnamed block
 * is coloured only on a confident guess; an unknown name stays plain. */
export function highlightCode(text: string, language?: string) {
  if (language && hljs.getLanguage(language))
    return hljs.highlight(text, { language, ignoreIllegals: true }).value;
  if (!language) {
    const guess = hljs.highlightAuto(text);
    if (guess.relevance >= CONFIDENT_GUESS) return guess.value;
  }
  return text.replace(/[&<>"']/g, (c) => ESCAPES[c]);
}
