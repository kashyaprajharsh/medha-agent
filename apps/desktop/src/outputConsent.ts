import type { ScreenOrigins } from "./api";

type Declaration = ScreenOrigins & { clipboard?: boolean; identity?: string };
type Grant = { identity: string; scope: string; at: number; durable?: boolean };
const STORE = "medha-screen-grants-v2";
const MAX_GRANTS = 64;
const LIFETIME = 90 * 24 * 60 * 60_000;
const grants = new Map<string, Grant>();
let loaded = false;

// Capabilities stay separate. Schemes, wildcard hosts and ports are never
// reduced to a hostname: doing so would silently widen a remembered grant.
export function screenScope(page: Declaration): string {
  const sorted = (origins: string[]) => [...new Set(origins)].sort();
  return JSON.stringify({
    connect: sorted(page.connect), resources: sorted(page.resources),
    frames: sorted(page.frames), clipboard: page.clipboard === true,
  });
}

export function screenIdentity(server: string, resource: string, page: Declaration): string {
  // A page without a backend connection identity can be approved for this
  // sitting, but never inherits a persisted approval under just a server name.
  return JSON.stringify([server, resource, page.identity ?? ""]);
}

function prune() {
  for (const [key, grant] of grants)
    if (!Number.isFinite(grant.at) || grant.at > Date.now() || Date.now() - grant.at > LIFETIME) grants.delete(key);
  while (grants.size > MAX_GRANTS) grants.delete(grants.keys().next().value!);
}

function load() {
  if (loaded) return;
  loaded = true;
  try {
    const saved: unknown = JSON.parse(localStorage.getItem(STORE) ?? "[]");
    if (!Array.isArray(saved)) return;
    for (const grant of saved.slice(-MAX_GRANTS))
      if (typeof grant?.identity === "string" && typeof grant?.scope === "string" &&
          typeof grant?.at === "number" && grant.identity.length < 2000 && grant.scope.length < 20_000)
        grants.set(grant.identity, { ...grant, durable: true });
  } catch { /* Storage may be disabled. Hostname-only legacy grants are ignored. */ }
  prune();
}

export function screenAgreed(identity: string, scope: string): boolean {
  load();
  prune();
  return grants.get(identity)?.scope === scope;
}

export function agreeScreen(identity: string, scope: string, durable: boolean) {
  load();
  grants.delete(identity);
  grants.set(identity, { identity, scope, at: Date.now(), durable });
  prune();
  if (!durable) return;
  try {
    localStorage.setItem(STORE, JSON.stringify([...grants.values()].filter((grant) => grant.durable)));
  } catch { /* The approval still holds for this sitting. */ }
}

export function screenReach(page: Declaration): string {
  const parts = [
    page.resources.length ? `Load resources: ${page.resources.join(", ")}` : "",
    page.connect.length ? `Connect: ${page.connect.join(", ")}` : "",
    page.frames.length ? `Embed frames: ${page.frames.join(", ")}` : "",
    page.clipboard ? "Write to the clipboard in Preview" : "",
  ];
  return parts.filter(Boolean).join(". ");
}
