// The tool modules this connector serves. Each is one family CLI's MCP tools
// compiled to WebAssembly and published with that CLI's release; the build
// (scripts/fetch-modules.mjs) downloads each file named in modules.json and
// checks its SHA-256 before wrangler bundles it, so the imports below are the
// verified files.
//
// Adding a CLI is two entries: its release asset in modules.json, and a line
// here naming the Worker secrets its module reads. Tool names are prefixed by
// their CLI (`coinbase_…`), so modules never collide; the Worker refuses to
// start if two do. Every module is optional: only the ones whose secrets are
// set serve tools.
import coinbase from "../modules/cbase-mcp.wasm";
import alpaca from "../modules/alp-mcp.wasm";

export interface ToolModule {
  /** Short name, used in log lines and crash messages. */
  name: string;
  wasm: WebAssembly.Module;
  /** The Worker secrets and vars the module reads, passed to each call by
   * name. Unset ones are left out; the module answers its own auth error. */
  config: readonly string[];
  /** The secrets that turn the module on. A deployer who sets none of them
   * (empty, or the Deploy form's `none`) gets none of its tools; one who
   * sets some gets the tools, whose calls name what is missing. */
  secrets: readonly string[];
}

export const MODULES: readonly ToolModule[] = [
  {
    name: "coinbase",
    wasm: coinbase,
    config: ["CBASE_API_KEY_NAME", "CBASE_API_PRIVATE_KEY", "CBASE_BASE_URL"],
    secrets: ["CBASE_API_KEY_NAME", "CBASE_API_PRIVATE_KEY"],
  },
  {
    name: "alpaca",
    wasm: alpaca,
    config: ["ALP_API_KEY_ID", "ALP_API_SECRET_KEY", "ALP_ENV", "ALP_BASE_URL"],
    secrets: ["ALP_API_KEY_ID", "ALP_API_SECRET_KEY"],
  },
];
