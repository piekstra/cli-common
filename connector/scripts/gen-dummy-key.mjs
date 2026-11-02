// Generate a throwaway CDP-shaped API key for the localhost Coinbase mock and
// write .dev.vars for `wrangler dev`. Nothing here is a real Coinbase key: the
// mock is the only party that ever sees it, and both files are gitignored.
//
//   node scripts/gen-dummy-key.mjs            # ES256 (EC P-256, SEC1 PEM)
//   node scripts/gen-dummy-key.mjs --ed25519  # EdDSA (base64 seed‖public)
//
// A .dev.vars this script did not write (one holding real secrets, say) is
// left alone.
import { generateKeyPairSync, randomBytes, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const MARK = "# written by scripts/gen-dummy-key.mjs: dummy values for the localhost mock";
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const devVars = join(root, ".dev.vars");
if (existsSync(devVars) && !readFileSync(devVars, "utf8").startsWith(MARK)) {
  console.error(".dev.vars exists and was not written by this script; move it aside to generate dummy keys");
  process.exit(1);
}
const keysDir = join(root, "mock", ".keys");
mkdirSync(keysDir, { recursive: true });

const ed = process.argv.includes("--ed25519");
let name;
let privateKey;
let publicPem;
if (ed) {
  const pair = generateKeyPairSync("ed25519");
  const seed = pair.privateKey.export({ type: "pkcs8", format: "der" }).subarray(-32);
  const pub = pair.publicKey.export({ type: "spki", format: "der" }).subarray(-32);
  name = randomUUID();
  privateKey = Buffer.concat([seed, pub]).toString("base64");
  publicPem = pair.publicKey.export({ type: "spki", format: "pem" });
} else {
  const pair = generateKeyPairSync("ec", {
    namedCurve: "prime256v1",
    privateKeyEncoding: { type: "sec1", format: "pem" },
    publicKeyEncoding: { type: "spki", format: "pem" },
  });
  name = `organizations/${randomUUID()}/apiKeys/${randomUUID()}`;
  privateKey = pair.privateKey;
  publicPem = pair.publicKey;
}
writeFileSync(join(keysDir, "public.pem"), publicPem);

// The PEM goes in with literal `\n` escapes, the way it reads in the key
// file's JSON: the module repairs that, as it does a one-line paste.
const lines = [
  MARK,
  `OWNER_PASSPHRASE=${randomBytes(24).toString("base64url")}`,
  `CBASE_API_KEY_NAME=${name}`,
  `CBASE_API_PRIVATE_KEY='${privateKey.trim().replaceAll("\n", "\\n")}'`,
  "CBASE_BASE_URL=http://127.0.0.1:8788",
  "",
];
writeFileSync(devVars, lines.join("\n"), { mode: 0o600 });
console.log(`wrote mock/.keys/public.pem and .dev.vars (${ed ? "EdDSA" : "ES256"} dummy key)`);
