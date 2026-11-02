// A personal MCP connector: one Worker, deployed by its owner into their own
// Cloudflare account, serving piekstra-family CLI tools to Claude.
//
//   /mcp                 Streamable HTTP MCP, behind OAuth (agents SDK handler)
//   /authorize           the owner-only approval page (passphrase)
//   /revoke              disconnect every client (passphrase)
//   /oauth/token, /oauth/register, /.well-known/*   workers-oauth-provider
//
// The tools come from two kinds of module, and every module is optional:
//
// - In-Worker modules (src/modules.ts): an official-API CLI's own Rust,
//   compiled to WebAssembly and published with its release. One is on when
//   any of its secrets is set (not empty, not `none`). src/pk-host.ts runs
//   them; it is a copy of cli-common's reference host.
// - Home modules (src/home.ts): a portal CLI running `<bin> mcp --http` on
//   the owner's own computer, reached through the owner's Cloudflare Tunnel.
//   One is on when its HOME_<NAME>_TOKEN secret is set.
//
// This file never interprets a tool: `tools/list` is what the modules
// describe, a `tools/call` goes to the module that owns the tool, and its
// CallToolResult comes back unchanged.

import { OAuthProvider, AuthorizationError, OAuthError } from "@cloudflare/workers-oauth-provider";
import type { OAuthHelpers } from "@cloudflare/workers-oauth-provider";
import { createMcpHandler } from "agents/mcp/server";
import { ProtocolError, Server } from "@modelcontextprotocol/server";
import type { CallToolResult, Tool } from "@modelcontextprotocol/server";
import { WasmToolset } from "./pk-host.ts";
import { MODULES } from "./modules.ts";
import type { ToolModule } from "./modules.ts";
import { isSet, moduleEnabled } from "./settings.ts";
import { callHome, homeInstructions, homeModules, knownListing, listing, problemResult } from "./home.ts";
import { PAGE_CSP, authorizePage, messagePage, revokePage } from "./pages.ts";
import {
  MIN_PASSPHRASE,
  checkPassphrase,
  grantIsCurrent,
  lockedOut,
  passphraseTag,
  recordFailure,
  redirectAllowed,
} from "./owner.ts";

export interface Env {
  OAUTH_KV: KVNamespace;
  OAUTH_PROVIDER: OAuthHelpers;
  /** Worker secret: what the owner types on /authorize. 20+ characters. */
  OWNER_PASSPHRASE: string;
  /** Optional var: more OAuth callbacks to allow, beyond Claude's and
   * loopback ones (src/owner.ts). */
  EXTRA_REDIRECT_URIS?: string;
  /** The modules' own secrets and vars (src/modules.ts), e.g.
   * CBASE_API_KEY_NAME, and the home modules' (src/home.ts): HOME_<NAME>_TOKEN
   * with a HOME_<NAME> VPC service binding or HOME_<NAME>_URL. */
  [moduleConfig: string]: unknown;
}

const CONNECTOR_VERSION = "0.2.0";
/** The one user: whoever knows the passphrase. */
const OWNER = "owner";
const INVALID_PARAMS = -32602;

interface Loaded {
  module: ToolModule;
  toolset: WasmToolset;
}

interface Described {
  loaded: Loaded;
  tools: Tool[];
  instructions: string;
}

interface Catalog {
  modules: Described[];
  owner: Map<string, Loaded>;
}

// One instance of each module per isolate, made on first use. Every module
// is described, set up or not, so two that define the same tool fail at once.
// A module that traps is replaced by pk-host on its next call.
let catalog: Catalog | null = null;
function tools(): Catalog {
  if (catalog) return catalog;
  const owner = new Map<string, Loaded>();
  const modules: Described[] = [];
  for (const module of MODULES) {
    const loaded = { module, toolset: new WasmToolset(module.wasm, { label: module.name }) };
    const description = loaded.toolset.describe();
    const list: Tool[] = [];
    for (const tool of description.tools) {
      const other = owner.get(tool.name);
      if (other) throw new Error(`the ${module.name} and ${other.module.name} modules both define ${tool.name}`);
      owner.set(tool.name, loaded);
      // The module's JSON is the contract; the cast only names it.
      list.push(tool as unknown as Tool);
    }
    modules.push({ loaded, tools: list, instructions: description.instructions });
  }
  catalog = { modules, owner };
  return catalog;
}

