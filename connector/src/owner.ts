// Owner-only approval: who may finish /authorize, and where the code may go.
import type { Env } from "./index.ts";

/**
 * Claude's web, desktop and mobile apps register exactly this callback
 * (https://claude.com/docs/connectors/building/authentication). The
 * claude.com twin is included in case the apps move domains; Anthropic owns
 * both. Loopback callbacks cover Claude Code and the MCP Inspector. The
 * owner may allow more in EXTRA_REDIRECT_URIS.
 */
const CLAUDE_CALLBACKS = ["https://claude.ai/api/mcp/auth_callback", "https://claude.com/api/mcp/auth_callback"];

/** Whether an authorization code may be sent to `uri`: Claude's callback, a
 * loopback client, or one of EXTRA_REDIRECT_URIS (comma-separated, matched
 * on origin and path exactly; the query is ignored). */
export function redirectAllowed(uri: string, env: Pick<Env, "EXTRA_REDIRECT_URIS"> | undefined): boolean {
  let u: URL;
  try {
    u = new URL(uri);
  } catch {
    return false;
  }
  if (u.protocol === "http:" && (u.hostname === "localhost" || u.hostname === "127.0.0.1" || u.hostname === "[::1]")) {
    return true;
  }
  const extra = (env?.EXTRA_REDIRECT_URIS ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  const target = `${u.origin}${u.pathname}`;
  return [...CLAUDE_CALLBACKS, ...extra].some((p) => target === p);
}

/** The shortest OWNER_PASSPHRASE the connector accepts. */
export const MIN_PASSPHRASE = 20;

/** Constant-time comparison of the typed passphrase with the secret. */
export async function checkPassphrase(typed: string, secret: string | undefined): Promise<boolean> {
  if (!secret || secret.length < MIN_PASSPHRASE) return false;
  const enc = new TextEncoder();
  // Hash both so the comparison is over equal-length values, then compare
  // every byte whatever the first difference.
  const [a, b] = await Promise.all([
    crypto.subtle.digest("SHA-256", enc.encode(typed)),
    crypto.subtle.digest("SHA-256", enc.encode(secret)),
  ]);
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x[i] ^ y[i];
  return diff === 0;
}

// A coarse lockout: ten wrong passphrases in a clock hour closes /authorize
// until the hour ends. KV is eventually consistent, so a burst can get a few
// more tries than ten; the passphrase's length is the real defence, and this
// only blunts a guesser working through a weak one. One KV write per failure
// keeps it well inside the free plan's 1,000 writes a day.
const MAX_FAILURES_PER_HOUR = 10;
const failureKey = () => `owner-auth-failures:${Math.floor(Date.now() / 3_600_000)}`;

type Kv = Pick<Env["OAUTH_KV"], "get" | "put">;

export async function lockedOut(env: { OAUTH_KV: Kv }): Promise<boolean> {
  return Number((await env.OAUTH_KV.get(failureKey())) ?? 0) >= MAX_FAILURES_PER_HOUR;
}

export async function recordFailure(env: { OAUTH_KV: Kv }): Promise<void> {
  const key = failureKey();
  const n = Number((await env.OAUTH_KV.get(key)) ?? 0) + 1;
  await env.OAUTH_KV.put(key, String(n), { expirationTtl: 3600 });
}
