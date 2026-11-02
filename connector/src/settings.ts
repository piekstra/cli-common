// What counts as a setting the owner made.
//
// The Deploy to Cloudflare button asks for every entry in .dev.vars.example
// and may not accept an empty answer, so each module's secrets are offered
// pre-filled with `none`: left that way, the module stays off.

/** A secret or var that is set: a non-empty string other than `none`. */
export function isSet(value: unknown): value is string {
  if (typeof value !== "string") return false;
  const v = value.trim();
  return v !== "" && v.toLowerCase() !== "none";
}

/** Whether the owner set an in-Worker module up: any of its secrets is set.
 * One with some missing is on, and its calls name what is missing. */
export function moduleEnabled(module: { secrets: readonly string[] }, env: Record<string, unknown>): boolean {
  return module.secrets.some((key) => isSet(env[key]));
}
