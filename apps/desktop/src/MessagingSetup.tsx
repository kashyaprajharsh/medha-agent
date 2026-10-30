import { useEffect, useRef, useState } from "react";
import { desktop } from "./api";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
import telegramLogo from "./assets/messaging/telegram.png";

/** Telegram's own mark; a platform without one bundled shows its first letter. */
const LOGOS: Record<string, string> = { telegram: telegramLogo };

export type Platform = {
  name: string;
  label: string;
  ready: boolean;
  account: string | null;
  home: boolean;
  steps: string;
  token: string;
  one_tap: boolean;
  /** Made by scanning: only its owner can message it, so nobody else can be let in. */
  owner_only: boolean;
};
type Setup = { id: number; link: string; qr_svg: string; expires_in: number };
type Step =
  | { state: "waiting"; expires_in: number }
  | { state: "created"; bot: string }
  | { state: "heard"; bot: string; name: string; user: string };
type Saved = { account: string; folder: string; paired: boolean; problem: string | null };

/** Where the sheet is: each phase is one thing the person does. */
type Phase =
  | { at: "intro" }
  | { at: "starting" }
  | { at: "scan"; setup: Setup; step: Step; ends: number }
  | { at: "saving"; setup: Setup }
  | { at: "token"; typed: string; checking: boolean }
  | { at: "done"; saved: Saved };

export const cleaned = (cause: unknown) => String(cause).replace(/^Error:\s*/, "");

/** A folder by its last part, as the workspace menu names projects. */
export const named = (folder: string) => folder.split(/[\\/]/).filter(Boolean).pop() ?? folder;

export function Plate({ platform, large }: { platform: Platform; large?: boolean }) {
  const logo = LOGOS[platform.name];
  return logo ? (
    <img className={`cx-plate${large ? " ms-large" : ""}`} src={logo} alt="" />
  ) : (
    <span className={`cx-plate ms-letter${large ? " ms-large" : ""}`} aria-hidden="true">
      {platform.label[0]}
    </span>
  );
}

