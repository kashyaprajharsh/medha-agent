import { useWorkspaceApi } from "./Workspace";
import {
  startTransition,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { type Attachment, type SessionChange } from "./api";
import { messageRequest } from "./messageRequest";
import type { OutputAbout } from "./outputBlocks";
import {
  reduceLive,
  recordSent,
  startingLive,
  type Approval,
  type ApprovalChoice,
  type LiveState,
  type QuestionForm,
} from "./live";

export function useLive() {
  const api = useWorkspaceApi();
  const [live, setLive] = useState<Record<string, LiveState>>({});
  const latest = useRef(live);
  const publishTimer = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  const publish = useCallback((streaming = false) => {
    if (publishTimer.current !== undefined) clearTimeout(publishTimer.current);
    publishTimer.current = undefined;
    const snapshot = latest.current;
    if (streaming) startTransition(() => setLive(snapshot));
    else setLive(snapshot);
  }, []);
  const opened = useRef(new Set<string>());
  const opening = useRef(new Map<string, Promise<void>>());

  useEffect(() => {
    let off: (() => void) | undefined;
    let gone = false;
    void api
      .onLive((key, frame) => {
        if (frame.method === "exit") opened.current.delete(key);
        if (gone) return;
        const before = latest.current[key] ?? startingLive();
        const after = reduceLive(before, frame);
        if (after === before) return;
        latest.current = { ...latest.current, [key]: after };
        const streaming =
          frame.method === "event" &&
          (frame.params?.kind === "model.text" ||
            frame.params?.kind === "model.reasoning");
        // Preserve every delta in order. Only defer painting, never controls.
        if (!streaming) publish();
        else if (publishTimer.current === undefined)
          publishTimer.current = setTimeout(() => publish(true), 16);
      })
      .then((unlisten) => (gone ? unlisten() : (off = unlisten)));
    return () => {
      gone = true;
      if (publishTimer.current !== undefined)
        clearTimeout(publishTimer.current);
      publishTimer.current = undefined;
      off?.();
    };
  }, [api, publish]);

  const patch = useCallback(
    (key: string, change: (state: LiveState) => LiveState) => {
      const before = latest.current[key] ?? startingLive();
      const after = change(before);
      if (after === before) return;
      latest.current = { ...latest.current, [key]: after };
      publish();
    },
    [publish],
  );

  const ensureOpen = useCallback(async (key: string, resume: string | null) => {
    if (opening.current.has(key)) return opening.current.get(key)!;
    if (opened.current.has(key)) return;
    const promise = (async () => {
      try {
        const reused = await api.liveOpen(key, resume);
        opened.current.add(key);
        if (reused) {
          await api.liveCall(key, "session.presentation");
          return;
        }
        const settings = await api.liveCall(key, "session.settings");
        patch(key, (state) =>
          reduceLive(state, { method: "settings", params: settings }),
        );
      } catch (error) {
        opened.current.delete(key);
        throw error;
      } finally {
        opening.current.delete(key);
      }
    })();
    opening.current.set(key, promise);
    return promise;
  }, []);

  const loadSettings = useCallback(
    async (key: string, resume: string | null) => {
      const alreadyOpen = opened.current.has(key);
      await ensureOpen(key, resume);
      // Opening already fetched settings. An existing chat still needs a fresh
      // catalogue: another window or the Settings page may have changed it.
      if (!alreadyOpen) return;
      const settings = await api.liveCall(key, "session.settings");
      patch(key, (state) =>
        reduceLive(state, { method: "settings", params: settings }),
      );
    },
    [api, ensureOpen, patch],
  );

  const send = useCallback(
    async (
      key: string,
      resume: string | null,
      text: string,
      attachments: Attachment[] = [],
      /** The saved output this message asks to change, and what to call it beside the message. */
      about?: OutputAbout,
    ) => {
      // Reject before opening a chat or optimistically recording an unsent
      // message. The composer keeps the text and attachments for correction.
      let params: ReturnType<typeof messageRequest> & { about?: { artifact_id: string; revision_id: string } };
      // A message sent into a running turn steers it; only one that starts a
      // turn can be about an output, so the reference is not claimed otherwise.
      const starts = latest.current[key]?.status !== "running" && latest.current[key]?.status !== "starting";
      try {
        params = messageRequest(text, attachments);
        if (about && starts) params = { ...params, about: { artifact_id: about.artifact_id, revision_id: about.revision_id } };
      } catch (cause) {
        patch(key, (state) => ({
          ...state,
          status: latest.current[key]?.status ?? "idle",
          error: String(cause),
        }));
        throw cause;
      }
      const message = {
        kind: "user" as const,
        id: crypto.randomUUID(),
        text: params.content,
        images: attachments.flatMap((attachment) => "data" in attachment
          ? [{ name: attachment.name, mime: attachment.mime, preview: attachment.preview }]
          : []),
        about: params.about ? about?.label : undefined,
        ts: Date.now() / 1000,
      };
      const before = latest.current[key];
      const opening = !before || before.status === "exited";
      patch(key, (state) => ({
        ...recordSent(
          opening
            ? {
                ...startingLive(),
                sessionId: state.sessionId,
                model: state.model,
                settings: state.settings,
                settled: state.settled,
              }
            : state,
          message,
        ),
        status: opening ? "starting" : "running",
        error: undefined,
        notice: undefined,
        verification: undefined,
        stopReason: undefined,
      }));
      try {
        await ensureOpen(key, resume);
        // Recovery replaces the live tail. Keep this not-yet-dispatched
        // message in an idle chat's timeline under its optimistic id; the
        // acceptance then enriches it rather than adding a duplicate.
        patch(key, (state) => state.status === "idle" && !state.items.some((item) => item.id === message.id)
          ? { ...state, status: "running", items: [...state.items, message] }
          : state);
        await api.liveCall(key, "message.send", params);
      } catch (cause) {
        patch(key, (state) => ({
          ...state,
          status: opened.current.has(key)
            ? before?.status === "running"
              ? state.status
              : "idle"
            : "exited",
          error: String(cause),
        }));
        throw cause;
      }
    },
    [patch, ensureOpen],
  );

  const takeReturned = useCallback(
    (key: string) => {
      const returned = latest.current[key]?.returned ?? [];
      if (returned.length) patch(key, (state) => ({ ...state, returned: [] }));
      return returned;
    },
    [patch],
  );

  const cancel = useCallback(
    async (key: string) => {
      await api
        .liveCall(key, "cancel")
        .catch((cause) =>
          patch(key, (state) => ({ ...state, error: String(cause) })),
        );
    },
    [patch],
  );

  const approve = useCallback(
    (key: string, approval: Approval, decision: ApprovalChoice) => {
      patch(key, (state) => ({
        ...state,
        approvals: state.approvals.map((item) =>
          item.gateId === approval.gateId ? { ...item, pending: true } : item,
        ),
      }));
      void api
        .liveCall(key, "approval.respond", {
          gate_id: approval.gateId,
          decision,
        })
        .then(() =>
          patch(key, (state) => ({
            ...state,
            approvals: state.approvals.filter(
              (item) => item.gateId !== approval.gateId,
            ),
          })),
        )
        .catch((cause) =>
          patch(key, (state) => ({
            ...state,
            error: String(cause),
            approvals: state.approvals.map((item) =>
              item.gateId === approval.gateId
                ? { ...item, pending: false }
                : item,
            ),
          })),
        );
    },
    [patch],
  );

  const clearTail = useCallback(
    (key: string, persisted: Set<string>) => {
      if (!latest.current[key]) return;
      patch(key, (state) => {
        if (state.status === "running" || state.status === "starting")
          return state;
        const unsaved = state.sent.filter(
          (message) => !persisted.has(message.text),
        );
        if (unsaved.length === state.sent.length && state.items.length === 0)
          return state;
        return { ...state, sent: unsaved, items: [] };
      });
    },
    [patch],
  );

  const configure = useCallback(
    async (key: string, resume: string | null, change: SessionChange) => {
      await ensureOpen(key, resume);
      const settings = await api.liveCall(key, "session.configure", change);
      patch(key, (state) =>
        reduceLive(state, { method: "settings", params: settings }),
      );
    },
    [ensureOpen, patch],
  );

  const answer = useCallback(
    (
      key: string,
      form: QuestionForm,
      answers?: { selected: string[]; other?: string }[],
    ) => {
      patch(key, (state) => ({
        ...state,
        questions: state.questions.map((question) =>
          question.id === form.id ? { ...question, pending: true } : question,
        ),
      }));
      void api
        .liveCall(key, "question.respond", {
          question_id: form.id,
          ...(answers ? { answers } : { dismiss: true }),
        })
        .catch((cause) =>
          patch(key, (state) => ({
            ...state,
            error: String(cause),
            questions: state.questions.map((question) =>
              question.id === form.id
                ? { ...question, pending: false }
                : question,
            ),
          })),
        );
    },
    [patch],
  );

  return {
    live,
    send,
    cancel,
    approve,
    clearTail,
    takeReturned,
    ensureOpen,
    loadSettings,
    configure,
    answer,
  };
}
