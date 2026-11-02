// A personal MCP connector: one Worker, deployed by its owner into their own
// Cloudflare account, serving piekstra-family CLI tools to Claude.
//
//   /mcp                 Streamable HTTP MCP, behind OAuth (agents SDK handler)
//   /authorize           the owner-only approval page (passphrase)
//   /revoke              disconnect every client (passphrase)
//   /oauth/token, /oauth/register, /.well-known/*   workers-oauth-provider
//
// The tools are the CLIs' own Rust, compiled to WebAssembly and published
// with each CLI's release (src/modules.ts). This file never interprets them:
// `tools/list` is what the modules describe, a `tools/call` goes to the
// module that owns the tool with the Worker secrets it reads, and the
// module's CallToolResult comes back unchanged. src/pk-host.ts runs the
// modules; it is a copy of cli-common's reference host.

import { OAuthProvider, AuthorizationError } from "@cloudflare/workers-oauth-provider";
import type { OAuthHelpers } from "@cloudflare/workers-oauth-provider";
import { createMcpHandler } from "agents/mcp/server";
import { ProtocolError, Server } from "@modelcontextprotocol/server";
import type { CallToolResult, Tool } from "@modelcontextprotocol/server";
import { WasmToolset } from "./pk-host.ts";
import { MODULES } from "./modules.ts";
import type { ToolModule } from "./modules.ts";
import { PAGE_CSP, authorizePage, messagePage, revokePage } from "./pages.ts";
import { MIN_PASSPHRASE, checkPassphrase, lockedOut, recordFailure, redirectAllowed } from "./owner.ts";

export interface Env {
  OAUTH_KV: KVNamespace;
  OAUTH_PROVIDER: OAuthHelpers;
  /** Worker secret: what the owner types on /authorize. 20+ characters. */
  OWNER_PASSPHRASE: string;
  /** Optional var: more OAuth callbacks to allow, beyond Claude's and
   * loopback ones (src/owner.ts). */
  EXTRA_REDIRECT_URIS?: string;
  /** The modules' own secrets and vars (src/modules.ts), e.g.
   * CBASE_API_KEY_NAME. */
  [moduleConfig: string]: unknown;
}

const CONNECTOR_VERSION = "0.1.0";
/** The one user: whoever knows the passphrase. */
const OWNER = "owner";
const INVALID_PARAMS = -32602;

interface Loaded {
  module: ToolModule;
  toolset: WasmToolset;
}

interface Catalog {
  tools: Tool[];
  owner: Map<string, Loaded>;
  instructions: string;
}

// One instance of each module per isolate, made on first use. A module that
// traps is replaced by pk-host on its next call.
let catalog: Catalog | null = null;
function tools(): Catalog {
  if (catalog) return catalog;
  const owner = new Map<string, Loaded>();
  const list: Tool[] = [];
  const instructions: string[] = [];
  for (const module of MODULES) {
    const loaded = { module, toolset: new WasmToolset(module.wasm, { label: module.name }) };
    const description = loaded.toolset.describe();
    instructions.push(description.instructions);
    for (const tool of description.tools) {
      const other = owner.get(tool.name);
      if (other) throw new Error(`the ${module.name} and ${other.module.name} modules both define ${tool.name}`);
      owner.set(tool.name, loaded);
      // The module's JSON is the contract; the cast only names it.
      list.push(tool as unknown as Tool);
    }
  }
  catalog = { tools: list, owner, instructions: instructions.join("\n\n") };
  return catalog;
}

/** The secrets and vars a module reads, by name; unset ones left out. */
function configFor(module: ToolModule, env: Env): Record<string, string> {
  const config: Record<string, string> = {};
  for (const key of module.config) {
    const value = env[key];
    if (typeof value === "string" && value !== "") config[key] = value;
  }
  return config;
}

/** A fresh low-level MCP server per request, wired straight to the modules. */
function mcpServer(env: Env): Server {
  const { tools: list, owner, instructions } = tools();
  const server = new Server(
    { name: "pk-cli-connector", title: "Personal CLI connector", version: CONNECTOR_VERSION },
    { capabilities: { tools: {} }, instructions },
  );
  server.setRequestHandler("tools/list", async () => ({ tools: list }));
  server.setRequestHandler("tools/call", async (request) => {
    const loaded = owner.get(request.params.name);
    if (!loaded) throw new ProtocolError(INVALID_PARAMS, `Unknown tool: ${request.params.name}`);
    const answer = await loaded.toolset.call(request.params, configFor(loaded.module, env));
    if ("error" in answer) throw new ProtocolError(answer.error.code, answer.error.message);
    return answer.result as CallToolResult;
  });
  return server;
}

const mcpApi = {
  fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
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
    props: { sub: OWNER },
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
