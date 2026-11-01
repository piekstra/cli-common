// Run the cbase WebAssembly module outside the Worker: once against the mock
// (proves the request path end to end), then many times against a stubbed
// fetch to measure the module's own CPU cost per tool call.
//
//   node scripts/bench-node.mjs live [tool] [argsJson]
//   node scripts/bench-node.mjs bench [iterations] [--rust-sign]
//
// --rust-sign signs ES256 with the pure-Rust p256 code instead of WebCrypto.
// --pages=N makes the stub answer with N one-wallet pages, so one call signs
// N JWTs: the slope over N is the per-page cost, the intercept the per-call
// cost (parsing the key, importing it into WebCrypto).
//
// Node runs the same V8 as workerd, so the stubbed-fetch numbers are the
// module's CPU per call (JWT signing, URL building, JSON parse and shaping)
// without network or runtime fetch overhead. They are a proxy, not a
// measurement on Cloudflare's hardware.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const t0 = performance.now();
const mod = await import(join(root, "pkg", "cbase_wasm.js"));
mod.initSync({ module: readFileSync(join(root, "pkg", "cbase_wasm_bg.wasm")) });
const initMs = performance.now() - t0;

const devVars = readFileSync(join(root, ".dev.vars"), "utf8");
const credential = devVars.match(/^CBASE_CDP_KEY='(.*)'$/m)[1];
const webcrypto = !process.argv.includes("--rust-sign");
const config = (baseUrl) => JSON.stringify({ credential, base_url: baseUrl, webcrypto });

const [mode = "live", ...rest] = process.argv.slice(2).filter((a) => !a.startsWith("--"));

if (mode === "live") {
  const [tool = "coinbase_accounts_list", args = "{}"] = rest;
  const out = JSON.parse(await mod.call(tool, args, config("http://127.0.0.1:8788")));
  console.log(JSON.stringify(out, null, 2));
  process.exit(out.isError ? 1 : 0);
}

if (mode === "bench") {
  const n = Number(rest[0] ?? 2000);
  const fixture = JSON.parse(readFileSync(join(root, "mock", "fixtures", "accounts.json"), "utf8"));
  const pages = Number((process.argv.find((a) => a.startsWith("--pages=")) ?? "--pages=1").slice(8));
  const pageBody = (i) =>
    JSON.stringify({
      pagination: { next_uri: i + 1 < pages ? `/v2/accounts?starting_after=p${i}` : null },
      data: pages === 1 ? fixture : [{ ...fixture[0], id: `p${i}` }],
    });
  // A synthetic Response has an empty `url`, which reqwest's wasm backend
  // refuses to parse; a real fetch response always carries one.
  globalThis.fetch = async (req) => {
    const after = new URL(req.url).searchParams.get("starting_after");
    const i = after ? Number(after.slice(1)) + 1 : 0;
    const resp = new Response(pageBody(i), { status: 200, headers: { "content-type": "application/json" } });
    Object.defineProperty(resp, "url", { value: req.url });
    return resp;
  };

  const cpu = () => {
    const u = process.cpuUsage();
    return (u.user + u.system) / 1000; // ms
  };
  const once = async () => {
    const c0 = cpu();
    const w0 = performance.now();
    const out = await mod.call("coinbase_accounts_list", "{}", config("https://api.coinbase.com"));
    const w = performance.now() - w0;
    if (JSON.parse(out).isError) throw new Error(out);
    return { cpu: cpu() - c0, wall: w };
  };

  const toolsStart = performance.now();
  mod.tools();
  const toolsMs = performance.now() - toolsStart;
  const first = await once();
  const wall = [];
  const c0 = cpu();
  for (let i = 0; i < n; i++) wall.push((await once()).wall);
  const cpuPerCall = (cpu() - c0) / n;
  wall.sort((a, b) => a - b);
  const pct = (p) => wall[Math.min(wall.length - 1, Math.floor((p / 100) * wall.length))].toFixed(3);
  const alg = JSON.parse(credential).privateKey.includes("BEGIN") ? "ES256" : "EdDSA";
  console.log(
    JSON.stringify(
      {
        alg,
        signer: alg === "ES256" && webcrypto ? "webcrypto" : "rust",
        iterations: n,
        pages_per_call: pages,
        module_init_ms: +initMs.toFixed(2),
        tools_ms: +toolsMs.toFixed(3),
        first_call: { cpu_ms: +first.cpu.toFixed(3), wall_ms: +first.wall.toFixed(3) },
        steady_cpu_ms_per_call: +cpuPerCall.toFixed(3),
        steady_wall_ms: { p50: +pct(50), p95: +pct(95), p99: +pct(99), max: +wall.at(-1).toFixed(3) },
      },
      null,
      2,
    ),
  );
}
