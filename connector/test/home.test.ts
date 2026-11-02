// Home modules (src/home.ts) against a fake computer: which settings turn a
// module on, the tool list and its cache, and every way a call can fail. The
// Worker around them, with a real `<bin> mcp --http`, is scripts/e2e.sh's.
import { beforeEach, test } from "node:test";
import assert from "node:assert/strict";

import {
  FRESH_MS,
  HomeCache,
  OFFLINE_RETRY_MS,
  callHome,
  endpointFor,
  homeInstructions,
  homeModules,
  knownListing,
  listing,
  problemResult,
} from "../src/home.ts";
import type { HomeEnv, HomeModule } from "../src/home.ts";
import { isSet, moduleEnabled } from "../src/settings.ts";

/** A dummy bearer token, as long as `mcp --new-token` makes. */
const TOKEN = "t".repeat(64);

interface Seen {
  url: string;
  headers: Record<string, string>;
  body: { method: string; params?: unknown };
}

/** A computer that answers like `m1f mcp --http`, or `fail` decides. */
function computer(fail?: (seen: Seen) => Response | Error | undefined) {
  const seen: Seen[] = [];
  const fetch = async (url: string, init: RequestInit): Promise<Response> => {
    const s: Seen = {
      url,
      headers: Object.fromEntries(new Headers(init.headers).entries()),
      body: JSON.parse(String(init.body)),
    };
    seen.push(s);
    const failure = fail?.(s);
    if (failure instanceof Error) throw failure;
    if (failure) return failure;
    const result = {
      initialize: { instructions: "M1 instructions.", serverInfo: { name: "m1f", version: "0.5.0" } },
      "tools/list": {
        tools: [
          { name: "m1_portfolio", inputSchema: { type: "object" } },
          { name: "coinbase_accounts", inputSchema: { type: "object" } },
        ],
      },
      ping: {},
      "tools/call": { content: [{ type: "text", text: "{}" }], structuredContent: {}, isError: false },
    }[s.body.method];
    return Response.json({ jsonrpc: "2.0", id: 1, result });
  };
  return { seen, fetcher: { fetch } };
}

function kv() {
  const map = new Map<string, string>();
  let puts = 0;
  return {
    map,
    puts: () => puts,
    store: {
      get: async (key: string) => map.get(key) ?? null,
      put: async (key: string, value: string) => {
        puts++;
        map.set(key, value);
      },
    } as unknown as HomeEnv["OAUTH_KV"],
  };
}

function only(env: HomeEnv): HomeModule {
  const { modules, problems } = homeModules(env);
  assert.deepEqual(problems, []);
  assert.equal(modules.length, 1);
  return modules[0];
}

/** A fresh isolate's view, per test. */
let cache = new HomeCache();
beforeEach(() => {
  cache = new HomeCache();
});

test("a setting is set unless it is empty or the Deploy form's `none`", () => {
  for (const off of [undefined, "", "  ", "none", "NONE", " None "]) assert.equal(isSet(off), false, String(off));
  for (const on of ["x", "nonexistent"]) assert.equal(isSet(on), true, on);
  const coinbase = { secrets: ["CBASE_API_KEY_NAME", "CBASE_API_PRIVATE_KEY"] };
  assert.equal(moduleEnabled(coinbase, { CBASE_API_KEY_NAME: "none", CBASE_API_PRIVATE_KEY: "" }), false);
  assert.equal(moduleEnabled(coinbase, {}), false);
  // Half set is on: its calls name what is missing.
  assert.equal(moduleEnabled(coinbase, { CBASE_API_KEY_NAME: "organizations/x" }), true);
});

