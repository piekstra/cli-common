// The owner gate's pure parts: where a code may go, the passphrase check and
// the lockout. The OAuth flow around them is scripts/smoke.sh's.
import { test } from "node:test";
import assert from "node:assert/strict";

import { checkPassphrase, lockedOut, recordFailure, redirectAllowed } from "../src/owner.ts";

test("codes go to Claude's callback, loopback clients and the owner's extras only", () => {
  for (const ok of [
    "https://claude.ai/api/mcp/auth_callback",
    "https://claude.com/api/mcp/auth_callback?x=1",
    "http://127.0.0.1:6274/oauth/callback",
    "http://localhost:33418/callback",
    "http://[::1]:9/cb",
  ]) {
    assert.ok(redirectAllowed(ok, undefined), ok);
  }
  for (const bad of [
    "https://claude.ai/api/mcp/auth_callback/../evil",
    "https://claude.ai.evil.example/api/mcp/auth_callback",
    "https://claude.ai/api/mcp/auth_callbackx",
    "https://localhost/callback",
    "http://127.0.0.2/cb",
    "javascript:alert(1)",
    "not a url",
  ]) {
    assert.ok(!redirectAllowed(bad, undefined), bad);
  }
  const env = { EXTRA_REDIRECT_URIS: " https://app.example/oauth/cb , https://other.example/cb" };
  assert.ok(redirectAllowed("https://app.example/oauth/cb?state=1", env));
  assert.ok(!redirectAllowed("https://app.example/oauth/cb2", env));
  assert.ok(!redirectAllowed("https://app.example/oauth/cb", undefined));
});

test("the passphrase must match exactly, and a short secret opens nothing", async () => {
  const secret = "correct horse battery staple";
  assert.equal(await checkPassphrase(secret, secret), true);
  assert.equal(await checkPassphrase(`${secret} `, secret), false);
  assert.equal(await checkPassphrase("", secret), false);
  assert.equal(await checkPassphrase("short", "short"), false);
  assert.equal(await checkPassphrase("", undefined), false);
});

test("ten failures in an hour lock the approval page", async () => {
  const store = new Map<string, string>();
  const env = {
    OAUTH_KV: {
      get: async (key: string) => store.get(key) ?? null,
      put: async (key: string, value: string) => void store.set(key, value),
    },
  } as unknown as Parameters<typeof lockedOut>[0];
  for (let i = 0; i < 9; i++) await recordFailure(env);
  assert.equal(await lockedOut(env), false);
  await recordFailure(env);
  assert.equal(await lockedOut(env), true);
});
