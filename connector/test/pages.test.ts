// The pages' Content-Security-Policy. Chrome applies `form-action` to where a
// form's answer redirects as well, and approving the consent form answers
// with a redirect to Claude's callback: with `form-action 'self'` the
// connection never finishes. curl in scripts/smoke.sh cannot see that, so it
// is pinned here.
import { test } from "node:test";
import assert from "node:assert/strict";

import { PAGE_CSP } from "../src/pages.ts";

test("the pages' policy lets the consent form's redirect reach the client", () => {
  assert.ok(!/form-action/.test(PAGE_CSP), PAGE_CSP);
  for (const directive of ["default-src 'none'", "frame-ancestors 'none'"]) {
    assert.ok(PAGE_CSP.includes(directive), directive);
  }
  assert.ok(!/script-src/.test(PAGE_CSP), "no script may run on the pages");
});