test("a home module is on when its token is set, through a VPC binding or a URL", () => {
  const { fetcher } = computer();
  const vpc = only({ HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher });
  assert.equal(vpc.via, "vpc");
  assert.equal(vpc.prefix, "m1_");
  assert.equal(vpc.endpoint, "http://localhost/mcp");
  assert.deepEqual(vpc.headers, { Authorization: `Bearer ${TOKEN}` });

  const url = only({
    HOME_ROBINHOOD_TOKEN: TOKEN,
    HOME_ROBINHOOD_URL: "https://rhd.example.com",
    HOME_ROBINHOOD_ACCESS_CLIENT_ID: "id.access",
    HOME_ROBINHOOD_ACCESS_CLIENT_SECRET: "secret",
  });
  assert.equal(url.via, "url");
  assert.equal(url.prefix, "robinhood_");
  assert.equal(url.endpoint, "https://rhd.example.com/mcp");
  assert.equal(url.headers["CF-Access-Client-Id"], "id.access");
  assert.equal(url.headers["CF-Access-Client-Secret"], "secret");

  // No token, or `none`: no module, and nothing to report.
  for (const env of [{ HOME_M1: fetcher }, { HOME_M1_TOKEN: "none", HOME_M1: fetcher }, { HOME_M1_URL: "https://x" }]) {
    assert.deepEqual(homeModules(env), { modules: [], problems: [] });
  }
});

test("a half-set home module is reported, never served", () => {
  const cases: [HomeEnv, RegExp][] = [
    [{ HOME_M1_TOKEN: "short" }, /shorter than 32/],
    [{ HOME_M1_TOKEN: TOKEN }, /neither the HOME_M1 VPC service binding nor HOME_M1_URL/],
    [{ HOME_M1_TOKEN: TOKEN, HOME_M1_URL: "http://m1.example.com/mcp" }, /must be an https/],
    [{ HOME_M1_TOKEN: TOKEN, HOME_M1_URL: "not a url" }, /must be an https/],
    [{ HOME_M1_TOKEN: TOKEN, HOME_M1_URL: "https://m1.example.com", HOME_M1_ACCESS_CLIENT_ID: "id" }, /or neither/],
  ];
  for (const [env, why] of cases) {
    const { modules, problems } = homeModules(env);
    assert.equal(modules.length, 0);
    assert.equal(problems.length, 1);
    assert.match(problems[0].message, why);
    const result = problemResult(problems[0]);
    assert.equal(result.isError, true);
    assert.match(String(result.content[0].type === "text" && result.content[0].text), /^other: .*not set up/);
    assert.match(homeInstructions([], problems, cache), /m1_… tools are not set up yet/);
  }
});

test("a URL is https, or http to loopback for local development", () => {
  assert.equal(endpointFor("https://m1.example.com/"), "https://m1.example.com/mcp");
  assert.equal(endpointFor("https://m1.example.com/other"), "https://m1.example.com/other");
  assert.equal(endpointFor("http://127.0.0.1:8790"), "http://127.0.0.1:8790/mcp");
  for (const bad of ["http://m1.example.com", "ftp://x", "https://user:pw@m1.example.com", ""]) {
    assert.equal(endpointFor(bad), null, bad);
  }
});

test("the list keeps the module's own tools, is cached, and is written to KV only when it changes", async () => {
  const { seen, fetcher } = computer();
  const store = kv();
  const env: HomeEnv = { HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher, OAUTH_KV: store.store };
  const module = only(env);
  const first = await listing(env, module, cache, 1_000_000);
  assert.deepEqual(first?.tools.map((t) => t.name), ["m1_portfolio"], "coinbase_accounts is not m1's to serve");
  assert.equal(first?.bin, "m1f");
  assert.equal(first?.instructions, "M1 instructions.");
  assert.equal(store.puts(), 1);
  for (const s of seen) {
    assert.equal(s.url, "http://localhost/mcp");
    assert.equal(s.headers.authorization, `Bearer ${TOKEN}`);
    assert.equal(s.headers.origin, undefined, "never an Origin: the server refuses one");
  }

  const asked = seen.length;
  await listing(env, module, cache, 1_000_000 + FRESH_MS - 1);
  assert.equal(seen.length, asked, "fresh: not asked again");
  await listing(env, module, cache, 1_000_000 + FRESH_MS);
  assert.equal(seen.length, asked + 2, "stale: asked again (initialize and tools/list)");
  assert.equal(store.puts(), 1, "the same list is not written again");

  // A new isolate finds the list in KV without asking the computer.
  const known = await knownListing(env, module, new HomeCache());
  assert.deepEqual(known?.tools.map((t) => t.name), ["m1_portfolio"]);
  assert.equal(seen.length, asked + 2);
});