/** The secrets and vars a module reads, by name; unset ones left out. */
function configFor(module: ToolModule, env: Env): Record<string, string> {
  const config: Record<string, string> = {};
  for (const key of module.config) {
    const value = env[key];
    if (isSet(value)) config[key] = value;
  }
  return config;
}

const NOTHING_SET_UP =
  "No tools are set up in this connector yet. Its owner turns a module on by setting its secrets in the " +
  "Worker's settings (Variables and Secrets); the connector's README lists them.";

/** A fresh low-level MCP server per request, wired straight to the modules
 * the owner set up. */
async function mcpServer(env: Env): Promise<Server> {
  const { modules, owner } = tools();
  const enabled = modules.filter((m) => moduleEnabled(m.loaded.module, env));
  const on = new Set(enabled.map((m) => m.loaded.module.name));
  const local = new Set(enabled.flatMap((m) => m.tools.map((t) => t.name)));
  const home = homeModules(env);

  // Each home CLI's own instructions, from the last list it gave; asking
  // the computer here would cost every request a round trip.
  const known = await Promise.all(home.modules.map((m) => knownListing(env, m)));
  const instructions = [
    ...enabled.map((m) => m.instructions),
    ...known.flatMap((l) => (l?.instructions ? [l.instructions] : [])),
    homeInstructions(home.modules, home.problems),
  ].filter((text) => text !== "");
  if (enabled.length === 0 && home.modules.length === 0) instructions.unshift(NOTHING_SET_UP);

  const server = new Server(
    { name: "pk-cli-connector", title: "Personal CLI connector", version: CONNECTOR_VERSION },
    { capabilities: { tools: {} }, instructions: instructions.join("\n\n") },
  );
  server.setRequestHandler("tools/list", async () => {
    const list = enabled.flatMap((m) => m.tools);
    const listings = await Promise.all(home.modules.map((m) => listing(env, m)));
    const seen = new Set(local);
    for (const l of listings) {
      for (const tool of l?.tools ?? []) {
        // An in-Worker tool keeps its name; a home server cannot shadow it.
        if (seen.has(tool.name)) continue;
        seen.add(tool.name);
        list.push(tool);
      }
    }
    return { tools: list };
  });
  server.setRequestHandler("tools/call", async (request) => {
    const name = request.params.name;
    const loaded = owner.get(name);
    if (loaded && on.has(loaded.module.name)) {
      const answer = await loaded.toolset.call(request.params, configFor(loaded.module, env));
      if ("error" in answer) throw new ProtocolError(answer.error.code, answer.error.message);
      return answer.result as CallToolResult;
    }
    const module = local.has(name) ? undefined : home.modules.find((m) => name.startsWith(m.prefix));
    if (module) {
      const answer = await callHome(module, request.params);
      if ("rpcError" in answer) throw new ProtocolError(answer.rpcError.code, answer.rpcError.message);
      return answer.result;
    }
    const problem = home.problems.find((p) => name.startsWith(p.prefix));
    if (problem) return problemResult(problem);
    throw new ProtocolError(INVALID_PARAMS, `Unknown tool: ${name}`);
  });
  return server;
}

const PASSPHRASE_CHANGED = "The connector's passphrase has changed since this client connected. Connect again.";

