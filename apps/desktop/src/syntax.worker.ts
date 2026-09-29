import { highlightCode } from "./highlight";

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
