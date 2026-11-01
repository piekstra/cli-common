// Generate throwaway CDP-shaped API keys for the localhost mock, and write
// .dev.vars for `wrangler dev`. Nothing here is a real Coinbase key: the mock
// is the only party that ever sees them, and both files are gitignored.
//
//   node scripts/gen-dummy-key.mjs            # ES256 (EC P-256, SEC1 PEM)
//   node scripts/gen-dummy-key.mjs --ed25519  # EdDSA (base64 seed||public)
import { generateKeyPairSync, randomBytes, randomUUID } from "node:crypto";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const keysDir = join(root, "mock", ".keys");
mkdirSync(keysDir, { recursive: true });

const ed = process.argv.includes("--ed25519");
const keyName = `organizations/${randomUUID()}/apiKeys/${randomUUID()}`;
let keyFile;
let publicPem;
if (ed) {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const seed = privateKey.export({ type: "pkcs8", format: "der" }).subarray(-32);
  const pub = publicKey.export({ type: "spki", format: "der" }).subarray(-32);
  keyFile = { id: keyName.split("/").pop(), privateKey: Buffer.concat([seed, pub]).toString("base64") };
  publicPem = publicKey.export({ type: "spki", format: "pem" });
} else {
  const { privateKey, publicKey } = generateKeyPairSync("ec", {
    namedCurve: "prime256v1",
    privateKeyEncoding: { type: "sec1", format: "pem" },
    publicKeyEncoding: { type: "spki", format: "pem" },
  });
  keyFile = { name: keyName, privateKey };
  publicPem = publicKey;
}

writeFileSync(join(keysDir, "cdp_api_key.json"), JSON.stringify(keyFile, null, 2) + "\n", { mode: 0o600 });
writeFileSync(join(keysDir, "public.pem"), publicPem);

// A long random passphrase, the shape the deploy instructions ask for.
const passphrase = randomBytes(24).toString("base64url");
const devVars = [
  `CBASE_CDP_KEY='${JSON.stringify(keyFile)}'`,
  `OWNER_PASSPHRASE=${passphrase}`,
  `COINBASE_BASE_URL=http://127.0.0.1:8788`,
  "",
].join("\n");
writeFileSync(join(root, ".dev.vars"), devVars, { mode: 0o600 });
console.log(`wrote mock/.keys/ and .dev.vars (${ed ? "EdDSA" : "ES256"} dummy key)`);
