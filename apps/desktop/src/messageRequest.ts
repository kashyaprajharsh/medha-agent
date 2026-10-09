import type { Attachment, LocalAttachment } from "./api";

// The transport measures UTF-8 JSON, including base64 image data. Leave room
// for native routing fields; the native adapter enforces the exact size too.
const FRAME_BYTES = 16 * 1024 * 1024;
const ROUTING_BYTES = 128;
const LOCAL_FILES = "\n\n[Attached local files and folders]\n";
const FILES_END = "\n[/Attached local files and folders]\nRead these local paths as needed with your tools. Ask for access when required.";

/** Path references stay in the durable prompt, so every viewer and resume sees
 * the same files. They grant no permanent access and never embed file contents. */
export function attachedFiles(text: string): { text: string; files: Omit<LocalAttachment, "id">[] } {
  const start = text.lastIndexOf(LOCAL_FILES);
  if (start < 0 || !text.endsWith(FILES_END)) return { text, files: [] };
  try {
    const files: unknown = JSON.parse(text.slice(start + LOCAL_FILES.length, -FILES_END.length));
    if (!Array.isArray(files) || files.length > 16 || !files.every((file) =>
      file && ["file", "folder"].includes(file.kind) && typeof file.name === "string"
      && typeof file.path === "string" && file.path.length > 0)) return { text, files: [] };
    return { text: text.slice(0, start), files };
  } catch {
    return { text, files: [] };
  }
}

export function messageRequest(content: string, attachments: Attachment[]) {
  const images = attachments.filter((attachment) => "data" in attachment);
  const files = attachments.filter((attachment): attachment is LocalAttachment => "path" in attachment);
  if (images.length > 4 || attachments.length > 16) {
    throw new Error("Attach up to four images and sixteen total attachments per message.");
  }
  const params = {
    content: files.length
      ? content + LOCAL_FILES + JSON.stringify(files.map(({ kind, name, path }) => ({ kind, name, path }))) + FILES_END
      : content,
    images: images.map(({ id, name, mime, data }) => ({ id, name, mime, data })),
  };
  const frame = { id: 0, method: "message.send", params };
  if (new TextEncoder().encode(JSON.stringify(frame)).byteLength + ROUTING_BYTES > FRAME_BYTES) {
    throw new Error("The message and processed images together exceed the 16 MiB send limit. Send fewer or smaller images, or split the message.");
  }
  return params;
}
