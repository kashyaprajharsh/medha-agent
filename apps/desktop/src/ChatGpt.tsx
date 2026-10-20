import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import chatgptMark from "./assets/chatgpt.svg?raw";
import { Sheet } from "./Sheet";
import { useWorkspaceApi } from "./Workspace";
import { profileFor, type Found } from "./modelSetup";

/** ChatGPT Settings → Usage, where people see and limit what apps use of their plan. */
export const MANAGE_USAGE = "https://chatgpt.com/settings/usage";
const NOTICE_SEEN = "medha-chatgpt-plan-notice";

export type ChatGptAccount = { client_id: string; email: string | null; active: boolean; signed_in: boolean; plan_enabled: boolean };

/** OpenAI's mark, drawn in the text colour so it reads on every theme. */
export function ChatGptMark({ size = 18 }: { size?: number }) {
  // Our own bundled file, drawn inline so its currentColor takes the theme's text colour.
  return <span className="cg-mark" style={{ width: size, height: size }} aria-hidden="true" dangerouslySetInnerHTML={{ __html: chatgptMark }} />;
}

/** The button OpenAI asks for, with its approved label. */
export function ContinueWithChatGpt({ onClick, label = "Continue with ChatGPT" }: { onClick: () => void; label?: string }) {
  return <button type="button" className="cg-button" onClick={onClick}><ChatGptMark />{label}</button>;
}

export function PlanLine() {
  const api = useWorkspaceApi();
  return <span className="cg-planline">
    <ChatGptMark size={14} />Using ChatGPT plan ·{" "}
    <button type="button" className="cg-link" onClick={() => void api.openLink(MANAGE_USAGE)}>Manage usage</button>
  </span>;
}

/** A model saved on the plan: OpenAI's Responses route, with the account's token instead of a key. */
const planProfile = (model: string) =>
  profileFor({ protocol: "open-ai-responses", url: "https://api.openai.com/v1", auth: "chatgpt-plan", provider_id: "openai" }, model);

const cleaned = (cause: unknown) => String(cause).replace(/^Error:\s*/, "");

type Step =
  | { at: "waiting"; link: string }
  | { at: "models"; account: ChatGptAccount }
  | { at: "off"; account: ChatGptAccount }
  | { at: "failed"; why: string };