const mcpApi = {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    // The provider has checked the token; this checks the grant behind it
    // still stands under today's passphrase.
    if (!(await grantIsCurrent((ctx as ExecutionContext & { props?: unknown }).props, env.OWNER_PASSPHRASE))) {
      const metadata = `${new URL(request.url).origin}/.well-known/oauth-protected-resource/mcp`;
      return Response.json(
        { error: "invalid_token", error_description: PASSPHRASE_CHANGED },
        {
          status: 401,
          // The provider's own challenge for a dead token, so a client
          // starts the authorization flow again.
          headers: {
            "Cache-Control": "no-store",
            "WWW-Authenticate": `Bearer realm="OAuth", resource_metadata="${metadata}", error="invalid_token", scope="mcp:read"`,
          },
        },
      );
    }
    return createMcpHandler(() => mcpServer(env), { route: "/mcp" })(request, env, ctx);
  },
};

/** Everything that is not /mcp or an OAuth endpoint the library owns. */
const site = {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/authorize") {
      try {
        return request.method === "POST" ? await approve(request, env) : await showConsent(request, env);
      } catch (error) {
        if (error instanceof AuthorizationError && error.redirectTo) return Response.redirect(error.redirectTo, 302);
        if (error instanceof AuthorizationError) return messagePage(400, error.description ?? "Invalid request.");
        throw error;
      }
    }
    if (url.pathname === "/revoke") {
      return request.method === "POST" ? await revokeAll(request, env) : revokePage();
    }
    if (url.pathname === "/") {
      return messagePage(200, `This is a personal MCP connector. Its owner adds ${url.origin}/mcp to Claude as a custom connector.`);
    }
    return new Response("Not found", { status: 404 });
  },
};

async function showConsent(request: Request, env: Env): Promise<Response> {
  if ((env.OWNER_PASSPHRASE ?? "").length < MIN_PASSPHRASE) {
    return messagePage(
      503,
      "OWNER_PASSPHRASE is not set or is shorter than 20 characters. Set it in the Worker's settings (Variables and Secrets) or with `npx wrangler secret put OWNER_PASSPHRASE`.",
    );
  }
  const oauth = env.OAUTH_PROVIDER;
  const authRequest = await oauth.parseAuthRequest(request);
  const details = await oauth.describeConsent(authRequest);
  if (!redirectAllowed(details.redirectUri, env)) {
    return messagePage(400, `Refusing to send access to ${details.redirectHost}: only Claude's callback and local MCP clients are allowed.`);
  }
  const consent = await oauth.beginConsent(authRequest);
  consent.headers.set("Content-Type", "text/html; charset=utf-8");
  // The library's headers already forbid framing and caching; this adds the
  // rest of the pages' policy.
  consent.headers.set("Content-Security-Policy", PAGE_CSP);
  return new Response(authorizePage(details, consent.handle), { headers: consent.headers });
}

async function approve(request: Request, env: Env): Promise<Response> {
  const oauth = env.OAUTH_PROVIDER;
  const form = await request.formData();
  const handle = String(form.get("handle") ?? "");
  if (form.get("decision") !== "approve") {
    const denied = await oauth.denyConsent(request, handle);
    return new Response(null, { status: 302, headers: denied.headers });
  }
  if (await lockedOut(env)) {
    return messagePage(429, "Too many wrong passphrases in the last hour. Try again later.");
  }
  if (!(await checkPassphrase(String(form.get("passphrase") ?? ""), env.OWNER_PASSPHRASE))) {
    await recordFailure(env);
    return messagePage(403, "Wrong passphrase. Go back to Claude and start the connection again.");
  }
  const approved = await oauth.approveConsent(request, handle, { scope: ["mcp:read"] });
  if (!redirectAllowed(approved.request.redirectUri, env)) {
    return messagePage(400, "Redirect URI not allowed.");
  }
  const { redirectTo } = await oauth.completeAuthorization({
    request: approved.request,
    userId: OWNER,
    metadata: {},
    scope: approved.request.scope,
    // The passphrase's tag: changing the passphrase ends this grant.
    props: { sub: OWNER, passphrase: await passphraseTag(env.OWNER_PASSPHRASE) },
  });
  approved.headers.set("Location", redirectTo);
  return new Response(null, { status: 302, headers: approved.headers });
}

