// Download each tool module named in modules.json into modules/ and check it
// against the pinned SHA-256. Runs as the Worker's build step (wrangler.jsonc
// `build.command`), so `wrangler dev`, `wrangler deploy` and the Deploy to
// Cloudflare button all bundle exactly the pinned bytes. A file already in
// modules/ with the right hash is kept; anything else is replaced or, if the
// download does not match, the build fails.
//
// For developing against an unreleased module, CONNECTOR_MODULE_<NAME>=<path>
// (e.g. CONNECTOR_MODULE_COINBASE=../../coinbase-cli/target/.../cbase.wasm)
// copies a local build instead, unverified, and says so.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(root, "modules");
const { modules } = JSON.parse(readFileSync(join(root, "modules.json"), "utf8"));
mkdirSync(outDir, { recursive: true });

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

function place(file, bytes) {
  const target = join(outDir, file);
  const tmp = `${target}.tmp`;
  writeFileSync(tmp, bytes);
  renameSync(tmp, target);
}

let failed = false;
for (const m of modules) {
  const target = join(outDir, m.file);
  const override = process.env[`CONNECTOR_MODULE_${m.name.toUpperCase()}`];
  if (override) {
    const bytes = readFileSync(resolve(override));
    place(m.file, bytes);
    console.warn(`${m.name}: using the local build ${override} (sha256 ${sha256(bytes)}), NOT the pinned release`);
    continue;
  }
  if (existsSync(target) && sha256(readFileSync(target)) === m.sha256) {
    console.log(`${m.name}: ${m.file} matches the pin`);
    continue;
  }
  const response = await fetch(m.url);
  if (!response.ok) {
    console.error(`${m.name}: downloading ${m.url} failed: HTTP ${response.status}`);
    failed = true;
    continue;
  }
  const bytes = new Uint8Array(await response.arrayBuffer());
  const got = sha256(bytes);
  if (got !== m.sha256) {
    console.error(`${m.name}: ${m.url} has sha256 ${got}, but modules.json pins ${m.sha256}; refusing to use it`);
    failed = true;
    continue;
  }
  place(m.file, bytes);
  console.log(`${m.name}: fetched ${m.url} (${bytes.length} bytes), sha256 verified`);
}
if (failed) process.exit(1);
