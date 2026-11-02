// Write the wrangler configs scripts/e2e.sh runs the home module with:
//
//   wrangler.e2e.jsonc         this Worker, as wrangler.jsonc has it, plus a
//                              HOME_<NAME> service binding standing in for
//                              the VPC service binding an owner adds
//   .e2e/vpc-standin.jsonc     mock/vpc-standin.ts, forwarding to
//                              127.0.0.1:<port>
//
// Both are gitignored. The modules are fetched before, so the copy has no
// build step.
//
//   node scripts/e2e-home-config.mjs <NAME> <port>
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [name, port] = process.argv.slice(2);
if (!/^[A-Z0-9]+$/.test(name ?? "") || !/^[0-9]+$/.test(port ?? "")) {
  console.error("usage: node scripts/e2e-home-config.mjs <NAME> <port>");
  process.exit(2);
}
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
// wrangler.jsonc's comments are whole lines.
const text = readFileSync(join(root, "wrangler.jsonc"), "utf8")
  .split("\n")
  .filter((line) => !line.trim().startsWith("//"))
  .join("\n");
const config = JSON.parse(text);
delete config.build;
config.services = [{ binding: `HOME_${name}`, service: "vpc-standin" }];
writeFileSync(join(root, "wrangler.e2e.jsonc"), `${JSON.stringify(config, null, 2)}\n`);

mkdirSync(join(root, ".e2e"), { recursive: true });
const standin = {
  name: "vpc-standin",
  main: "../mock/vpc-standin.ts",
  compatibility_date: config.compatibility_date,
  vars: { HOME_PORT: port },
};
writeFileSync(join(root, ".e2e", "vpc-standin.jsonc"), `${JSON.stringify(standin, null, 2)}\n`);
console.log(`wrote wrangler.e2e.jsonc (HOME_${name} bound to the stand-in) and .e2e/vpc-standin.jsonc (127.0.0.1:${port})`);
