import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import type { FilePreview } from "./api";
import { useWorkspaceApi } from "./Workspace";

type Report = ((problem: string | undefined) => void) | undefined;

const reason = (cause: unknown) => String(cause).replace(/^Error:\s*/, "");

/** State that belongs to one version of an output and is gone the moment that changes. */
export function useKeyed<T>(key: string): [T | undefined, (value: T | undefined) => void] {
  const [held, setHeld] = useState<{ key: string; value: T | undefined }>();
  const set = useCallback((value: T | undefined) => setHeld({ key, value }), [key]);
  return [held?.key === key ? held.value : undefined, set];
}

/** A file Medha made, read from the chat's folder. */
export function useFile(path: string | undefined, onProblem: Report): FilePreview | undefined {
  const api = useWorkspaceApi();
  const [file, setFile] = useState<FilePreview>();
  useEffect(() => {
    if (!path) return;
    let current = true;
    setFile(undefined);
    void api
      .preview(path)
      .then((read) => {
        if (!current) return;
        setFile(read);
        onProblem?.(undefined);
      })
      .catch((cause) => current && onProblem?.(reason(cause)));
    return () => {
      current = false;
    };
  }, [api, path, onProblem]);
  return file;
}

/** A link the window can play a video or a sound from, without reading it whole. */
export function useMedia(path: string | undefined, onProblem: Report) {
  const api = useWorkspaceApi();
  const [media, setMedia] = useState<{ url: string; size: number }>();
  useEffect(() => {
    if (!path) return;
    let current = true;
    void api
      .mediaLink(path)
      .then((link) => current && setMedia(link))
      .catch((cause) => current && onProblem?.(reason(cause)));
    return () => {
      current = false;
    };
  }, [api, path, onProblem]);
  return media;
}

/** Bytes shown through a URL that lives exactly as long as they are on screen. */
export function useBlobUrl(bytes: number[] | undefined, type: string): string | undefined {
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
