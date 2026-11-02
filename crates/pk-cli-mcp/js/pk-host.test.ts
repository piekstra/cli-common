// The reference host against a real module: pk-cli-mcp-wasm-demo built for
// wasm32-unknown-unknown. Run by `make wasm-host-check`, which builds the
// module and passes its path in PK_WASM_DEMO.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { ABI_VERSION, ModuleError, WasmToolset, checkModule } from "./pk-host.ts";
import type { CallAnswer } from "./pk-host.ts";

const path = process.env.PK_WASM_DEMO;
if (!path) throw new Error("set PK_WASM_DEMO to the demo module's path (make wasm-host-check does)");
const demoModule = new WebAssembly.Module(readFileSync(path));

interface Seen {
  url: string;
  init: RequestInit;
}

/** A host whose fetch answers from `respond` and records each request. */
function host(respond: (url: string) => Promise<Response> | Response = () => new Response("ok")) {
  const seen: Seen[] = [];
  const logs: Array<[number, string]> = [];
  const toolset = new WasmToolset(demoModule, {
    label: "demo",
    fetch: async (url, init) => {
      seen.push({ url, init });
      return respond(url);
    },
    log: (level, text) => logs.push([level, text]),
  });
  return { toolset, seen, logs };
}

function result(answer: CallAnswer): Record<string, any> {
  assert.ok("result" in answer, JSON.stringify(answer));
  return answer.result as Record<string, any>;
}

const config = { base: "https://demo.test" };

test("a module with a foreign import, or missing an export, is refused", () => {
  // A minimal module importing env.x: magic, version, a type, one import.
  const foreign = new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x02, 0x09, 0x01, 0x03,
    0x65, 0x6e, 0x76, 0x01, 0x78, 0x00, 0x00,
  ]);
  assert.throws(() => checkModule(new WebAssembly.Module(foreign)), /imports env\.x/);
  const empty = new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
  assert.throws(() => new WasmToolset(new WebAssembly.Module(empty)), ModuleError);
  assert.doesNotThrow(() => checkModule(demoModule));
});

test("describe hands over identity, the Worker's wording and the tools", () => {
  const d = host().toolset.describe();
  assert.equal(d.abi, ABI_VERSION);
  assert.equal(d.server.name, "demo");
  assert.deepEqual(
    d.tools.map((t) => t.name),
    ["demo_fetch", "demo_sign", "demo_both", "demo_random", "demo_panic"],
  );
  assert.match(d.instructions, /running in the user's own Cloudflare Worker/);
  assert.doesNotMatch(d.instructions, /auth login/);
});

test("a fetch goes out as the module built it and its answer comes back", async () => {
  const { toolset, seen } = host(() => new Response("hello", { status: 202 }));
  const r = result(await toolset.call({ name: "demo_fetch", arguments: { path: "items" } }, config));
  assert.equal(r.isError, false);
  assert.deepEqual(r.structuredContent, { status: 202, body: "hello" });
  assert.equal(seen.length, 1);
  assert.equal(seen[0].url, "https://demo.test/items?q=a%20b");
  assert.equal(seen[0].init.method, "GET");
  assert.equal(seen[0].init.redirect, "manual");
  assert.deepEqual(seen[0].init.headers, [["X-Demo", "1"]]);
});

test("a failed fetch is an upstream tool error naming the origin only", async () => {
  const { toolset } = host(() => {
    throw new TypeError("fetch failed");
  });
  const r = result(await toolset.call({ name: "demo_fetch", arguments: { path: "secret-id" } }, config));
  assert.equal(r.isError, true);
  assert.match(r.content[0].text, /^upstream: the request to https:\/\/demo\.test failed: TypeError: fetch failed\./);
  assert.doesNotMatch(r.content[0].text, /secret-id/);
});

test("ES256 is signed by WebCrypto and verifies under the public key", async () => {
  const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
  const der = new Uint8Array(await crypto.subtle.exportKey("pkcs8", pair.privateKey));
  const hex = Array.from(der, (b) => b.toString(16).padStart(2, "0")).join("");
  const { toolset } = host();
  for (const msg of ["first", "second"]) {
    const r = result(await toolset.call({ name: "demo_sign", arguments: { message: msg } }, { es256_key_hex: hex }));
    assert.equal(r.isError, false, JSON.stringify(r));
    const sig = Uint8Array.from((r.structuredContent.signature_hex as string).match(/../g)!, (h) => parseInt(h, 16));
    assert.equal(sig.length, 64);
    const ok = await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      pair.publicKey,
      sig,
      new TextEncoder().encode(msg),
    );
    assert.ok(ok, "the signature verifies");
  }
  const bad = result(await toolset.call({ name: "demo_sign", arguments: { message: "x" } }, { es256_key_hex: "00" }));
  assert.equal(bad.isError, true);
  assert.match(bad.content[0].text, /the host could not sign/);
});

test("two operations in one call, and two calls, are in flight together", async () => {
  const gates = new Map<string, (r: Response) => void>();
  const { toolset, seen } = host(
    (url) => new Promise<Response>((resolve) => gates.set(new URL(url).pathname, resolve)),
  );
  const both = toolset.call({ name: "demo_both" }, config);
  const single = toolset.call({ name: "demo_fetch", arguments: { path: "solo" } }, config);
  for (let i = 0; i < 10 && gates.size < 3; i++) await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual([...gates.keys()].sort(), ["/one", "/solo", "/two"]);
  // Answer out of order: the later call and the second fetch first.
  gates.get("/solo")!(new Response("s", { status: 200 }));
  gates.get("/two")!(new Response(null, { status: 204 }));
  assert.deepEqual(result(await single).structuredContent, { status: 200, body: "s" });
  gates.get("/one")!(new Response(null, { status: 201 }));
  assert.deepEqual(result(await both).structuredContent, { one: 201, two: 204 });
  assert.equal(seen.length, 3);
});

test("the clock and the random source are the host's", async () => {
  const { toolset } = host();
  const a = result(await toolset.call({ name: "demo_random" }, null)).structuredContent;
  const b = result(await toolset.call({ name: "demo_random" }, null)).structuredContent;
  assert.match(a.random_hex, /^[0-9a-f]{32}$/);
  assert.notEqual(a.random_hex, b.random_hex);
  assert.ok(Math.abs(a.now_ms - Date.now()) < 60_000);
});

test("a panic is a crash answer with its message, and the next call gets a fresh instance", async () => {
  const { toolset, logs } = host();
  const r = result(await toolset.call({ name: "demo_panic" }, null));
  assert.equal(r.isError, true);
  assert.match(r.content[0].text, /^other: the demo module crashed: .*demo_panic was called/);
  assert.ok(logs.some(([level, text]) => level === 0 && text.includes("demo_panic was called")));
  const next = result(await toolset.call({ name: "demo_random" }, null));
  assert.equal(next.isError, false);
});

test("an unknown tool and bad arguments are told apart", async () => {
  const { toolset } = host();
  const unknown = await toolset.call({ name: "demo_nope" }, null);
  assert.ok("error" in unknown);
  assert.equal(unknown.error.code, -32602);
  const bad = result(await toolset.call({ name: "demo_fetch", arguments: { pth: "x" } }, config));
  assert.equal(bad.isError, true);
  assert.match(bad.content[0].text, /^usage: unknown argument `pth`/);
});
