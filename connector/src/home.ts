// Home modules: tools served by a family CLI on the owner's own computer
// (`<bin> mcp --http`, on 127.0.0.1 there), which this Worker reaches through
// the owner's Cloudflare Tunnel and forwards calls to.
//
// A portal CLI (m1f, rhd) signs in through a browser and keeps its session
// cookies on the computer, and its provider refuses datacenter addresses, so
// its tools cannot run in this Worker the way an official-API module does.
// The Worker never holds that session: it holds only the bearer token the
// computer's server requires, and passes MCP JSON through.
//
// One module per CLI, named by the owner in the Worker's settings:
//
//   HOME_<NAME>_TOKEN   secret, required: the bearer token `<bin> mcp --http`
//                       requires (`<bin> mcp --new-token` makes it). Setting
//                       it is what turns the module on.
//   HOME_<NAME>         a Workers VPC service binding to the computer's
//                       127.0.0.1:<port>, through the tunnel (preferred: no
//                       public hostname)
//   HOME_<NAME>_URL     or instead, https://<host on the owner's domain>/mcp,
//                       a tunnel route behind Cloudflare Access, with
//   HOME_<NAME>_ACCESS_CLIENT_ID / _ACCESS_CLIENT_SECRET, the Access
//                       service token
//
// <NAME> is letters and digits; the module serves the tools named
// `<name>_…` in lower case (HOME_M1 serves m1_portfolio, HOME_ROBINHOOD
// robinhood_portfolio), and drops any other tool the computer lists, so a
// home server cannot stand in for another module's tools.
//
// When the computer is asleep or offline, the tunnel has nowhere to go: the
// VPC fetch throws, a route answers 530. The last tool list is kept (in
// memory and in KV), so Claude still sees the tools, and a call answers at
// once that the computer is offline instead of waiting out a timeout.

import type { CallToolResult, Tool } from "@modelcontextprotocol/server";
import { isSet } from "./settings.ts";
import { CONNECTOR_VERSION } from "./version.ts";

/** What a home module needs of the Worker's environment. */
export interface HomeEnv {
  OAUTH_KV?: Pick<KVNamespace, "get" | "put">;
  [key: string]: unknown;
}

type FetchLike = (url: string, init: RequestInit) => Promise<Response>;

export interface HomeModule {
  /** `M1`, from HOME_M1_TOKEN. */
  name: string;
  /** `m1_`: the tools this module serves. */
  prefix: string;
  /** How the computer is reached. */
  via: "vpc" | "url";
  /** What is fetched: `http://localhost/mcp` through the VPC binding (its
   * host becomes the Host header; the binding's own host and port are
   * what is dialed), or HOME_<NAME>_URL. */
  endpoint: string;
  fetch: FetchLike;
  /** Authorization and, on the URL path, the Access service token. */
  headers: Record<string, string>;
}

/** A module the owner started to set up but left incomplete. */
export interface HomeProblem {
  name: string;
  prefix: string;
  message: string;
}

/** The MCP protocol version the Worker speaks to the computer's server. */
export const HOME_PROTOCOL = "2025-11-25";
/** A home server's tools are re-read at most this often while it answers. */
export const FRESH_MS = 5 * 60_000;
/** After the computer failed to answer, it is asked again no sooner than this. */
export const OFFLINE_RETRY_MS = 30_000;
/** A list or a liveness check waits this long before calling it offline. */
export const PROBE_TIMEOUT_MS = 4_000;
/** A tool call waits this long; the computer's server gives up at 120 s. */
export const CALL_TIMEOUT_MS = 150_000;
const MIN_TOKEN = 32;
const KV_PREFIX = "home-tools:";

const TOKEN_KEY = /^HOME_([A-Z0-9]+)_TOKEN$/;

function isFetcher(value: unknown): value is { fetch: FetchLike } {
  return typeof value === "object" && value !== null && typeof (value as { fetch?: unknown }).fetch === "function";
}

/** Whether a HOME_<NAME>_URL is usable: https, or http to this machine's
 * loopback for local development (`wrangler dev`). */
