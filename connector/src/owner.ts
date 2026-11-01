// Owner-only approval: who may finish /authorize, and where the code may go.
import type { Env } from "./index";

/**
 * Claude's web, desktop and mobile apps register exactly this callback
 * (https://claude.com/docs/connectors/building/authentication). The
 * claude.com twin is included in case the apps move domains; Anthropic owns
 * both. Loopback callbacks cover Claude Code and the MCP Inspector.
 */
const DEFAULT_PREFIXES = ["https://claude.ai/api/mcp/auth_callback", "https://claude.com/api/mcp/auth_callback"];

export function redirectAllowed(uri: string, env: Env | undefined): boolean {
  let u: URL;
  try {
    u = new URL(uri);
  } catch {
    return false;
  }
  if (u.protocol === "http:" && (u.hostname === "localhost" || u.hostname === "127.0.0.1" || u.hostname === "[::1]")) {
    return true;
  }
  const extra = (env?.EXTRA_REDIRECT_PREFIXES ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  const target = `${u.origin}${u.pathname}`;
  return [...DEFAULT_PREFIXES, ...extra].some((p) => target === p);
}

/** Constant-time comparison of the typed passphrase with the secret. */
export async function checkPassphrase(typed: string, secret: string | undefined): Promise<boolean> {
  if (!secret || secret.length < 20) return false;
  const enc = new TextEncoder();
  // Hash both so the comparison is over equal-length values.
  const [a, b] = await Promise.all([
    crypto.subtle.digest("SHA-256", enc.encode(typed)),
    crypto.subtle.digest("SHA-256", enc.encode(secret)),
  ]);
  return crypto.subtle.timingSafeEqual(a, b);
}

// A coarse lockout: ten wrong passphrases in a clock hour closes /authorize
// until the hour ends. KV is eventually consistent, so a burst can get a few
// more tries than ten; the passphrase's length is the real defence, and this
// only blunts a guesser working through a weak one. One KV write per failure
// keeps it well inside the free plan's 1,000 writes a day.
const MAX_FAILURES_PER_HOUR = 10;
const failureKey = () => `owner-auth-failures:${Math.floor(Date.now() / 3_600_000)}`;

export async function lockedOut(env: Env): Promise<boolean> {
  return Number((await env.OAUTH_KV.get(failureKey())) ?? 0) >= MAX_FAILURES_PER_HOUR;
}

export async function recordFailure(env: Env): Promise<void> {
  const key = failureKey();
  const n = Number((await env.OAUTH_KV.get(key)) ?? 0) + 1;
  await env.OAUTH_KV.put(key, String(n), { expirationTtl: 3600 });
}
