import { useCallback, useEffect, useMemo, useRef, useState, type RefObject } from "react";
import type { FilePreview } from "./api";
import { useWorkspaceApi } from "./Workspace";
import { extensionOf, type Output } from "./outputBlocks";

type Report = ((problem: string | undefined) => void) | undefined;

const reason = (cause: unknown) => String(cause).replace(/^Error:\s*/, "");

/** State that belongs to one version of an output and is gone the moment that changes. */
export function useKeyed<T>(key: string): [T | undefined, (value: T | undefined) => void] {
  const [held, setHeld] = useState<{ key: string; value: T | undefined }>();
  const set = useCallback((value: T | undefined) => setHeld({ key, value }), [key]);
  return [held?.key === key ? held.value : undefined, set];
}

/** A file Medha made, read from the chat's folder. */
export function useFile(request: string | Output | undefined, onProblem: Report): FilePreview | undefined {
  const api = useWorkspaceApi();
  const path = typeof request === "string" ? request : request?.path;
  const revision = typeof request === "object" ? request.revisionId : undefined;
  const session = typeof request === "object" ? request.sessionId : undefined;
  const stored = typeof request === "object" && !!(request.snapshot || request.storedSource);
  const [file, setFile] = useState<FilePreview>();
  useEffect(() => {
    if (!path && !stored) { setFile(undefined); return; }
    let current = true;
    let bundle: string | undefined;
    setFile(undefined);
    const read = stored ? (revision && session ? api.artifactPreview(session, revision) : Promise.reject(new Error("This file has no saved revision identity."))) : api.preview(path!);
    void read
      .then((read) => {
        if (!current) { if (read.bundle) void api.artifactRelease(read.bundle); return; }
        bundle = read.bundle;
        setFile(read);
        onProblem?.(undefined);
      })
      .catch((cause) => current && onProblem?.(reason(cause)));
    return () => {
      current = false;
      if (bundle) void api.artifactRelease(bundle);
    };
  }, [api, path, revision, session, stored, onProblem]);
  return file;
}

/** A link the window can play a video or a sound from, without reading it whole. */
export function useMedia(request: string | Output | undefined, onProblem: Report) {
  const api = useWorkspaceApi();
  const path = typeof request === "string" ? request : request?.path;
  const stored = typeof request === "object" && !!request.snapshot;
  const file = useFile(stored ? request : undefined, onProblem);
  const extension = extensionOf(path ?? "");
  const types: Record<string, string> = { mp4: "video/mp4", m4v: "video/mp4", mov: "video/quicktime", webm: "video/webm", mp3: "audio/mpeg", wav: "audio/wav", m4a: "audio/mp4", ogg: "audio/ogg", oga: "audio/ogg", flac: "audio/flac" };
  const url = useBlobUrl(file?.bytes, types[extension] ?? "application/octet-stream");
  const [media, setMedia] = useState<{ url: string; size: number }>();
  useEffect(() => {
    if (!path || stored) return;
    let current = true;
    void api
      .mediaLink(path)
      .then((link) => current && setMedia(link))
      .catch((cause) => current && onProblem?.(reason(cause)));
    return () => {
      current = false;
    };
  }, [api, path, stored, onProblem]);
  return useMemo(() => stored ? (url && file ? { url, size: file.size } : undefined) : media, [stored, url, file, media]);
}

/** Bytes shown through a URL that lives exactly as long as they are on screen. */
export function useBlobUrl(bytes: number[] | Uint8Array | undefined, type: string): string | undefined {
  const [url, setUrl] = useState<string>();
  useEffect(() => {
    if (!bytes) return;
    const made = URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type }));
    setUrl(made);
    return () => URL.revokeObjectURL(made);
  }, [bytes, type]);
  return url;
}

const FAR_FOR = 20_000;

/**
 * Whether the element is near the screen; heavy content waits for that. With
 * `stay` it remains true once reached; without, it follows the element out again.
 */
export function useNear(ref: RefObject<Element | null>, stay = true): boolean {
  const [near, setNear] = useState(typeof IntersectionObserver === "undefined");
  useEffect(() => {
    if ((near && stay) || !ref.current || typeof IntersectionObserver === "undefined") return;
    // Coming near is acted on at once. Going far is not, until it has lasted:
    // a layout shift or a scroll past would otherwise unload what is being used.
    let leaving: ReturnType<typeof setTimeout> | undefined;
    const observer = new IntersectionObserver(
      (entries) => {
        clearTimeout(leaving);
        if (entries.some((entry) => entry.isIntersecting)) setNear(true);
        else leaving = setTimeout(() => setNear(false), FAR_FOR);
      },
      { rootMargin: "400px" },
    );
    observer.observe(ref.current);
    return () => {
      clearTimeout(leaving);
      observer.disconnect();
    };
  }, [near, stay, ref]);
  return near;
}

/** The element's width, kept current, for content drawn at a fixed size and scaled to fit. */
export function useWidth(): [RefObject<HTMLDivElement | null>, number] {
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const box = ref.current;
    if (!box) return;
    setWidth(box.clientWidth);
    const observer = new ResizeObserver(() => setWidth(box.clientWidth));
    observer.observe(box);
    return () => observer.disconnect();
  }, []);
  return [ref, width];
}
