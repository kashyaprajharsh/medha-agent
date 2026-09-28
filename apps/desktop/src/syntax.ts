// One worker and one in-flight job. Obsolete streaming requests are removed
// before dispatch, rather than piling up inside the worker's message queue.
type Consumer = { finish: (html?: string) => void };
type Job = {
  id: number;
  key: string;
  text: string;
  language?: string;
  consumers: Set<Consumer>;
};
const jobs = new Map<string, Job>();
const cache = new Map<string, string>();
const CACHE_LIMIT = 1_000_000;
let cacheSize = 0;
let serial = 0;
let worker: Worker | undefined;
let active: Job | undefined;
let failed = false;

function remember(key: string, html: string) {
  const size = key.length + html.length;
  if (size > CACHE_LIMIT) return;
  while (cacheSize + size > CACHE_LIMIT) {
    const oldest = cache.keys().next().value!;
    cacheSize -= oldest.length + cache.get(oldest)!.length;
    cache.delete(oldest);
  }
  cache.set(key, html);
  cacheSize += size;
}

function fail() {
  failed = true;
  worker?.terminate();
  worker = undefined;
  active = undefined;
  for (const job of jobs.values())
    for (const consumer of job.consumers) consumer.finish();
  jobs.clear();
}

function pump() {
  if (active || failed) return;
  const next = jobs.values().next().value;
  if (!next) return;
  try {
    if (!worker) {
      worker = new Worker(new URL("./syntax.worker.ts", import.meta.url), {
        type: "module",
      });
      worker.addEventListener("error", fail);
      worker.addEventListener("messageerror", fail);
      worker.addEventListener(
        "message",
        (event: MessageEvent<{ id: number; html?: string }>) => {
          if (!active || event.data.id !== active.id) return;
          const done = active;
          active = undefined;
          jobs.delete(done.key);
          if (typeof event.data.html === "string")
            remember(done.key, event.data.html);
          for (const consumer of done.consumers)
            consumer.finish(event.data.html);
          pump();
        },
      );
    }
    active = next;
    worker.postMessage({
      id: next.id,
      text: next.text,
      language: next.language,
    });
  } catch {
    fail();
  }
}

export function highlightAsync(
  text: string,
  language: string | undefined,
  signal: AbortSignal,
): Promise<string | undefined> {
  if (signal.aborted || failed) return Promise.resolve(undefined);
  const key = `${language ?? ""}\0${text}`;
  const cached = cache.get(key);
  if (cached !== undefined) {
    cache.delete(key);
    cache.set(key, cached);
    return Promise.resolve(cached);
  }
  let job = jobs.get(key);
  if (!job) {
    job = { id: ++serial, key, text, language, consumers: new Set() };
    jobs.set(key, job);
  }
  const target = job;
  const result = new Promise<string | undefined>((resolve) => {
    const consumer: Consumer = {
      finish(html) {
        signal.removeEventListener("abort", abort);
        resolve(html);
      },
    };
    const abort = () => {
      target.consumers.delete(consumer);
      consumer.finish();
      if (target !== active && target.consumers.size === 0) jobs.delete(key);
      pump();
    };
    target.consumers.add(consumer);
    signal.addEventListener("abort", abort, { once: true });
  });
  pump();
  return result;
}
