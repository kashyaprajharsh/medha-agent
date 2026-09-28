import hljs from "highlight.js/lib/common";

export function highlightCode(text: string, language?: string) {
  return language && hljs.getLanguage(language)
    ? hljs.highlight(text, { language, ignoreIllegals: true }).value
    : hljs.highlightAuto(text).value;
}

self.onmessage = (
  event: MessageEvent<{ id: number; text: string; language?: string }>,
) => {
  const { id, text, language } = event.data;
  try {
    self.postMessage({ id, html: highlightCode(text, language) });
  } catch {
    self.postMessage({ id });
  }
};
