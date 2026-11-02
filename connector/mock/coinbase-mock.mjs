// A localhost stand-in for api.coinbase.com's v2 reads, strict about auth.
//
// It checks every request the way Coinbase does (as far as Coinbase
// documents it): a CDP JWT in `Authorization: Bearer`, signed by the dummy key
// from scripts/gen-dummy-key.mjs, with `iss: cdp`, `sub == kid`, a nonce in
// the header that is never reused, a lifetime of at most 120 s, and a `uri`
// claim of exactly "GET <host><path>" without the query string. Anything off
// gets the live API's observed answer: HTTP 401, `text/plain`, `Unauthorized`.
//
//   node mock/coinbase-mock.mjs [port]     (default 8788, binds 127.0.0.1)
//
// The fixtures are invented; page size is capped at 2 so a three-wallet list
// exercises the cursor walk.
import { createPublicKey, verify } from "node:crypto";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const port = Number(process.argv[2] ?? 8788);
const publicKey = createPublicKey(readFileSync(join(here, ".keys", "public.pem")));
const accounts = JSON.parse(readFileSync(join(here, "fixtures", "accounts.json"), "utf8"));
const transactions = JSON.parse(readFileSync(join(here, "fixtures", "transactions.json"), "utf8"));
const PAGE_CAP = 2;
const seenNonces = new Set();
let served = 0;

const b64url = (s) => Buffer.from(s, "base64url");

function checkJwt(req, path) {
  const auth = req.headers.authorization ?? "";
  if (!auth.startsWith("Bearer ")) return "no bearer token";
  const parts = auth.slice(7).split(".");
  if (parts.length !== 3) return "not a JWS";
  const [h, c, s] = parts;
  const header = JSON.parse(b64url(h));
  const claims = JSON.parse(b64url(c));
  const input = Buffer.from(`${h}.${c}`);
  const sig = b64url(s);
  let ok;
  if (header.alg === "ES256") {
    ok = verify("sha256", input, { key: publicKey, dsaEncoding: "ieee-p1363" }, sig);
  } else if (header.alg === "EdDSA") {
    ok = verify(null, input, publicKey, sig);
  } else {
    return `unsupported alg ${header.alg}`;
  }
  if (!ok) return "bad signature";
  if (header.typ !== "JWT") return "typ is not JWT";
  if (!header.nonce) return "no nonce in the JOSE header";
  if (seenNonces.has(header.nonce)) return "nonce replayed";
  seenNonces.add(header.nonce);
  if (claims.iss !== "cdp") return "iss is not cdp";
  if (claims.sub !== header.kid) return "sub != kid";
  const now = Math.floor(Date.now() / 1000);
  if (claims.nbf > now + 5) return "not yet valid";
  if (claims.exp <= now) return "expired";
  if (claims.exp - claims.nbf > 120) return "lifetime over 120 s";
  const want = `GET ${req.headers.host}${path}`;
  if (claims.uri !== want) return `uri claim ${JSON.stringify(claims.uri)} != ${JSON.stringify(want)}`;
  if (req.headers["cb-version"] !== "2022-01-06") return "missing CB-VERSION pin";
  return null;
}

function page(rows, url, path) {
  const limit = Math.min(Number(url.searchParams.get("limit") ?? 25), PAGE_CAP);
  const after = url.searchParams.get("starting_after");
  const start = after ? rows.findIndex((r) => r.id === after) + 1 : 0;
  const data = rows.slice(start, start + limit);
  const more = start + limit < rows.length;
  return {
    pagination: {
      ending_before: null,
      starting_after: after,
      limit,
      order: "desc",
      previous_uri: null,
      next_uri: more ? `${path}?starting_after=${data.at(-1).id}` : null,
    },
    data,
  };
}

createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const path = url.pathname;
  const reject = checkJwt(req, path);
  served += 1;
  if (reject) {
    console.log(`#${served} ${req.method} ${path} -> 401 (${reject})`);
    res.writeHead(401, { "content-type": "text/plain" }).end("Unauthorized");
    return;
  }
  let body;
  if (req.method === "GET" && path === "/v2/accounts") {
    body = page(accounts, url, path);
  } else {
    const m = path.match(/^\/v2\/accounts\/([^/]+)\/transactions$/);
    if (req.method === "GET" && m && accounts.some((a) => a.id === m[1])) {
      body = page(m[1] === accounts[0].id ? transactions : [], url, path);
    }
  }
  if (!body) {
    console.log(`#${served} ${req.method} ${path} -> 404`);
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ errors: [{ id: "not_found", message: "Not found" }] }));
    return;
  }
  console.log(`#${served} ${req.method} ${url.pathname}${url.search} -> 200 (${body.data.length} rows)`);
  res.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify(body));
}).listen(port, "127.0.0.1", () => console.log(`coinbase mock on http://127.0.0.1:${port}`));