/** One platform: making its bot, pasting one made by hand, or what is set up. */
export function PlatformSheet({
  platform,
  suggested,
  onChanged,
}: {
  platform: Platform;
  suggested: string | null;
  onChanged: () => void;
}) {
  const api = useWorkspaceApi();
  const [phase, setPhase] = useState<Phase>({ at: "intro" });
  // Where new chats start; nothing is saved until the person pairs.
  const [folder, setFolder] = useState(suggested);
  const changeFolder = async () => {
    const chosen = await desktop.chooseFolder("Choose the project your phone works in");
    if (chosen) setFolder(chosen);
  };
  const folderLine = (
    <div className="ms-folder">
      <Icon name="folder" />
      <span>
        {folder ? (
          <>
            <span>
              New chats start in <b>{named(folder)}</b>
            </span>
            <small>{folder}</small>
          </>
        ) : (
          <>Choose the project your phone works in.</>
        )}
      </span>
      <button type="button" className="btn-line" onClick={() => void changeFolder()}>
        {folder ? "Change" : "Choose"}
      </button>
    </div>
  );
  const [problem, setProblem] = useState<string>();
  const [, tick] = useState(0);
  // Each start is numbered, so an answer for one the sheet moved on from changes nothing.
  const attempt = useRef(0);
  const pending = useRef<number | undefined>(undefined);
  const cancel = (id: number) => void api.settings("settings.messaging.cancel", { id }).catch(() => undefined);

  useEffect(
    () => () => {
      attempt.current++;
      // Closing the sheet mid-setup drops the bot's token from Medha at once.
      if (pending.current !== undefined) cancel(pending.current);
    },
    [], // eslint-disable-line react-hooks/exhaustive-deps
  );
  useEffect(() => {
    if (phase.at !== "scan" || phase.step.state !== "waiting") return;
    const clock = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(clock);
  }, [phase]);

  async function scan() {
    const mine = ++attempt.current;
    setProblem(undefined);
    setPhase({ at: "starting" });
    try {
      const setup = (await api.settings("settings.messaging.begin", { platform: platform.name })) as Setup;
      if (mine !== attempt.current) return cancel(setup.id);
      pending.current = setup.id;
      let step: Step = { state: "waiting", expires_in: setup.expires_in };
      setPhase({ at: "scan", setup, step, ends: Date.now() + setup.expires_in * 1000 });
      // Each wait answers within a minute; asked again until the owner's message is heard.
      while (step.state !== "heard") {
        step = (await api.settings("settings.messaging.await", { id: setup.id })) as Step;
        if (mine !== attempt.current) return;
        const now = step;
        setPhase((was) =>
          was.at === "scan"
            ? { ...was, step: now, ends: now.state === "waiting" ? Date.now() + now.expires_in * 1000 : was.ends }
            : was,
        );
      }
    } catch (cause) {
      if (mine !== attempt.current) return;
      pending.current = undefined;
      setProblem(cleaned(cause));
      setPhase({ at: "intro" });
    }
  }

  async function save(setup: Setup) {
    const mine = attempt.current;
    setPhase({ at: "saving", setup });
    try {
      const saved = (await api.settings("settings.messaging.save", { id: setup.id, folder })) as Saved;
      if (mine !== attempt.current) return;
      pending.current = undefined;
      setPhase({ at: "done", saved });
      onChanged();
    } catch (cause) {
      if (mine !== attempt.current) return;
      setProblem(cleaned(cause));
      setPhase({ at: "intro" });
    }
  }

  function notMe(setup: Setup, bot: string) {
    attempt.current++;
    pending.current = undefined;
    cancel(setup.id);
    setProblem(`Setup stopped. Delete @${bot} in @BotFather, then set up again.`);
    setPhase({ at: "intro" });
  }

  async function saveToken(typed: string) {
    const mine = ++attempt.current;
    setProblem(undefined);
    setPhase({ at: "token", typed, checking: true });
    try {
      const saved = (await api.settings("settings.messaging.save_token", {
        platform: platform.name,
        token: typed.trim(),
        folder,
      })) as Saved;
      if (mine !== attempt.current) return;
      setPhase({ at: "done", saved });
      onChanged();
    } catch (cause) {
      if (mine !== attempt.current) return;
      setProblem(cleaned(cause));
      // The pasted token is cleared: nothing secret stays in the form after a refusal.
      setPhase({ at: "token", typed: "", checking: false });
    }
  }

  const hero = (
    <div className="cx-hero">
      <Plate platform={platform} large />
      <div>
        <h2>{platform.label}</h2>
        <small>{platform.ready && phase.at === "intro" ? platform.account ?? "Set up" : "Talk to Medha from your phone"}</small>
      </div>
    </div>
  );
  const problemLine = problem && (
    <p className="cx-problem" role="alert">
      {problem}
    </p>
  );

  if (phase.at === "scan" || phase.at === "saving") {
    const setup = phase.setup;
    const step: Step = phase.at === "scan" ? phase.step : { state: "heard", bot: "", name: "", user: "" };
    const left = phase.at === "scan" ? Math.max(0, Math.ceil((phase.ends - Date.now()) / 1000)) : 0;
    return (
      <>
        {hero}
        {step.state === "waiting" ? (
          <>
            {/* An image, so the SVG can never run anything. */}
            <img
              className="ms-code-plate"
              alt="Code to scan with your phone"
              src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(setup.qr_svg)}`}
            />
            <ol className="ms-steps">
              <li>Scan the code with your phone's camera. Telegram opens Medha Setup.</li>
              <li>Tap Start, then Create bot. Keep the name it suggests or pick your own.</li>
            </ol>
            <div className="cx-foot">
              <button className="cx-connect busy waiting" aria-live="polite">
                <span className="cx-label">Waiting for Telegram</span>
              </button>
              <p className="cx-hint">
                {Math.floor(left / 60)}:{String(left % 60).padStart(2, "0")} left.{" "}
                <button className="cx-link" onClick={() => void api.openLink(setup.link)}>
                  Open in Telegram here
                </button>
                <button
                  className="cx-link quiet"
                  onClick={() => {
                    attempt.current++;
                    pending.current = undefined;
                    cancel(setup.id);
                    setPhase({ at: "intro" });
                  }}
                >
                  Cancel
                </button>
              </p>
            </div>
          </>
        ) : step.state === "created" ? (
          <>
            <p className="ms-moment">
              <Icon name="check" />
              <span>
                <b>@{step.bot} is yours.</b> Send it any message from your phone, so Medha knows it is you.
              </span>
            </p>
            <div className="cx-foot">
              <button className="cx-connect busy waiting" aria-live="polite">
                <span className="cx-label">Waiting for your message</span>
              </button>
            </div>
          </>
        ) : (
          <>
            {phase.at === "scan" && (
              <div className="ms-moment ms-ask">
                <small>Is this you?</small>
                <b>{step.name}</b>
                <span>Telegram {step.user}</span>
              </div>
            )}
            <ul className="cx-facts">
              <li>
                <Icon name="lock" />
                <span>Only this person can message the bot, and Medha answers them from this computer.</span>
              </li>
            </ul>
            {folderLine}
            <div className="cx-foot">
              <button
                className={`cx-connect ${phase.at === "saving" ? "busy waiting" : "gild"}`}
                onClick={phase.at === "scan" && folder ? () => void save(setup) : undefined}
                disabled={phase.at === "scan" && !folder}
                aria-live="polite"
              >
                <span className="cx-label">{phase.at === "saving" ? "Pairing" : "Yes, pair me"}</span>
              </button>
              {phase.at === "scan" && step.state === "heard" && (
                <p className="cx-hint">
                  <button className="cx-link quiet" onClick={() => notMe(setup, step.bot)}>
                    No, that's not me
                  </button>
                </p>
              )}
            </div>
          </>
        )}
        {problemLine}
      </>
    );
  }

  if (phase.at === "token") {
    return (
      <>
        {hero}
        <ol className="ms-steps">
          {platform.steps.split("\n").map((line) => (
            <li key={line}>{line.replace(/^\d+\.\s*/, "")}</li>
          ))}
        </ol>
        <form
          className="ms-token"
          onSubmit={(event) => {
            event.preventDefault();
            if (phase.typed.trim() && folder && !phase.checking) void saveToken(phase.typed);
          }}
        >
          <label>
            <span>{platform.token}</span>
            <input
              type="password"
              autoComplete="off"
              autoFocus
              value={phase.typed}
              placeholder="Paste it here"
              disabled={phase.checking}
              onChange={(event) => setPhase({ at: "token", typed: event.target.value, checking: false })}
            />
          </label>
          <p className="quiet">Medha checks it with {platform.label}, then keeps it on this computer. It is never shown again.</p>
          {folderLine}
          <div className="cx-foot">
            <button
              type="submit"
              className={`cx-connect ${phase.checking ? "busy waiting" : "gild"}`}
              disabled={!phase.typed.trim() || !folder}
            >
              <span className="cx-label">{phase.checking ? `Checking with ${platform.label}` : "Check and save"}</span>
            </button>
            <p className="cx-hint">
              <button type="button" className="cx-link quiet" onClick={() => setPhase({ at: "intro" })}>
                Back
              </button>
            </p>
          </div>
        </form>
        {problemLine}
      </>
    );
  }

  if (phase.at === "done") {
    const { saved } = phase;
    return (
      <>
        {hero}
        <p className="ms-moment">
          <Icon name="check" />
          <span>
            <b>{saved.paired ? "Paired." : "Saved."}</b>{" "}
            {saved.paired
              ? `Message ${saved.account} from your phone.`
              : `Message ${saved.account} from your phone. It answers with a code; let yourself in under People.`}
          </span>
        </p>
        <ul className="cx-facts">
          <li>
            <Icon name="folder" />
            <span>New chats start in {saved.folder}. Add more under Projects.</span>
          </li>
          <li>
            <Icon name="refresh" />
            <span>
              {saved.problem
                ? `It could not be set to start at login: ${saved.problem}`
                : "The gateway runs now, at login and after a crash."}
            </span>
          </li>
        </ul>
        <div className="cx-foot">
          <button className="cx-connect done">
            <span className="cx-label">
              <Icon name="check" />
              {saved.paired ? "Paired" : "Saved"}
            </span>
          </button>
        </div>
      </>
    );
  }

  return (
    <>
      {hero}
      <p className="cx-about">
        {platform.ready
          ? `Medha answers ${platform.account ?? "your bot"} on ${platform.label}. Setting it up again replaces that bot.`
          : `Medha gets its own ${platform.label} bot. Message it from your phone and Medha works on this computer.`}
      </p>
      <ul className="cx-facts">
        <li>
          <Icon name="lock" />
          <span>
            {platform.one_tap
              ? "A bot made by scanning is yours alone: only you can ever message it."
              : "Others who message the bot get a code, and reach Medha only if you let them in."}
          </span>
        </li>
        <li>
          <Icon name="shield" />
          <span>When Medha needs a yes, the chat shows buttons only a paired person can press.</span>
        </li>
        {platform.one_tap && (
          <li>
            <Icon name="link" />
            <span>
              Medha Setup makes the bot, and can read its token for as long as the bot exists. To share Medha, or if
              no one else should hold the token, use a bot you made in @BotFather.
            </span>
          </li>
        )}
      </ul>
      <div className="cx-foot">
        {platform.one_tap ? (
          <button
            className={`cx-connect ${phase.at === "starting" ? "busy waiting" : "gild"}`}
            onClick={phase.at === "intro" ? () => void scan() : undefined}
            aria-live="polite"
          >
            <span className="cx-label">{phase.at === "starting" ? "Making a code" : platform.ready ? "Make a new bot" : "Make my bot"}</span>
          </button>
        ) : (
          <button className="cx-connect gild" onClick={() => setPhase({ at: "token", typed: "", checking: false })}>
            <span className="cx-label">Connect a bot</span>
          </button>
        )}
        {platform.one_tap && (
          <p className="cx-hint">
            You'll scan a code with your phone.{" "}
            <button className="cx-link" onClick={() => setPhase({ at: "token", typed: "", checking: false })}>
              Use a bot I made in @BotFather
            </button>
          </p>
        )}
      </div>
      {problemLine}
    </>
  );
}