test("an offline computer keeps the last list, and is asked again only after a pause", async () => {
  let offline = false;
  const { seen, fetcher } = computer(() => (offline ? new Error("destination_unavailable") : undefined));
  const env: HomeEnv = { HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher };
  const module = only(env);
  await listing(env, module, cache, 0);
  offline = true;
  const t = FRESH_MS;
  const kept = await listing(env, module, cache, t);
  assert.deepEqual(kept?.tools.map((x) => x.name), ["m1_portfolio"]);
  const asked = seen.length;
  await listing(env, module, cache, t + OFFLINE_RETRY_MS - 1);
  assert.equal(seen.length, asked, "not asked again inside the pause");
  await listing(env, module, cache, t + OFFLINE_RETRY_MS);
  assert.ok(seen.length > asked, "asked again after it");
});

test("a computer never seen lists nothing, and the instructions say why", async () => {
  const { fetcher } = computer(() => new Error("connection_refused"));
  const env: HomeEnv = { HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher };
  const module = only(env);
  assert.equal(await listing(env, module, cache), undefined);
  assert.match(
    homeInstructions([module], [], cache),
    /m1_… tools are not listed because the user's computer has not answered/,
  );
});

test("a computer that answers but refuses the Worker lists nothing, and the instructions give the remedy", async () => {
  const cases: [number, RegExp][] = [
    [401, /m1_… tools are not listed\. auth: .*HOME_M1_TOKEN.*set them again/],
    [403, /m1_… tools are not listed\. other: .*Host Header must be localhost/],
  ];
  for (const [status, why] of cases) {
    const own = new HomeCache();
    const { fetcher } = computer(() => new Response("", { status }));
    const env: HomeEnv = { HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher };
    const module = only(env);
    assert.equal(await listing(env, module, own), undefined);
    const said = homeInstructions([module], [], own);
    assert.match(said, why, String(status));
    assert.doesNotMatch(said, /has not answered/, "an awake computer is not called asleep");
  }
});

test("a list without a tools array is a failure, not an empty list", async () => {
  const { fetcher } = computer((s) =>
    s.body.method === "tools/list" ? Response.json({ jsonrpc: "2.0", id: 1, result: { tools: "m1_portfolio" } }) : undefined,
  );
  const env: HomeEnv = { HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher };
  const module = only(env);
  assert.equal(await listing(env, module, cache), undefined);
  assert.match(homeInstructions([module], [], cache), /upstream: .*without a list of tools/);
});

test("a call is forwarded as it came, after a ping", async () => {
  const { seen, fetcher } = computer();
  const module = only({ HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher });
  const answer = await callHome(module, { name: "m1_portfolio", arguments: { limit: 5 } }, cache);
  assert.ok("result" in answer);
  assert.equal(answer.result.isError, false);
  assert.deepEqual(
    seen.map((s) => s.body.method),
    ["ping", "tools/call"],
  );
  assert.deepEqual(seen[1].body.params, { name: "m1_portfolio", arguments: { limit: 5 } });
});