export function endpointFor(raw: string): string | null {
  let u: URL;
  try {
    u = new URL(raw.trim());
  } catch {
    return null;
  }
  const loopback = u.hostname === "127.0.0.1" || u.hostname === "localhost";
  if (u.protocol !== "https:" && !(u.protocol === "http:" && loopback)) return null;
  if (u.username || u.password) return null;
  if (u.pathname === "/" || u.pathname === "") u.pathname = "/mcp";
  return u.toString();
}

/** The home modules the environment configures, and the half-configured ones. */
export function homeModules(env: HomeEnv): { modules: HomeModule[]; problems: HomeProblem[] } {
  const modules: HomeModule[] = [];
  const problems: HomeProblem[] = [];
  for (const key of Object.keys(env).sort()) {
    const match = TOKEN_KEY.exec(key);
    if (!match || !isSet(env[key])) continue;
    const name = match[1];
    const prefix = `${name.toLowerCase()}_`;
    const problem = (message: string) => problems.push({ name, prefix, message });
    const token = (env[key] as string).trim();
    if (token.length < MIN_TOKEN) {
      problem(`HOME_${name}_TOKEN is shorter than ${MIN_TOKEN} characters; the computer's server refuses such tokens`);
      continue;
    }
    const headers: Record<string, string> = { Authorization: `Bearer ${token}` };
    const binding = env[`HOME_${name}`];
    if (isFetcher(binding)) {
      modules.push({
        name,
        prefix,
        via: "vpc",
        endpoint: "http://localhost/mcp",
        fetch: (url, init) => binding.fetch(url, init),
        headers,
      });
      continue;
    }
    const url = env[`HOME_${name}_URL`];
    if (!isSet(url)) {
      problem(`HOME_${name}_TOKEN is set, but neither the HOME_${name} VPC service binding nor HOME_${name}_URL is`);
      continue;
    }
    const endpoint = endpointFor(url);
    if (!endpoint) {
      problem(`HOME_${name}_URL must be an https:// URL on your own domain (http only to 127.0.0.1 or localhost)`);
      continue;
    }
    const id = env[`HOME_${name}_ACCESS_CLIENT_ID`];
    const secret = env[`HOME_${name}_ACCESS_CLIENT_SECRET`];
    if (isSet(id) !== isSet(secret)) {
      problem(`set both HOME_${name}_ACCESS_CLIENT_ID and HOME_${name}_ACCESS_CLIENT_SECRET, or neither`);
      continue;
    }
    if (isSet(id) && isSet(secret)) {
      headers["CF-Access-Client-Id"] = id.trim();
      headers["CF-Access-Client-Secret"] = secret.trim();
    }
    modules.push({ name, prefix, via: "url", endpoint, fetch: (u, init) => fetch(u, init), headers });
  }
  return { modules, problems };
}

/** What a home server last said about itself. */
export interface Listing {
  tools: Tool[];
  instructions: string;
  /** Its `serverInfo.name`, the CLI binary (`m1f`), for messages. */
  bin: string;
  at: number;
}

/** Why a request to the computer failed, in the owner's terms. */
export class HomeError extends Error {
  readonly kind: "offline" | "auth" | "refused" | "upstream" | "timeout";
  constructor(kind: HomeError["kind"], message: string) {
    super(message);
    this.kind = kind;
  }
}

/** Why the computer last failed to answer. */
export interface Failure {
  kind: HomeError["kind"];
  message: string;
  /** When it first failed since it last answered. */
  since: number;
}

interface State {
  listing?: Listing;
  /** When the server was last asked for its list, answered or not. */
  checkedAt: number;
  /** Set while it has not answered since it last failed to. */
  failure?: Failure;
}

/** What one isolate has learned about each home module: its last list and
 * its last failure. The Worker keeps one for the isolate's life (asking the
 * computer on every request would cost each a round trip); a test makes its
 * own. */
export class HomeCache {
  readonly #states = new Map<string, State>();

