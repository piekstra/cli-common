// A personal MCP connector: one Worker, deployed by its owner into their own
// Cloudflare account, serving piekstra-family CLI tools to claude.ai.
//
//   /mcp                 Streamable HTTP MCP, behind OAuth (agents SDK handler)
//   /authorize           the owner-only approval page (passphrase)
//   /oauth/token, /oauth/register, /.well-known/*   workers-oauth-provider
//
// The tools themselves are Rust compiled to WebAssembly (pkg/, built by
// scripts/build-wasm.sh). This file never interprets them: it passes the
// module's `tools()` JSON to `tools/list` and `tools/call` arguments to
// `call()`, and returns the module's CallToolResult unchanged.

import { OAuthProvider, AuthorizationError } from "@cloudflare/workers-oauth-provider";
import type { OAuthHelpers } from "@cloudflare/workers-oauth-provider";
import { createMcpHandler } from "agents/mcp/server";
import { Server } from "@modelcontextprotocol/server";
import type { CallToolResult, Tool } from "@modelcontextprotocol/server";
import cbaseWasm from "../pkg/cbase_wasm_bg.wasm";
import { initSync, tools as cbaseTools, call as cbaseCall } from "../pkg/cbase_wasm.js";
import { authorizePage, messagePage } from "./pages";
import { checkPassphrase, lockedOut, recordFailure, redirectAllowed } from "./owner";

export interface Env {
  OAUTH_KV: KVNamespace;
  OAUTH_PROVIDER: OAuthHelpers;
  /** Worker secret: what the owner types on /authorize. 20+ characters. */
  OWNER_PASSPHRASE: string;
  /** Worker secret: the CDP API key JSON file, verbatim. */
  CBASE_CDP_KEY?: string;
  /** Plain var; points at the localhost mock in development. */
  COINBASE_BASE_URL?: string;
  /** Plain var: extra allowed redirect URI prefixes, comma-separated. */
  EXTRA_REDIRECT_PREFIXES?: string;
}

initSync({ module: cbaseWasm });
// The module's JSON is the contract; the casts only name it for TypeScript.
const TOOLS = JSON.parse(cbaseTools()) as Tool[];
const SERVER_INFO = { name: "pk-cli-connector", version: "0.0.0-spike" };

/** A fresh low-level MCP server per request, wired straight to the WASM module. */
function mcpServer(env: Env): Server {
  const server = new Server(SERVER_INFO, { capabilities: { tools: {} } });
  server.setRequestHandler("tools/list", async () => ({ tools: TOOLS }));
  server.setRequestHandler("tools/call", async (request) => {
    const config = JSON.stringify({ credential: env.CBASE_CDP_KEY, base_url: env.COINBASE_BASE_URL });
    const result = await cbaseCall(request.params.name, JSON.stringify(request.params.arguments ?? {}), config);
    return JSON.parse(result) as CallToolResult;
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
    if (url.pathname === "/") {
      return messagePage(200, `This is a personal MCP connector. Add ${url.origin}/mcp to Claude as a custom connector.`);
    }
    return new Response("Not found", { status: 404 });
  },
};

async function showConsent(request: Request, env: Env): Promise<Response> {
  if ((env.OWNER_PASSPHRASE ?? "").length < 20) {
    return messagePage(503, "OWNER_PASSPHRASE is not set or is shorter than 20 characters. Set it with `wrangler secret put OWNER_PASSPHRASE`.");
  }
  const oauth = env.OAUTH_PROVIDER;
  const authRequest = await oauth.parseAuthRequest(request);
  const details = await oauth.describeConsent(authRequest);
  if (!redirectAllowed(details.redirectUri, env)) {
    return messagePage(400, `Refusing to send access to ${details.redirectHost}: only Claude's callback and local MCP clients are allowed.`);
  }
  const consent = await oauth.beginConsent(authRequest);
  consent.headers.set("Content-Type", "text/html; charset=utf-8");
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
    userId: "owner",
    metadata: {},
    scope: approved.request.scope,
    props: { sub: "owner" },
  });
  approved.headers.set("Location", redirectTo);
  return new Response(null, { status: 302, headers: approved.headers });
}

// The protected-resource URL must be absolute, but a deploy-your-own Worker
// does not know its hostname until it runs. Build the provider for the origin
// the request arrived on; Cloudflare only routes this Worker's own hostnames
// here, so the origin is one the owner chose.
const providers = new Map<string, OAuthProvider<Env>>();
function providerFor(origin: string): OAuthProvider<Env> {
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
      // Registration stays open (claude.ai registers itself), but only for
      // clients whose callback is Claude's or a loopback one; /authorize
      // re-checks the exact redirect URI of every request.
      clientRegistrationCallback: ({ clientMetadata }) => {
        const uris = Array.isArray(clientMetadata.redirect_uris) ? clientMetadata.redirect_uris.map(String) : [];
        if (uris.length === 0 || !uris.every((u) => redirectAllowed(u, undefined))) {
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
    return providerFor(new URL(request.url).origin).fetch(request, env, ctx);
  },
};
