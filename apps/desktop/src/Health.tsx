import { useEffect, useState } from "react";
import { PageTop, ViewBar } from "./ExtensionParts";
import { Icon } from "./Icon";
import { Spinner } from "./Spinner";
import { useWorkspaceApi } from "./Workspace";

type Check = { health: "ok" | "warn" | "error"; title: string; detail: string; fixable: boolean };
type Active = {
  profile: string;
  model: string;
  base_url: string;
  protocol: string;
  model_from: string;
  base_url_from: string;
  key_from: string;
  key_present: boolean;
};
type Report = {
  checks: Check[];
  fixable: boolean;
  active: Active | { error: string } | null;
  medha_home: string;
  config_path: string;
  config_exists: boolean;
  medha_env: string[];
  ignored_env: string[];
  project_lock: string | null;
  lock_executor: string | null;
};

/** Whether Medha can run, and where each part of the setup comes from. */
export function Health() {
  const api = useWorkspaceApi();
  const [report, setReport] = useState<Report>();
  const [error, setError] = useState("");
  const [fixed, setFixed] = useState<string[]>();
  const [working, setWorking] = useState(false);
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    let active = true;
    setError("");
    void api
      .settings("settings.health")
      .then((result) => active && setReport(result as unknown as Report))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [refresh]);

  async function fix() {
    setWorking(true);
    setError("");
    try {
      const result = await api.settings("settings.health.fix");
      setFixed(result.applied as string[]);
      setRefresh((value) => value + 1);
      window.dispatchEvent(new CustomEvent("medha-preferences"));
    } catch (cause) {
      setError(String(cause));
    } finally {
      setWorking(false);
    }
  }

  const errors = report?.checks.filter((check) => check.health === "error").length ?? 0;
  const warnings = report?.checks.filter((check) => check.health === "warn").length ?? 0;
  const active = report?.active && !("error" in report.active) ? report.active : undefined;
  const again = (
    <button className="ext-add" onClick={() => setRefresh((value) => value + 1)}>
      <Icon name="refresh" />
      Check again
    </button>
  );
  return (
    <div className="health">
      <PageTop title="Health">Whether Medha can run, and where each part of its setup comes from.</PageTop>
      {error && <p className="surface-error" role="alert">{error}</p>}
      {/* With no verdict to sit beside, the way to try again stands alone. */}
      {!report && error && <ViewBar>{again}</ViewBar>}
      {!report && !error && <p className="quiet">Checking…</p>}
      {report && (
        <>
          <div className={`health-verdict ${errors ? "error" : warnings ? "warn" : "ok"}`}>
            <Icon name={errors || warnings ? "x" : "check"} />
            <b>{errors ? "Needs attention" : warnings ? `${warnings} ${warnings === 1 ? "warning" : "warnings"}` : "Everything is working"}</b>
            {report.fixable && (
              <button className="btn-gold" disabled={working} onClick={() => void fix()}>
                {working ? <Spinner /> : null}
                Fix automatically
              </button>
            )}
            {again}
          </div>
          {fixed && (
            <p className="quiet" role="status">
              {fixed.length ? `Fixed: ${fixed.join("; ")}` : "Nothing needed fixing."}
            </p>
          )}
          <ul className="health-checks">
            {report.checks.map((check, index) => (
              <li key={index} className={`health-${check.health}`}>
                <Icon name={check.health === "ok" ? "check" : "x"} />
                <div>
                  <b>{check.title}</b>
                  <p>{check.detail}</p>
                </div>
                {check.fixable && <small>Can be fixed automatically</small>}
              </li>
            ))}
          </ul>
          <section className="health-block">
            <h3>Active model</h3>
            {active ? (
              <dl>
                <dt>Profile</dt>
                <dd>{active.profile}</dd>
                <dt>Model</dt>
                <dd>
                  <code>{active.model}</code> <small>from {active.model_from}</small>
                </dd>
                <dt>Endpoint</dt>
                <dd>
                  <code>{active.base_url}</code> <small>from {active.base_url_from}</small>
                </dd>
                <dt>Protocol</dt>
                <dd>{active.protocol}</dd>
                <dt>Key</dt>
                <dd>
                  {active.key_present ? "Found" : "Not set"} <small>in {active.key_from}</small>
                </dd>
              </dl>
            ) : (
              <p className="quiet">
                {report.active && "error" in report.active ? report.active.error : "No model is set up yet. Add one in Models."}
              </p>
            )}
          </section>
          <section className="health-block">
            <h3>Where settings live</h3>
            <dl>
              <dt>Medha home</dt>
              <dd><code>{report.medha_home}</code></dd>
              <dt>Config file</dt>
              <dd>
                <code>{report.config_path}</code> {!report.config_exists && <small>not created yet</small>}
              </dd>
              {report.project_lock && (
                <>
                  <dt>Project lock</dt>
                  <dd><code>{report.project_lock}</code>{report.lock_executor && <small> · runs with {report.lock_executor}</small>}</dd>
                </>
              )}
              {report.medha_env.length > 0 && (
                <>
                  <dt>Environment</dt>
                  <dd>{report.medha_env.join(", ")} <small>override the config file</small></dd>
                </>
              )}
            </dl>
            {report.ignored_env.length > 0 && (
              <p className="quiet">
                Medha ignores {report.ignored_env.join(", ")}. Set keys in Models instead.
              </p>
            )}
          </section>
        </>
      )}
    </div>
  );
}
