import { useEffect, useRef, useState } from "react";
import { useWorkspace, useWorkspaceApi } from "./Workspace";
import type { LocalAttachment, MessageImage } from "./api";
import { Icon } from "./Icon";
import { Sheet } from "./Sheet";

function imageUrl(mime: string, data: string) {
  if (!["image/png", "image/jpeg", "image/webp", "image/gif"].includes(mime))
    throw new Error("This image format cannot be displayed.");
  return `data:${mime};base64,${data}`;
}

function useImage(image: MessageImage, thumbnail: boolean, ready: boolean) {
  const api = useWorkspaceApi();
  const active = useWorkspace().active;
  const [url, setUrl] = useState(image.preview);
  const [error, setError] = useState("");
  useEffect(() => {
    let current = true;
    setUrl(image.preview);
    setError("");
    if (!ready || !active || image.preview || !image.hash) return;
    void api.historyImage(image.hash, thumbnail)
      .then((saved) => { if (current) setUrl(imageUrl(saved.mime, saved.data)); })
      .catch((cause) => { if (current) setError(String(cause)); });
    return () => { current = false; };
  }, [api, active, image.hash, image.preview, thumbnail, ready]);
  return { url, error };
}

function SentImage({ image }: { image: MessageImage }) {
  const frame = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(Boolean(image.preview));
  const [open, setOpen] = useState(false);
  const { url, error } = useImage(image, true, visible);
  useEffect(() => {
    if (visible || !frame.current) return;
    if (typeof IntersectionObserver === "undefined") { setVisible(true); return; }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        setVisible(true);
        observer.disconnect();
      }
    }, { rootMargin: "200px" });
    observer.observe(frame.current);
    return () => observer.disconnect();
  }, [visible]);
  return (
    <div ref={frame} className="sent-image">
      <button type="button" aria-label={`Open ${image.name}`} onClick={() => setOpen(true)} disabled={!url}>
        {url ? <img src={url} alt={image.name} loading="lazy" /> : <Icon name="file" />}
        <span>{image.name}</span>
      </button>
      {error && <span className="surface-error" role="status">Image unavailable: {error}</span>}
      {open && <ImageSheet image={image} onClose={() => setOpen(false)} />}
    </div>
  );
}

function ImageSheet({ image, onClose }: { image: MessageImage; onClose: () => void }) {
  const { url, error } = useImage(image, false, true);
  return <Sheet label={image.name} onClose={onClose}>
    <h2>{image.name}</h2>
    {url ? <img className="message-image-full" src={url} alt={image.name} />
      : <p role="status">{error || "Loading image…"}</p>}
  </Sheet>;
}

export function MessageAttachments({ images, files }: {
  images: MessageImage[];
  files: Omit<LocalAttachment, "id">[];
}) {
  if (!images.length && !files.length) return null;
  return <div className="message-attachments" aria-label="Message attachments">
    {images.map((image, index) => <SentImage key={`${image.hash ?? image.name}-${index}`} image={image} />)}
    {files.map((file, index) => <details className="sent-file" key={`${file.path}-${index}`}>
      <summary><Icon name={file.kind} /><span>{file.name}</span></summary>
      <code>{file.path}</code>
    </details>)}
  </div>;
}
