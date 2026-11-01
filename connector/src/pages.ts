// The two HTML pages the Worker renders. Everything from the client
// (its name, the redirect host, scopes) is escaped: dynamic registration lets
// anyone choose those strings.
import type { ConsentDescription } from "@cloudflare/workers-oauth-provider";

const esc = (v: string) => v.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

const HEADERS = {
  "Content-Type": "text/html; charset=utf-8",
  "X-Frame-Options": "DENY",
  "Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
  "Cache-Control": "no-store",
};

const STYLE = `body{font:16px/1.5 system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 1rem}
input[type=password]{width:100%;padding:.5rem;font:inherit}button{padding:.5rem 1rem;font:inherit}
.warn{background:#fff4e5;border-left:4px solid #f59e0b;padding:.5rem .75rem}`;

export function authorizePage(d: ConsentDescription, handle: string): string {
  const who = d.clientDomain
    ? `Published by <strong>${esc(d.clientDomain)}</strong>.`
    : "This app registered itself; its name is not verified.";
  const loopback = d.redirectIsLoopback
    ? `<p class="warn">Access goes to an app on <em>this computer</em>. Continue only if you just started connecting from it.</p>`
    : "";
  return `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Connect ${esc(d.clientName)}</title><style>${STYLE}</style>
<h1>Connect ${esc(d.clientName)}?</h1>
<p>${who} Access will be sent to <strong>${esc(d.redirectHost)}</strong>, read-only.</p>
${loopback}
<form method="post">
<input type="hidden" name="handle" value="${esc(handle)}">
<p><label>Owner passphrase<br><input type="password" name="passphrase" autocomplete="current-password" required autofocus></label></p>
<p><button name="decision" value="approve">Connect</button> <button name="decision" value="deny" formnovalidate>Cancel</button></p>
</form>`;
}

export function messagePage(status: number, message: string): Response {
  const body = `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Connector</title><style>${STYLE}</style><p>${esc(message)}</p>`;
  return new Response(body, { status, headers: HEADERS });
}
