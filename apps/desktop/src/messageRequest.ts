import type { Attachment } from "./api";

// The transport measures UTF-8 JSON, including base64 image data. Leave room
// for native routing fields; the native adapter enforces the exact size too.
const FRAME_BYTES = 16 * 1024 * 1024;
const ROUTING_BYTES = 128;

export function messageRequest(content: string, attachments: Attachment[]) {
  const params = {
    content,
    images: attachments.map(({ mime, data }) => ({ mime, data })),
  };
  const frame = { id: 0, method: "message.send", params };
  if (new TextEncoder().encode(JSON.stringify(frame)).byteLength + ROUTING_BYTES > FRAME_BYTES) {
    throw new Error("The message and processed images together exceed the 16 MiB send limit. Send fewer or smaller images, or split the message.");
  }
  return params;
}