/** Sign in, then pick a model on the plan; `again` signs a registered account in again. */
export function ChatGptSheet({ again, onClose, onSaved, onUseKey }: {
  again?: string;
  onClose: () => void;
  onSaved: (name: string) => void;
  onUseKey: () => void;
}) {
  const api = useWorkspaceApi();
  const [step, setStep] = useState<Step>();
  const [notice, setNotice] = useState(false);
  const attempt = useRef(0);
  const added = useRef("");

  async function signIn(clientId?: string, consent = false) {
    const mine = ++attempt.current;
    setStep(undefined);
    try {
      const begun = await api.settings("settings.chatgpt.begin", { client_id: clientId, consent });
      const link = begun.link as string;
      if (mine !== attempt.current) return;
      setStep({ at: "waiting", link });
      void api.openLink(link);
      const finished = await api.settings("settings.chatgpt.finish");
      if (mine !== attempt.current) return;
      const account = finished.account as ChatGptAccount;
      setStep({ at: account.plan_enabled ? "models" : "off", account });
    } catch (cause) {
      if (mine === attempt.current) setStep({ at: "failed", why: cleaned(cause) });
    }
  }
  useEffect(() => {
    void signIn(again);
    return () => {
      attempt.current++;
      void api.settings("settings.chatgpt.cancel").catch(() => undefined);
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const title = <div className="cg-who"><span className="cg-tile"><ChatGptMark size={24} /></span><div><h2>ChatGPT</h2>
    <small>{step?.at === "models" ? "Signed in" : step?.at === "off" ? `Signed in as ${step.account.email ?? "your account"}` : "Your Plus, Pro, or Business plan"}</small></div></div>;

  return <Sheet label="ChatGPT" onClose={onClose}>
    <div className="cg-sheet">
      {title}
      {!step && <p className="quiet cg-gap">Opening ChatGPT…</p>}
      {step?.at === "waiting" && <Waiting link={step.link} onCancel={onClose} />}
      {step?.at === "failed" && <div className="cg-gap">
        <p role="alert">{step.why}</p>
        <div className="cg-choice"><button type="button" className="btn-gold" onClick={() => void signIn(again)}>Try again</button></div>
      </div>}
      {step?.at === "off" && <div className="cg-gap">
        <div className="cg-note"><b>Plan use is off for Medha.</b> You signed in, but did not allow Medha to use your ChatGPT plan, so Medha cannot send requests with it yet.</div>
        <div className="cg-choice">
          <ContinueWithChatGpt label="Allow plan use" onClick={() => void signIn(step.account.client_id, true)} />
          <button type="button" className="btn-line" onClick={onUseKey}>Use an OpenAI key instead</button>
        </div>
        <p className="quiet">Allowing it opens ChatGPT once more to ask for that permission.</p>
      </div>}
      {step?.at === "models" && <PlanModels account={step.account} onSwitch={() => void signIn()} onSaved={(name) => {
        added.current = name;
        if (seen()) onSaved(name);
        else setNotice(true);
      }} />}
    </div>
    {notice && <PlanNotice onClose={() => { setNotice(false); markSeen(); onSaved(added.current); }} />}
  </Sheet>;
}

function seen() {
  try { return localStorage.getItem(NOTICE_SEEN) === "1"; } catch { return false; }
}
function markSeen() {
  try { localStorage.setItem(NOTICE_SEEN, "1"); } catch { /* shown again next time */ }
}

function Waiting({ link, onCancel }: { link: string; onCancel: () => void }) {
  const api = useWorkspaceApi();
  const [copied, setCopied] = useState(false);
  return <>
    <p className="cg-wait"><span className="cg-pulse" /><span><b>Continue in your browser.</b> Sign in to ChatGPT and allow Medha to use your plan. This page updates by itself when you are done.</span></p>
    <p className="cg-label">Browser did not open?</p>
    <div className="cg-linkbox">
      <code>{link}</code>
      <button type="button" onClick={() => void navigator.clipboard.writeText(link).then(() => setCopied(true))}>{copied ? "Copied" : "Copy link"}</button>
      <button type="button" onClick={() => void api.openLink(link)}>Open again</button>
    </div>
    <p className="quiet">Medha waits up to 5 minutes. Nothing is kept until ChatGPT sends you back.</p>
    <div className="cg-foot"><span className="quiet">Uses your ChatGPT plan, not an API key.</span><button type="button" className="cg-link" onClick={onCancel}>Cancel</button></div>
  </>;
}

function PlanModels({ account, onSwitch, onSaved }: { account: ChatGptAccount; onSwitch: () => void; onSaved: (name: string) => void }) {
  const api = useWorkspaceApi();
  const [models, setModels] = useState<Found[]>();
  const [chosen, setChosen] = useState("");
  const [asDefault, setAsDefault] = useState(false);
  const [busy, setBusy] = useState("");
  const [problem, setProblem] = useState("");
  useEffect(() => {
    let live = true;
    api.settings("settings.model.discover", { profile: planProfile("discovery") })
      .then((reply) => {
        if (!live) return;
        const found = reply.models as Found[];
        setModels(found);
        setChosen(found[0]?.id ?? "");
      })
      .catch((cause) => live && setProblem(cleaned(cause)));
    return () => { live = false; };
  }, [api]);
  async function run(what: "check" | "add") {
    setBusy(what);
    setProblem("");
    try {
      if (what === "check") await api.settings("settings.model.test", { profile: planProfile(chosen) });
      else {
        const reply = await api.settings("settings.model.save", { profile: planProfile(chosen), default: asDefault });
        onSaved(reply.name as string);
      }
    } catch (cause) { setProblem(cleaned(cause)); }
    finally { setBusy(""); }
  }
  return <>
    <p className="cg-label"><span>Account</span><button type="button" className="cg-link" onClick={onSwitch}>Switch</button></p>
    <div className="cg-account"><span className="cg-dot" /><span className="grow">{account.email ?? "ChatGPT account"}</span><span className="quiet">Plan use on</span></div>
    <p className="quiet cg-under"><PlanLine /></p>
    <p className="cg-label"><span>Model</span><span className="quiet">From your plan, in OpenAI's order</span></p>
    {!models && !problem && <p className="quiet">Finding the models on your plan…</p>}
    <div className="cg-models" role="listbox" aria-label="Models on your plan">
      {models?.map((model) => <button type="button" role="option" aria-selected={chosen === model.id} key={model.id}
        className={`cg-model${chosen === model.id ? " on" : ""}`} onClick={() => setChosen(model.id)} onDoubleClick={() => void run("add")}>
        <span className="tick">{chosen === model.id ? "✓" : ""}</span><b>{model.id}</b><span className="quiet">Included in your plan</span>
      </button>)}
    </div>
    <label className="cg-default"><input type="checkbox" checked={asDefault} onChange={(event) => setAsDefault(event.target.checked)} /> Use as the default model for new chats</label>
    {problem && <p role="alert">{problem}</p>}
    <div className="cg-foot">
      <span className="quiet">No API key is used.</span>
      <span className="cg-actions">
        <button type="button" className="btn-line" disabled={!chosen || !!busy} onClick={() => void run("check")}>{busy === "check" ? "Checking…" : "Check connection"}</button>
        <button type="button" className="btn-gold" disabled={!chosen || !!busy} onClick={() => void run("add")}>{busy === "add" ? "Adding…" : `Add ${chosen || "model"}`}</button>
      </span>
    </div>
  </>;
}

/** Shown once, after the first model on the plan is added. */
export function PlanNotice({ onClose }: { onClose: () => void }) {
  const api = useWorkspaceApi();
  // Over the whole window: inside the sliding sheet a fixed box would be placed within the sheet.
  return createPortal(<div className="cg-notice-scrim">
    <div className="cg-notice" role="dialog" aria-modal="true" aria-label="You're using your ChatGPT plan">
      <h2><ChatGptMark size={22} />You're using your ChatGPT plan</h2>
      <ul>
        <li>Medha's requests count toward your plan's usage limits.</li>
        <li>On Plus, the five-hour limit is shared with ChatGPT and your other apps.</li>
        <li>You can see usage and set a limit for Medha in ChatGPT settings.</li>
      </ul>
      <div className="cg-foot">
        <button type="button" className="cg-link" onClick={() => void api.openLink(MANAGE_USAGE)}>Manage usage</button>
        <button type="button" className="btn-gold" autoFocus onClick={onClose}>Got it</button>
      </div>
    </div>
  </div>, document.body);
}

/** Settings: every registered account; models on the plan use the active one. */
export function ChatGptAccounts({ onClose, onSignIn }: { onClose: () => void; onSignIn: (clientId?: string) => void }) {
  const api = useWorkspaceApi();
  const [accounts, setAccounts] = useState<ChatGptAccount[]>([]);
  const [problem, setProblem] = useState("");
  const load = () => api.settings("settings.chatgpt.accounts").then((reply) => setAccounts(reply.accounts as ChatGptAccount[])).catch((cause) => setProblem(cleaned(cause)));
  useEffect(() => { void load(); }, []); // eslint-disable-line react-hooks/exhaustive-deps
  async function act(method: string, clientId: string) {
    setProblem("");
    try {
      const reply = await api.settings(method, { client_id: clientId });
      if (reply.revoked === false) setProblem("Signed out here. OpenAI did not confirm it; you can disconnect Medha in ChatGPT settings.");
      await load();
    } catch (cause) { setProblem(cleaned(cause)); }
  }
  return <Sheet label="ChatGPT accounts" onClose={onClose}>
    <div className="cg-sheet">
      <h2>ChatGPT accounts</h2>
      <p className="quiet">Models on your ChatGPT plan use the active account.</p>
      <div className="cg-accounts">
        {accounts.map((account) => <div className="cg-row" key={account.client_id}>
          <span className="cg-tile"><ChatGptMark size={20} /></span>
          <div><h3>{account.email ?? account.client_id}</h3>
            <p className="quiet">{account.signed_in ? `${account.active ? "Active" : "Signed in"} · Plan use ${account.plan_enabled ? "on" : "off"}` : "Signed out"}</p></div>
          <span className="cg-actions">
            {account.signed_in && !account.active && <button type="button" className="btn-line" onClick={() => void act("settings.chatgpt.switch", account.client_id)}>Use this one</button>}
            {account.signed_in
              ? <button type="button" className="btn-line" onClick={() => void act("settings.chatgpt.sign_out", account.client_id)}>Sign out</button>
              : <button type="button" className="btn-line" onClick={() => onSignIn(account.client_id)}>Sign in</button>}
          </span>
        </div>)}
        {!accounts.length && <p className="quiet">No ChatGPT account yet.</p>}
      </div>
      {problem && <p role="alert">{problem}</p>}
      <div className="cg-choice">{accounts.length ? <button type="button" className="btn-line" onClick={() => onSignIn()}>Add another account</button> : <ContinueWithChatGpt onClick={() => onSignIn()} />}</div>
      <div className="cg-foot"><PlanLine /></div>
    </div>
  </Sheet>;
}
