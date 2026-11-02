// Which module answers a tool, and the tool list Claude sees: the rules
// index.ts applies to every request, kept apart from the Worker so a test
// can hold them without WebAssembly or a computer.
//
// An in-Worker module that is on owns its tools' names: a home server cannot
// list or answer a name such a module serves. A home module serves the
// names under its prefix that no module that is on serves, including those
// of an in-Worker module the owner left off.

/** Where a `tools/call` goes. */
export type Route<H, P> =
  | { kind: "local"; module: string }
  | { kind: "home"; module: H }
  | { kind: "problem"; problem: P }
  | { kind: "unknown" };

export interface Modules<H, P> {
  /** Each in-Worker tool's module, by tool name. */
  owners: ReadonlyMap<string, string>;
  /** The in-Worker modules that are on, by name. */
  on: ReadonlySet<string>;
  /** The home modules that are set up. */
  home: readonly H[];
  /** The home modules left half set up. */
  problems: readonly P[];
}

export function route<H extends { prefix: string }, P extends { prefix: string }>(
  name: string,
  modules: Modules<H, P>,
): Route<H, P> {
  const owner = modules.owners.get(name);
  if (owner !== undefined && modules.on.has(owner)) return { kind: "local", module: owner };
  const home = modules.home.find((m) => name.startsWith(m.prefix));
  if (home) return { kind: "home", module: home };
  const problem = modules.problems.find((p) => name.startsWith(p.prefix));
  if (problem) return { kind: "problem", problem };
  return { kind: "unknown" };
}

/** The in-Worker tools that are on, then each home list's tools whose names
 * are not taken yet. */
export function mergeTools<T extends { name: string }>(local: readonly T[], home: readonly (readonly T[])[]): T[] {
  const tools = [...local];
  const taken = new Set(local.map((t) => t.name));
  for (const list of home) {
    for (const tool of list) {
      if (taken.has(tool.name)) continue;
      taken.add(tool.name);
      tools.push(tool);
    }
  }
  return tools;
}
