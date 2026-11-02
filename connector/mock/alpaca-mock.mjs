// A localhost stand-in for Alpaca's Trading API reads, strict about auth.
//
// Every request must carry the dummy key pair scripts/gen-dummy-key.mjs
// wrote (mock/.keys/alpaca.json) in `APCA-API-KEY-ID` and
// `APCA-API-SECRET-KEY`, as Alpaca requires; anything else gets Alpaca's
// documented 401 body. Only GET is served.
//
//   node mock/alpaca-mock.mjs [port]     (default 8789, binds 127.0.0.1)
//
// The fixtures are alpaca-cli's synthetic ones (doc-derived, dummy values).
// The activity feed adds 105 invented cash deposits dated 2026-07-19 to
// 2026-10-31, so a newest-first walk needs a second page (Alpaca's
// page_size caps at 100, and page_token is the last row's id) to reach the
// fixture rows before them.
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const port = Number(process.argv[2] ?? 8789);
const fixture = (name) => JSON.parse(readFileSync(join(here, "fixtures", `alpaca-${name}.json`), "utf8"));
const keys = JSON.parse(readFileSync(join(here, ".keys", "alpaca.json"), "utf8"));
const account = fixture("account");
const positions = fixture("positions");

const deposits = [];
for (let i = 0; i < 105; i++) {
  const day = new Date(Date.UTC(2026, 9, 31 - i)).toISOString().slice(0, 10);
  deposits.push({
    id: `${day.replaceAll("-", "")}000000000::00000000-0000-4000-8000-${String(500 + i).padStart(12, "0")}`,
    activity_type: "CSD",
    date: day,
    net_amount: "1.00",
    description: "invented deposit",
    status: "executed",
  });
}
const dateOf = (row) => (row.transaction_time ?? row.date ?? "").slice(0, 10);
const activities = [...deposits, ...fixture("activities")].sort((a, b) => dateOf(b).localeCompare(dateOf(a)));
let served = 0;

function send(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
}

createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const path = url.pathname;
  served += 1;
  const id = req.headers["apca-api-key-id"];
  const secret = req.headers["apca-api-secret-key"];
  if (id !== keys.key_id || secret !== keys.secret_key) {
    console.log(`#${served} ${req.method} ${path} -> 401 (${id ? "wrong key" : "no key headers"})`);
    send(res, 401, { code: 40110000, message: "request is not authorized" });
    return;
  }
  if (req.method !== "GET") {
    send(res, 405, { code: 40510000, message: "method not allowed" });
    return;
  }
  if (path === "/v2/account") {
    console.log(`#${served} GET ${path} -> 200`);
    send(res, 200, account);
    return;
  }
  if (path === "/v2/positions") {
    console.log(`#${served} GET ${path} -> 200 (${positions.length} positions)`);
    send(res, 200, positions);
    return;
  }
  if (path === "/v2/account/activities") {
    if (url.searchParams.get("direction") !== "desc") {
      send(res, 422, { code: 42210000, message: "this mock serves direction=desc only" });
      return;
    }
    const size = Math.min(Number(url.searchParams.get("page_size") ?? 100), 100);
    const types = url.searchParams.get("activity_types")?.split(",");
    let rows = types ? activities.filter((r) => types.includes(r.activity_type)) : activities;
    const token = url.searchParams.get("page_token");
    if (token) rows = rows.slice(rows.findIndex((r) => r.id === token) + 1);
    const page = rows.slice(0, size);
    console.log(`#${served} GET ${path}${url.search} -> 200 (${page.length} rows)`);
    send(res, 200, page);
    return;
  }
  console.log(`#${served} GET ${path} -> 404`);
  send(res, 404, { code: 40410000, message: "not found" });
}).listen(port, "127.0.0.1", () => console.log(`alpaca mock on http://127.0.0.1:${port}`));
