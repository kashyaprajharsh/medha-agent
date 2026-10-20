import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Sheet } from "./Sheet";
import { useWorkspaceApi } from "./Workspace";
import { profileFor, type Found } from "./modelSetup";

/** ChatGPT Settings → Usage, where people see and limit what apps use of their plan. */
export const MANAGE_USAGE = "https://chatgpt.com/settings/usage";
const NOTICE_SEEN = "medha-chatgpt-plan-notice";

export type ChatGptAccount = { client_id: string; email: string | null; active: boolean; signed_in: boolean; plan_enabled: boolean };

/** OpenAI's mark from its Sign in with ChatGPT guidelines, in the text colour so it reads on every theme. */
const MARK =
  "M9.20509 8.76514V6.50548C9.20509 6.31517 9.27649 6.17237 9.44293 6.07733L13.9861 3.46091C14.6046 3.10416 15.342 2.93772 16.103 2.93772C18.9573 2.93772 20.7651 5.14986 20.7651 7.50457C20.7651 7.67101 20.7651 7.86132 20.7412 8.05164L16.0316 5.29243C15.7462 5.12599 15.4607 5.12599 15.1753 5.29243L9.20509 8.76514ZM19.8136 17.566V12.1665C19.8136 11.8334 19.6708 11.5955 19.3854 11.4291L13.4152 7.95637L15.3656 6.83836C15.5321 6.74332 15.6749 6.74332 15.8413 6.83836L20.3845 9.45477C21.6928 10.216 22.5728 11.8334 22.5728 13.4032C22.5728 15.2108 21.5025 16.8759 19.8136 17.5657V17.566ZM7.80173 12.8088L5.8513 11.6671C5.68486 11.5721 5.61346 11.4293 5.61346 11.239V6.00616C5.61346 3.46114 7.56389 1.53436 10.2042 1.53436C11.2033 1.53436 12.1307 1.86746 12.9159 2.46205L8.2301 5.17374C7.94475 5.34018 7.80196 5.57801 7.80196 5.91112V12.809L7.80173 12.8088ZM12 15.2349L9.20509 13.6651V10.3352L12 8.76537L14.7947 10.3352V13.6651L12 15.2349ZM13.7958 22.4659C12.7967 22.4659 11.8693 22.1328 11.0841 21.5382L15.7699 18.8265C16.0553 18.6601 16.198 18.4223 16.198 18.0892V11.1912L18.1723 12.3329C18.3388 12.4279 18.4102 12.5707 18.4102 12.7611V17.9939C18.4102 20.5389 16.4359 22.4657 13.7958 22.4657V22.4659ZM8.15848 17.1617L3.61528 14.5453C2.30696 13.784 1.42701 12.1667 1.42701 10.5969C1.42701 8.76537 2.52115 7.12417 4.20987 6.43431V11.8575C4.20987 12.1906 4.35266 12.4284 4.63802 12.5948L10.5846 16.0437L8.63415 17.1617C8.46771 17.2567 8.32492 17.2567 8.15848 17.1617ZM7.897 21.0626C5.20919 21.0626 3.23488 19.0407 3.23488 16.5432C3.23488 16.3529 3.25875 16.1626 3.2824 15.9723L7.96817 18.684C8.25352 18.8504 8.53911 18.8504 8.82447 18.684L14.7947 15.2351V17.4948C14.7947 17.6851 14.7233 17.8279 14.5568 17.9229L10.0136 20.5394C9.39518 20.8961 8.6578 21.0626 7.89677 21.0626H7.897ZM13.7958 23.8929C16.6739 23.8929 19.0762 21.8475 19.6235 19.1358C22.2874 18.4459 24 15.9484 24 13.4034C24 11.7383 23.2865 10.121 22.002 8.95546C22.121 8.45591 22.1924 7.95637 22.1924 7.45705C22.1924 4.05573 19.4331 1.51048 16.2458 1.51048C15.6037 1.51048 14.9852 1.60553 14.3668 1.81971C13.2963 0.773101 11.8215 0.107117 10.2042 0.107117C7.32606 0.107117 4.92383 2.15259 4.37653 4.86428C1.7126 5.55414 0 8.05164 0 10.5967C0 12.2617 0.713506 13.8791 1.99795 15.0446C1.87904 15.5441 1.80764 16.0437 1.80764 16.543C1.80764 19.9443 4.56685 22.4896 7.75421 22.4896C8.39632 22.4896 9.01478 22.3945 9.63324 22.1803C10.7035 23.2269 12.1783 23.8929 13.7958 23.8929Z";

export function ChatGptMark({ size = 18 }: { size?: number }) {
  return <svg className="cg-mark" width={size} height={size} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d={MARK} /></svg>;
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
