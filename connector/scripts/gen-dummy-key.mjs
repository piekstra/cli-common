// Generate a throwaway CDP-shaped API key for the localhost Coinbase mock, a
// dummy Alpaca key pair for the Alpaca mock, and write .dev.vars for
// `wrangler dev`. Nothing here is a real key: the mocks are the only parties
// that ever see them, and every file written is gitignored.
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

// Alpaca's keys are a plain ID and secret, sent as headers; the mock
// checks both.
const alpaca = { key_id: `PK${randomBytes(9).toString("hex").toUpperCase()}`, secret_key: randomBytes(30).toString("base64url") };
writeFileSync(join(keysDir, "alpaca.json"), JSON.stringify(alpaca), { mode: 0o600 });

// The PEM goes in with literal `\n` escapes, the way it reads in the key
// file's JSON: the module repairs that, as it does a one-line paste.
const lines = [
  MARK,
  `OWNER_PASSPHRASE=${randomBytes(24).toString("base64url")}`,
  `CBASE_API_KEY_NAME=${name}`,
  `CBASE_API_PRIVATE_KEY='${privateKey.trim().replaceAll("\n", "\\n")}'`,
  "CBASE_BASE_URL=http://127.0.0.1:8788",
  `ALP_API_KEY_ID=${alpaca.key_id}`,
  `ALP_API_SECRET_KEY=${alpaca.secret_key}`,
  "ALP_BASE_URL=http://127.0.0.1:8789",
  "",
];
writeFileSync(devVars, lines.join("\n"), { mode: 0o600 });
console.log(`wrote mock/.keys/public.pem, mock/.keys/alpaca.json and .dev.vars (${ed ? "EdDSA" : "ES256"} dummy Coinbase key)`);