  /** The module's state, created empty on first use. */
  state(name: string): State {
    let s = this.#states.get(name);
    if (!s) {
      s = { checkedAt: 0 };
      this.#states.set(name, s);
    }
    return s;
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isTimeout(error: unknown): boolean {
  const name = error instanceof Error ? error.name : "";
  return name === "TimeoutError" || name === "AbortError";
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** One JSON-RPC request to the computer's server: its `result` (an object),
 * or its own JSON-RPC error as `{ rpcError }`. Anything else, from a lost
 * tunnel to an answer that is not MCP JSON, throws a HomeError. */
async function rpc(
  module: HomeModule,
  method: string,
  params: unknown,
  timeoutMs: number,
): Promise<{ result: Record<string, unknown> } | { rpcError: { code: number; message: string } }> {
  const late = () => new HomeError("timeout", `no answer within ${Math.round(timeoutMs / 1000)} s`);
  let response: Response;
  try {
    response = await module.fetch(module.endpoint, {
      method: "POST",
      headers: {
        ...module.headers,
        "Content-Type": "application/json",
        Accept: "application/json, text/event-stream",
        "MCP-Protocol-Version": HOME_PROTOCOL,
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, ...(params === undefined ? {} : { params }) }),
      signal: AbortSignal.timeout(timeoutMs),
      redirect: "manual",
    });
  } catch (error) {
    if (isTimeout(error)) throw late();
    // Workers VPC throws when the tunnel has no connector (the computer is
    // asleep or offline) or the server refuses the connection.
    throw new HomeError("offline", describe(error));
  }
  if (response.status === 401) {
    throw new HomeError("auth", "the computer's server did not accept the Worker's bearer token");
  }
  if (response.status === 403) {
    throw new HomeError("refused", "the computer's server refused the request's Host or Origin");
  }
  // On the URL path: 530 is Cloudflare's "the tunnel has no connector"
  // (error 1033); 502 to 504, the route could not reach the origin.
  if ([502, 503, 504, 530].includes(response.status)) {
    throw new HomeError("offline", `HTTP ${response.status} from the tunnel`);
  }
  if (response.status !== 200) {
    throw new HomeError("upstream", `HTTP ${response.status} from the computer's server`);
  }
  let text: string;
  try {
    text = await response.text();
  } catch (error) {
    // The timeout covers the body too, and a tunnel can drop mid-answer.
    if (isTimeout(error)) throw late();
    throw new HomeError("upstream", `the answer was cut off (${describe(error)})`);
  }
  // JSON, as `mcp --http` answers; an SSE stream's data lines otherwise.
  const json = text.trimStart().startsWith("{")
    ? text
    : text
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).trim())
        .at(-1);
  const notMcp = () => new HomeError("upstream", "the computer's server answered with something other than MCP JSON");
  let message: unknown;
  try {
    message = JSON.parse(json ?? "");
  } catch {
    throw notMcp();
  }
  if (!isObject(message)) throw notMcp();
  if (message.error !== undefined) {
    const e = message.error;
    if (isObject(e) && typeof e.code === "number" && typeof e.message === "string") {
      return { rpcError: { code: e.code, message: e.message } };
    }
    throw notMcp();
  }
  if (!isObject(message.result)) throw notMcp();
  return { result: message.result };
}

/** A failure as a HomeError. Anything else thrown is a fault in this code:
 * logged as one, reported as upstream. */
function asHomeError(module: HomeModule, error: unknown): HomeError {
  if (error instanceof HomeError) return error;
  console.error(`[home ${module.name}] internal error:`, error);
  return new HomeError("upstream", `the connector could not read the answer (${describe(error)})`);
}

/** Remember that the computer failed to answer, and why. */
function record(s: State, e: HomeError, now: number): void {
  s.failure = { kind: e.kind, message: e.message, since: s.failure?.since ?? now };
}

/** The module's tools, from the computer when it is due a re-read and
 * answers, else from the last list kept. Never throws. */