test("every failure is a tool error that says what to do", async () => {
  const text = async (fail: (s: Seen) => Response | Error | undefined) => {
    const { fetcher } = computer(fail);
    const module = only({ HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher });
    const answer = await callHome(module, { name: "m1_portfolio" }, new HomeCache());
    assert.ok("result" in answer);
    assert.equal(answer.result.isError, true);
    const block = answer.result.content[0];
    return block.type === "text" ? block.text : "";
  };
  // The VPC binding throws when the tunnel has no connector.
  assert.match(await text(() => new Error("destination_unavailable")), /^offline: .*asleep or offline/);
  // A tunnel route answers 530 (error 1033), or 502 to 504.
  for (const status of [502, 503, 504, 530]) {
    assert.match(await text(() => new Response("", { status })), /^offline: /, String(status));
  }
  // A ping that does not come back in time is an offline computer too.
  const timeout = Object.assign(new Error("timed out"), { name: "TimeoutError" });
  assert.match(await text(() => timeout), /^offline: /);
  assert.match(await text(() => new Response("", { status: 401 })), /^auth: .*HOME_M1_TOKEN/);
  assert.match(await text(() => new Response("", { status: 403 })), /^other: .*Host Header/);
  // The ping passes; the call itself fails.
  assert.match(
    await text((s) => (s.body.method === "tools/call" ? new Response("", { status: 500 }) : undefined)),
    /^upstream: .*HTTP 500/,
  );
  assert.match(
    await text((s) => (s.body.method === "tools/call" ? timeout : undefined)),
    /^upstream: .*did not answer in time/,
  );
  assert.match(
    await text((s) => (s.body.method === "tools/call" ? new Response("<html>", { status: 200 }) : undefined)),
    /^upstream: .*other than MCP JSON/,
  );
  // JSON that is not a JSON-RPC answer: from another service on the port,
  // it fails the ping; an SSE `data: null`, the call.
  assert.match(await text(() => Response.json({ status: "ok" })), /^upstream: .*other than MCP JSON/);
  assert.match(
    await text((s) =>
      s.body.method === "tools/call"
        ? new Response("event: message\ndata: null\n\n", { headers: { "content-type": "text/event-stream" } })
        : undefined,
    ),
    /^upstream: .*other than MCP JSON/,
  );
  assert.match(
    await text((s) => (s.body.method === "tools/call" ? Response.json({ jsonrpc: "2.0", id: 1, result: {} }) : undefined)),
    /^upstream: .*without a tool result/,
  );
  // The body breaks off after the headers: the tunnel dropped, or the time
  // ran out while it was read.
  const broken = (error: Error) =>
    new Response(new ReadableStream({ start: (c) => c.error(error) }), { status: 200 });
  assert.match(
    await text((s) => (s.body.method === "tools/call" ? broken(new TypeError("Network connection lost.")) : undefined)),
    /^upstream: .*cut off \(Network connection lost\.\)/,
  );
  assert.match(
    await text((s) => (s.body.method === "tools/call" ? broken(timeout) : undefined)),
    /^upstream: .*did not answer in time/,
  );
  assert.match(await text(() => broken(timeout)), /^offline: /, "a ping whose answer does not finish in time");
});

test("the computer's own JSON-RPC error comes back as one", async () => {
  const { fetcher } = computer((s) =>
    s.body.method === "tools/call"
      ? Response.json({ jsonrpc: "2.0", id: 1, error: { code: -32602, message: "unknown tool `m1_nope`" } })
      : undefined,
  );
  const module = only({ HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher });
  const answer = await callHome(module, { name: "m1_nope" }, cache);
  assert.deepEqual(answer, { rpcError: { code: -32602, message: "unknown tool `m1_nope`" } });
});

test("an SSE answer is read as well as a JSON one", async () => {
  const { fetcher } = computer((s) =>
    s.body.method === "tools/call"
      ? new Response(
          `event: message\ndata: ${JSON.stringify({ jsonrpc: "2.0", id: 1, result: { content: [], isError: false } })}\n\n`,
          { headers: { "content-type": "text/event-stream" } },
        )
      : undefined,
  );
  const module = only({ HOME_M1_TOKEN: TOKEN, HOME_M1: fetcher });
  const answer = await callHome(module, { name: "m1_portfolio" }, cache);
  assert.deepEqual(answer, { result: { content: [], isError: false } });
});