/** Revoke every grant, which ends every connection (Claude's included) at
 * once: their access and refresh tokens stop working. Behind the passphrase
 * and its lockout, like /authorize. */
async function revokeAll(request: Request, env: Env): Promise<Response> {
  const form = await request.formData();
  if (await lockedOut(env)) {
    return messagePage(429, "Too many wrong passphrases in the last hour. Try again later.");
  }
  if (!(await checkPassphrase(String(form.get("passphrase") ?? ""), env.OWNER_PASSPHRASE))) {
    await recordFailure(env);
    return messagePage(403, "Wrong passphrase. Nothing was revoked.");
  }
  const oauth = env.OAUTH_PROVIDER;
  let revoked = 0;
  // Collect first: revoking while paging would move the cursor's ground.
  const ids: string[] = [];
  let cursor: string | undefined;
  do {
    const page = await oauth.listUserGrants(OWNER, { cursor });
    ids.push(...page.items.map((g) => g.id));
    cursor = page.cursor;
  } while (cursor);
  for (const id of ids) {
    await oauth.revokeGrant(id, OWNER);
    revoked++;
  }
  return messagePage(
    200,
    `Revoked ${revoked} connection${revoked === 1 ? "" : "s"}. Each client must connect again, with the passphrase.`,
  );
}

// The protected-resource URL must be absolute, but a deploy-your-own Worker
// does not know its hostname until it runs. Build the provider for the origin
// the request arrived on; Cloudflare only routes this Worker's own hostnames
// here, so the origin is one the owner chose. The env it closes over is the
// isolate's: changing a var or secret redeploys, which starts new isolates.
const providers = new Map<string, OAuthProvider<Env>>();
function providerFor(origin: string, env: Env): OAuthProvider<Env> {
  let p = providers.get(origin);
  if (!p) {
    p = new OAuthProvider<Env>({
      apiRoute: "/mcp",
      apiHandler: mcpApi,
      defaultHandler: site,
      authorizeEndpoint: "/authorize",
      tokenEndpoint: "/oauth/token",
      clientRegistrationEndpoint: "/oauth/register",
      clientIdMetadataDocumentEnabled: true,
      scopesSupported: ["mcp:read"],
      requiredScopes: ["mcp:read"],
      resourceMetadata: { resource: `${origin}/mcp`, resource_name: "Personal CLI connector" },
      accessTokenTTL: 3600,
      // A connection stays signed in while it is used and lapses after 30
      // days unused; /revoke ends them all at once.
      refreshTokenTTL: 30 * 86_400,
      refreshTokenIdleTTL: 30 * 86_400,
      // A grant approved under an earlier passphrase cannot be refreshed:
      // invalid_grant also revokes it.
      tokenExchangeCallback: async ({ grantType, props, env: current }) => {
        if (grantType === "refresh_token" && !(await grantIsCurrent(props, current.OWNER_PASSPHRASE))) {
          throw new OAuthError("invalid_grant", { description: PASSPHRASE_CHANGED });
        }
      },
      // Registration stays open (claude.ai registers itself), but only for
      // clients whose callbacks are all allowed; /authorize re-checks the
      // redirect URI of every request.
      clientRegistrationCallback: ({ clientMetadata }) => {
        const uris = Array.isArray(clientMetadata.redirect_uris) ? clientMetadata.redirect_uris.map(String) : [];
        if (uris.length === 0 || !uris.every((u) => redirectAllowed(u, env))) {
          return { code: "invalid_redirect_uri", description: "Only Claude's callback and loopback clients may register." };
        }
      },
    });
    providers.set(origin, p);
  }
  return p;
}

export default {
  fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    return providerFor(new URL(request.url).origin, env).fetch(request, env, ctx);
  },
};