export async function listing(
  env: HomeEnv,
  module: HomeModule,
  cache: HomeCache,
  now = Date.now(),
): Promise<Listing | undefined> {
  const s = cache.state(module.name);
  if (!s.listing) s.listing = await fromKv(env, module.name);
  const due = now - s.checkedAt >= (s.failure ? OFFLINE_RETRY_MS : FRESH_MS);
  if (!due && s.checkedAt !== 0) return s.listing;
  s.checkedAt = now;
  try {
    const [init, list] = await Promise.all([
      rpc(
        module,
        "initialize",
        {
          protocolVersion: HOME_PROTOCOL,
          capabilities: {},
          clientInfo: { name: "pk-cli-connector", version: CONNECTOR_VERSION },
        },
        PROBE_TIMEOUT_MS,
      ),
      rpc(module, "tools/list", {}, PROBE_TIMEOUT_MS),
    ]);
    if ("rpcError" in init) throw new HomeError("upstream", init.rpcError.message);
    if ("rpcError" in list) throw new HomeError("upstream", list.rpcError.message);
    const listed = list.result.tools;
    if (!Array.isArray(listed)) {
      throw new HomeError("upstream", "the computer's server answered tools/list without a list of tools");
    }
    const all = listed.filter((t): t is Tool => isObject(t) && typeof t.name === "string");
    const tools = all.filter((t) => t.name.startsWith(module.prefix));
    if (tools.length < all.length) {
      console.warn(
        `[home ${module.name}] ignoring ${all.length - tools.length} tool(s) not named ${module.prefix}…: ${all
          .filter((t) => !t.name.startsWith(module.prefix))
          .map((t) => t.name)
          .join(", ")}`,
      );
    }
    const info = init.result as { instructions?: unknown; serverInfo?: { name?: unknown } };
    const fresh: Listing = {
      tools,
      instructions: typeof info.instructions === "string" ? info.instructions : "",
      bin: typeof info.serverInfo?.name === "string" ? info.serverInfo.name : "",
      at: now,
    };
    await toKv(env, module.name, s.listing, fresh);
    s.listing = fresh;
    s.failure = undefined;
  } catch (error) {
    const e = asHomeError(module, error);
    record(s, e, now);
    console.warn(`[home ${module.name}] no tool list (${e.kind}): ${e.message}`);
  }
  return s.listing;
}

/** The last list this isolate or KV holds, without asking the computer:
 * for the instructions, which every request builds. */
export async function knownListing(
  env: HomeEnv,
  module: HomeModule,
  cache: HomeCache,
): Promise<Listing | undefined> {
  const s = cache.state(module.name);
  if (!s.listing) s.listing = await fromKv(env, module.name);
  return s.listing;
}

/** The kept list, as an isolate that has not asked yet finds it. */
async function fromKv(env: HomeEnv, name: string): Promise<Listing | undefined> {
  try {
    const raw = await env.OAUTH_KV?.get(`${KV_PREFIX}${name}`);
    return raw ? (JSON.parse(raw) as Listing) : undefined;
  } catch {
    return undefined;
  }
}

/** Keep a list across isolates, writing only when the tools or instructions
 * changed: the free plan allows 1,000 KV writes a day. */
async function toKv(env: HomeEnv, name: string, before: Listing | undefined, after: Listing): Promise<void> {
  const same =
    before !== undefined &&
    before.bin === after.bin &&
    before.instructions === after.instructions &&
    JSON.stringify(before.tools) === JSON.stringify(after.tools);
  if (same) return;
  try {
    await env.OAUTH_KV?.put(`${KV_PREFIX}${name}`, JSON.stringify(after));
  } catch (error) {
    console.warn(`[home ${name}] could not keep the tool list: ${describe(error)}`);
  }
}

function toolError(text: string): CallToolResult {
  return { content: [{ type: "text", text }], isError: true };
}

/** How the model should name the computer's server: `m1f mcp --http` once a
 * list has said which CLI it is. */
function server(module: HomeModule, bin: string | undefined): string {
  return bin ? `\`${bin} mcp --http\`` : `the server for the ${module.prefix}… tools`;
}

