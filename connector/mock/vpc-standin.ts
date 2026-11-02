// A local stand-in for a Workers VPC service binding, for scripts/e2e.sh.
//
// A real binding (`vpc_services` in wrangler.jsonc) carries the Worker's
// `fetch("http://localhost/mcp")` through the owner's Cloudflare Tunnel to
// the address the VPC service names, 127.0.0.1:<port> on their computer.
// `wrangler dev` cannot run one (it needs the account), so the e2e config
// binds HOME_<NAME> to this Worker instead, which forwards to
// 127.0.0.1:HOME_PORT on this machine, where `<bin> mcp --http` listens.
//
// The one difference: when nothing listens, a real binding's fetch throws,
// and a service binding cannot make its caller's fetch throw, so this
// answers 502, as a tunnel route does. src/home.ts reports both as offline;
// its unit tests cover the throw.

interface StandinEnv {
  HOME_PORT: string;
}

export default {
  async fetch(request: Request, env: StandinEnv): Promise<Response> {
    const url = new URL(request.url);
    const target = `http://127.0.0.1:${env.HOME_PORT}${url.pathname}${url.search}`;
    try {
      return await fetch(target, {
        method: request.method,
        headers: request.headers,
        body: request.body,
        redirect: "manual",
      });
    } catch (error) {
      return new Response(`vpc stand-in: nothing answered on 127.0.0.1:${env.HOME_PORT}: ${error}`, { status: 502 });
    }
  },
};
