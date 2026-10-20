export type ConnectionOptions = { headers?: Record<string, string>; ca_cert_file?: string | null; proxy?: string | null };
export type UnavailableProvider = { id: string; name: string; reason?: string | null };
export type Provider = ConnectionOptions & { id?: string; name: string; url: string; auth: string; protocol?: string; parameters?: string[]; models?: Found[] };
export const providerKey = (provider: Provider) => provider.id ?? (provider.protocol ? `${provider.protocol}:${provider.url}` : provider.url);
export type ModelTestResult = Record<"text" | "streaming" | "tools", { status: "passed" | "failed" | "skipped"; message: string; failure: string | null }>;
export type Protocol = {
  value: string;
  label: string;
  available: boolean;
  auth: string;
  discovery: boolean;
  providers: Provider[];
};
export type Found = {
  id: string;
  context_length: number | null;
  /** What the catalogue knows, when it lists the model: display name, USD per million tokens, tool use. */
  name?: string | null;
  input_per_mtok?: number | null;
  output_per_mtok?: number | null;
  tools?: boolean | null;
};

/** "1.05M" or "400K": a model's context in a table column. */
export function contextShort(tokens: number | null) {
  if (!tokens) return "";
  return tokens >= 1_000_000 ? `${Number((tokens / 1_000_000).toFixed(2))}M` : `${Math.round(tokens / 1000)}K`;
}

/** "$2 / $10" or "$0.10 / $0.50" per million tokens in and out, or "" when the catalogue has no price. */
export function priceLabel(model: Found) {
  const dollars = (value: number) => (Number.isInteger(value) ? `$${value}` : `$${value.toFixed(2)}`);
  return model.input_per_mtok != null && model.output_per_mtok != null ? `${dollars(model.input_per_mtok)} / ${dollars(model.output_per_mtok)}` : "";
}

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
export function profileFor(connection: ConnectionOptions & { protocol: string; url: string; auth: string; provider_id?: string }, model = "", context: number | null = null) {
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
    headers: { ...connection.headers },
    ca_cert_file: connection.ca_cert_file ?? null,
    proxy: connection.proxy ?? null,
    ...(connection.provider_id ? { provider_id: connection.provider_id } : {}),
  };
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
  if (/\b403\b|forbidden/i.test(text)) return text.replace(/^Error:\s*/, "");
  if (/\b401\b|unauthori[sz]ed|invalid.{0,12}key|api key/i.test(text)) return "That key was not accepted. Check it and try again.";
  if (/connect|refused|timed? ?out|dns|resolve|unreachable|network/i.test(text))
    return `Nothing answered at ${hostOf(url)}. Start the server, or check the address.`;
  return text.replace(/^Error:\s*/, "");
}

export function matching(models: Found[], query: string) {
  const wanted = query.trim().toLowerCase();
  return wanted ? models.filter((model) => model.id.toLowerCase().includes(wanted)) : models;
}