/** A failure as the model is told it: what happened and what to do. */
function explain(module: HomeModule, bin: string | undefined, failure: Pick<Failure, "kind" | "message">): string {
  const named = server(module, bin);
  switch (failure.kind) {
    case "offline":
      return (
        `offline: the user's computer, which runs ${named} for these tools, is not reachable ` +
        `(it is asleep or offline, or the server or its tunnel is not running). Nothing was read. ` +
        `Tell the user to wake the computer, or start the server, and ask again.`
      );
    case "timeout":
      return (
        `upstream: ${named} on the user's computer did not answer in time (${failure.message}). ` +
        `Try again later, or narrow the call.`
      );
    case "auth":
      return (
        `auth: ${failure.message}. The token in the Worker's HOME_${module.name}_TOKEN secret and the one ` +
        `${named} uses must be the same; ask the user to set them again.`
      );
    case "refused":
      return (
        `other: ${failure.message}. On the HOME_${module.name}_URL path, the tunnel route's HTTP Host Header ` +
        `must be localhost; ask the user to check it.`
      );
    default:
      return `upstream: ${named} failed: ${failure.message}.`;
  }
}

/** Forward one `tools/call`. A liveness check comes first, so a sleeping
 * computer is reported in seconds, not after the call's whole timeout. Every
 * failure is a tool error; the server's own JSON-RPC error comes back as
 * `{ rpcError }`. */
export async function callHome(
  module: HomeModule,
  params: { name: string; arguments?: unknown },
  cache: HomeCache,
  now = Date.now(),
): Promise<{ result: CallToolResult } | { rpcError: { code: number; message: string } }> {
  const s = cache.state(module.name);
  try {
    await rpc(module, "ping", undefined, PROBE_TIMEOUT_MS);
  } catch (error) {
    const e = asHomeError(module, error);
    record(s, e, now);
    // No answer to a ping within seconds: as good as offline.
    const shown = e.kind === "timeout" ? { kind: "offline" as const, message: e.message } : e;
    return { result: toolError(explain(module, s.listing?.bin, shown)) };
  }
  s.failure = undefined;
  try {
    const answer = await rpc(
      module,
      "tools/call",
      { name: params.name, arguments: params.arguments ?? {} },
      CALL_TIMEOUT_MS,
    );
    if ("rpcError" in answer) return answer;
    if (!Array.isArray(answer.result.content)) {
      throw new HomeError("upstream", "the computer's server answered tools/call without a tool result");
    }
    return { result: answer.result as CallToolResult };
  } catch (error) {
    // The ping got through, so only a lost tunnel counts against the
    // computer; a slow or malformed answer is this call's alone.
    const e = asHomeError(module, error);
    if (e.kind === "offline") record(s, e, now);
    return { result: toolError(explain(module, s.listing?.bin, e)) };
  }
}

/** The answer for a call to a module the owner left half set up. */
export function problemResult(problem: HomeProblem): CallToolResult {
  return toolError(
    `other: the connector's ${problem.prefix}… tools are not set up: ${problem.message}. ` +
      `Ask the user to finish the setup in the Worker's settings (the connector README's home section).`,
  );
}

/** What the model is told about the home modules, after the modules' own
 * instructions: how they work, and for each module that has failed to give
 * a list and has none kept, why, with the remedy a call would give. */
export function homeInstructions(modules: HomeModule[], problems: HomeProblem[], cache: HomeCache): string {
  const parts: string[] = [];
  if (modules.length > 0) {
    const names = modules.map((m) => `${m.prefix}…`).join(", ");
    parts.push(
      `The ${names} tools run on the user's own computer, reached through the user's Cloudflare Tunnel; ` +
        `their sign-in stays on that computer. When it is asleep or offline those tools answer \`offline:\` ` +
        `at once; the other tools still work.`,
    );
    for (const m of modules) {
      const s = cache.state(m.name);
      if (s.listing || !s.failure) continue;
      if (s.failure.kind === "offline" || s.failure.kind === "timeout") {
        parts.push(
          `The ${m.prefix}… tools are not listed because the user's computer has not answered yet ` +
            `(it is asleep or offline, or the server or its tunnel is not running).`,
        );
      } else {
        parts.push(`The ${m.prefix}… tools are not listed. ${explain(m, undefined, s.failure)}`);
      }
    }
  }
  for (const p of problems) {
    parts.push(`The ${p.prefix}… tools are not set up yet: ${p.message}.`);
  }
  return parts.join(" ");
}
