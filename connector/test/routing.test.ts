// Which module answers a tool, and the list Claude sees (src/routing.ts),
// with a home server that lists a name an in-Worker module serves.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { mergeTools, route } from "../src/routing.ts";
import { CONNECTOR_VERSION } from "../src/version.ts";

const m1 = { name: "M1", prefix: "m1_" };
const shadow = { name: "COINBASE", prefix: "coinbase_" };
const half = { name: "ROBINHOOD", prefix: "robinhood_", message: "not set up" };
const owners = new Map([
  ["coinbase_accounts", "coinbase"],
  ["alpaca_portfolio", "alpaca"],
]);

test("a tool goes to its in-Worker module while that module is on", () => {
  const where = { owners, on: new Set(["coinbase"]), home: [m1, shadow], problems: [half] };
  assert.deepEqual(route("coinbase_accounts", where), { kind: "local", module: "coinbase" });
  // A home module under the same prefix does not take it.
  assert.deepEqual(route("m1_portfolio", where), { kind: "home", module: m1 });
  assert.deepEqual(route("robinhood_portfolio", where), { kind: "problem", problem: half });
  // Off, with no home module under its prefix: unknown, not served.
  assert.deepEqual(route("alpaca_portfolio", where), { kind: "unknown" });
  assert.deepEqual(route("nope", where), { kind: "unknown" });
});

test("with its in-Worker module off, a name under a home module's prefix is the home module's", () => {
  const where = { owners, on: new Set<string>(), home: [shadow], problems: [] };
  assert.deepEqual(route("coinbase_accounts", where), { kind: "home", module: shadow });
});

test("a home list cannot shadow an in-Worker tool that is on, and lists a name once", () => {
  const local = [{ name: "coinbase_accounts", from: "worker" }];
  const home = [
    [
      { name: "coinbase_accounts", from: "home" },
      { name: "coinbase_balances", from: "home" },
    ],
    [{ name: "coinbase_balances", from: "second" }],
  ];
  assert.deepEqual(mergeTools(local, home), [
    { name: "coinbase_accounts", from: "worker" },
    { name: "coinbase_balances", from: "home" },
  ]);
  // The same home list with the in-Worker module off: its tool is listed.
  assert.deepEqual(
    mergeTools([], home).map((t) => `${t.name}:${t.from}`),
    ["coinbase_accounts:home", "coinbase_balances:home"],
  );
});

test("the version MCP clients see is package.json's", () => {
  const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string };
  assert.equal(CONNECTOR_VERSION, pkg.version);
});
