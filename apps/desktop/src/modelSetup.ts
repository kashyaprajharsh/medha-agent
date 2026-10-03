export type Provider = { name: string; url: string; auth: string };
export type Protocol = {
  value: string;
  label: string;
  available: boolean;
  auth: string;
  discovery: boolean;
  providers: Provider[];
};
export type Found = { id: string; context_length: number | null };

/** A server on this computer answers without a key, so it can be looked for unasked. */
export function isLocal(url: string) {
  try {
    return ["localhost", "127.0.0.1", "[::1]"].includes(new URL(url).hostname);
  } catch {
    return false;
  }
}

export function hostOf(url: string) {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

export const plainName = (name: string) => name.replace(/\s*\(local\)$/, "");

/** A server of your own is asked for a key only when you gave one. */
export const authFor = (choice: { custom: boolean; auth: string }, key: string) => (choice.custom && !key ? "none" : choice.auth);

/** The profile a new model is saved as; everything unasked keeps the value Settings would give it. */
export function profileFor(connection: { protocol: string; url: string; auth: string }, model = "", context: number | null = null) {
  return {
    model: model || "model-discovery",
    base_url: connection.url,
    protocol: connection.protocol,
    auth: connection.auth,
    max_ctx: context,
    max_output_tokens: null,
    reasoning: "unknown",
    reasoning_efforts: null,
    image_input: "auto",
    token_counter: "none",
    token_accounting: "adaptive",
    chat_token_limit: "auto",
    headers: {},
  };
}

export function contextLabel(tokens: number | null) {
  if (!tokens) return "";
  const size = tokens >= 1_000_000 ? `${Number((tokens / 1_000_000).toFixed(1))}M` : `${Math.round(tokens / 1000)}k`;
  return `${size} context`;
}

/**
 * What an empty context size leads to. `published` is the size a session would look up:
 * a number, `null` when there is none, `undefined` while that is not known yet.
 */
export function sizeNote(listed: boolean, published: number | null | undefined) {
  const lead = listed ? "The server did not report it. " : "";
  if (published === undefined) return `${lead}Leave it empty if you are not sure.`;
  if (published === null) return `${lead}Medha found no size for this model. Add it, or long chats will not be shortened automatically.`;
  return `${lead}Left empty, Medha uses ${published.toLocaleString("en-US")} tokens, the published size for this model. Set it lower if your server serves less.`;
}

/** Why a server gave no models, in words that say what to do next. */
export function reasonFor(cause: unknown, url: string) {
  const text = String(cause);
  if (/\b(401|403)\b|unauthori[sz]ed|forbidden|invalid.{0,12}key|api key/i.test(text)) return "That key was not accepted. Check it and try again.";
  if (/connect|refused|timed? ?out|dns|resolve|unreachable|network/i.test(text))
    return `Nothing answered at ${hostOf(url)}. Start the server, or check the address.`;
  return text.replace(/^Error:\s*/, "");
}

export function matching(models: Found[], query: string) {
  const wanted = query.trim().toLowerCase();
  return wanted ? models.filter((model) => model.id.toLowerCase().includes(wanted)) : models;
}
