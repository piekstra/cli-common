// The reference host for pk-cli-mcp's WebAssembly ABI, version 1: it runs a
// family CLI's toolset module (one `.wasm`, built with
// `pk_cli_mcp::export_wasm!`) inside a Cloudflare Worker, or in Node for
// tests. The ABI itself is documented in `src/wasm/mod.rs`.
//
// A Worker that runs modules copies this file rather than reimplementing
// the ABI.
//
// Plain TypeScript with erasable syntax only, so Node runs it directly
// (type stripping) and wrangler bundles it unchanged.

export const ABI_VERSION = 1;

const STATUS_FAILED = -1;
const STATUS_UNSUPPORTED = -2;
const IMPORTS = ["now_ms", "random", "fetch", "sign_es256", "log"];
const EXPORTS = [
  "memory",
  "pk_abi_version",
  "pk_alloc",
  "pk_free",
  "pk_out_ptr",
  "pk_describe",
  "pk_call",
  "pk_poll",
  "pk_complete",
  "pk_drop",
];

/** What `pk_describe` returns. */
export interface Description {
  abi: number;
  server: { name: string; title?: string; version: string };
  instructions: string;
  tools: Array<Record<string, unknown> & { name: string }>;
}

/** A `tools/call` answer: the CallToolResult, or a JSON-RPC error. */
export type CallAnswer =
  | { result: Record<string, unknown> }
  | { error: { code: number; message: string } };

export interface HostOptions {
  /** The `fetch` provider requests go through; the global one by default. */
  fetch?: (url: string, init: RequestInit) => Promise<Response>;
  /** Where module log lines go; `console` by default. */
  log?: (level: number, message: string) => void;
  /** The module's name in messages, e.g. "coinbase". */
  label?: string;
}

interface Exports {
  memory: WebAssembly.Memory;
  pk_abi_version(): number;
  pk_alloc(len: number): number;
  pk_free(ptr: number, len: number): void;
  pk_out_ptr(): number;
  pk_describe(): number;
  pk_call(params: number, paramsLen: number, config: number, configLen: number): number;
  pk_poll(task: number): number;
  pk_complete(op: number, status: number, ptr: number, len: number): void;
  pk_drop(task: number): void;
}

/** A module this host cannot run: wrong imports, exports or ABI version. */
export class ModuleError extends Error {}

/** Throws a ModuleError unless `module` imports only `pk_host`'s functions
 * and exports the whole ABI. */
export function checkModule(module: WebAssembly.Module): void {
  for (const imp of WebAssembly.Module.imports(module)) {
    if (imp.module !== "pk_host" || !IMPORTS.includes(imp.name)) {
      throw new ModuleError(`the module imports ${imp.module}.${imp.name}, which this host does not provide`);
    }
  }
  const exported = new Set(WebAssembly.Module.exports(module).map((e) => e.name));
  const missing = EXPORTS.filter((name) => !exported.has(name));
  if (missing.length > 0) {
    throw new ModuleError(`the module does not export ${missing.join(", ")}`);
  }
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** A fetch's answer as the ABI frames it: the length of the headers' JSON
 * (u32, little-endian), that JSON, then the body. */
function frameResponse(headers: Array<[string, string]>, body: Uint8Array): Uint8Array {
  const names = encoder.encode(JSON.stringify(headers));
  const out = new Uint8Array(4 + names.length + body.length);
  new DataView(out.buffer).setUint32(0, names.length, true);
  out.set(names, 4);
  out.set(body, 4 + names.length);
  return out;
}

function message(e: unknown): string {
  return e instanceof Error ? `${e.name}: ${e.message}` : String(e);
}

/** One live instance. A trap kills it; the next call starts a fresh one. */
class Live {
  exports!: Exports;
  dead = false;
  lastPanic: string | undefined;
  /** The pending set of the task being polled: operations an import starts
   * during that poll belong to it. */
  collecting: Set<Promise<void>> | null = null;

  bytes(ptr: number, len: number): Uint8Array {
    return new Uint8Array(this.exports.memory.buffer, ptr, len);
  }

  /** Copy bytes into a buffer the module owns from here on. */
  pass(data: Uint8Array): [number, number] {
    const ptr = this.exports.pk_alloc(data.length);
    this.bytes(ptr, data.length).set(data);
    return [ptr, data.length];
  }

  /** The last answer, as JSON. */
  out(len: number): unknown {
    return JSON.parse(decoder.decode(this.bytes(this.exports.pk_out_ptr(), len)));
  }

  /** Hand an operation's answer back; a no-op once the instance is dead. */
  deliver(op: number, status: number, data: Uint8Array): void {
    if (this.dead) return;
    try {
      const [ptr, len] = this.pass(data);
      this.exports.pk_complete(op, status, ptr, len);
    } catch (e) {
      if (!(e instanceof WebAssembly.RuntimeError)) throw e;
      this.dead = true;
    }
  }

  /** Run an operation; while a poll is running, it belongs to that task. */
  track(work: Promise<void>): void {
    const set = this.collecting;
    if (!set) return;
    const settled: Promise<void> = work.then(() => {
      set.delete(settled);
    });
    set.add(settled);
  }
}

/** A toolset module, ready to describe its tools and run calls. */
export class WasmToolset {
  readonly label: string;
  #module: WebAssembly.Module;
  #fetch: (url: string, init: RequestInit) => Promise<Response>;
  #log: (level: number, message: string) => void;
  #live: Live | null = null;
  #description: Description | null = null;
  #keys = new Map<string, Promise<CryptoKey>>();

  constructor(module: WebAssembly.Module, options: HostOptions = {}) {
    checkModule(module);
    this.#module = module;
    this.label = options.label ?? "toolset";
    this.#fetch = options.fetch ?? ((url, init) => fetch(url, init));
    this.#log =
      options.log ??
      ((level, text) => {
        const line = `[${this.label}] ${text}`;
        if (level === 0) console.error(line);
        else if (level === 1) console.warn(line);
        else console.log(line);
      });
    const version = this.#instance().exports.pk_abi_version();
    if (version !== ABI_VERSION) {
      throw new ModuleError(`the module speaks ABI ${version}; this host speaks ${ABI_VERSION}`);
    }
  }

  /** Identity, instructions and tools. Computed once: they never change. */
  describe(): Description {
    if (!this.#description) {
      const live = this.#instance();
      const answer = live.out(live.exports.pk_describe()) as Description;
      if (answer.abi !== ABI_VERSION) {
        throw new ModuleError(`the module describes ABI ${answer.abi}; this host speaks ${ABI_VERSION}`);
      }
      this.#description = answer;
    }
    return this.#description;
  }

  /** Run one `tools/call`. `params` is the call's params object; `config`
   * the per-call JSON the module's factory reads (its credentials). */
  async call(params: { name: string; arguments?: unknown }, config: unknown): Promise<CallAnswer> {
    const live = this.#instance();
    const ex = live.exports;
    let task: number;
    try {
      const [p, pl] = live.pass(encoder.encode(JSON.stringify(params)));
      const [c, cl] = live.pass(encoder.encode(JSON.stringify(config ?? null)));
      task = ex.pk_call(p, pl, c, cl);
    } catch (e) {
      return this.#crashed(live, e);
    }
    const pending = new Set<Promise<void>>();
    let finished = false;
    try {
      for (;;) {
        if (live.dead) return this.#crashed(live);
        let len: number;
        live.collecting = pending;
        try {
          len = ex.pk_poll(task);
        } catch (e) {
          return this.#crashed(live, e);
        } finally {
          live.collecting = null;
        }
        if (len >= 0) {
          finished = true;
          return live.out(len) as CallAnswer;
        }
        if (pending.size === 0) {
          return { error: { code: -32603, message: `the ${this.label} module is waiting on nothing (a bug in the module)` } };
        }
        await Promise.race(pending);
      }
    } finally {
      if (!finished && !live.dead) {
        try {
          ex.pk_drop(task);
        } catch {
          live.dead = true;
        }
      }
    }
  }

  #instance(): Live {
    if (this.#live && !this.#live.dead) return this.#live;
    const live = new Live();
    const imports = {
      pk_host: {
        now_ms: (): number => Date.now(),
        random: (ptr: number, len: number): void => {
          // getRandomValues fills at most 65,536 bytes per call.
          for (let at = 0; at < len; at += 65536) {
            crypto.getRandomValues(live.bytes(ptr + at, Math.min(65536, len - at)));
          }
        },
        fetch: (op: number, ptr: number, len: number): void => {
          const request = JSON.parse(decoder.decode(live.bytes(ptr, len))) as {
            method: string;
            url: string;
            headers: Array<[string, string]>;
            body: string | null;
          };
          live.track(this.#fetchOp(live, op, request));
        },
        sign_es256: (op: number, keyPtr: number, keyLen: number, dataPtr: number, dataLen: number): void => {
          const key = live.bytes(keyPtr, keyLen).slice();
          const data = live.bytes(dataPtr, dataLen).slice();
          live.track(this.#signOp(live, op, key, data));
        },
        log: (level: number, ptr: number, len: number): void => {
          const text = decoder.decode(live.bytes(ptr, len));
          if (level === 0) live.lastPanic = text;
          this.#log(level, text);
        },
      },
    };
    const instance = new WebAssembly.Instance(this.#module, imports);
    live.exports = instance.exports as unknown as Exports;
    this.#live = live;
    return live;
  }

  async #fetchOp(
    live: Live,
    op: number,
    request: { method: string; url: string; headers: Array<[string, string]>; body: string | null },
  ): Promise<void> {
    let status: number;
    let data: Uint8Array;
    try {
      // `manual`: a signed request is never re-sent to wherever a redirect
      // points; the module sees the 3xx and its Location, and decides.
      // Workers' fetch keeps no cookies, so nothing carries between sends.
      const response = await this.#fetch(request.url, {
        method: request.method,
        headers: request.headers,
        body: request.body ?? undefined,
        redirect: "manual",
      });
      status = response.status;
      data = frameResponse([...response.headers], new Uint8Array(await response.arrayBuffer()));
    } catch (e) {
      status = STATUS_FAILED;
      data = encoder.encode(message(e));
    }
    live.deliver(op, status, data);
  }

  async #signOp(live: Live, op: number, key: Uint8Array, data: Uint8Array): Promise<void> {
    const subtle = typeof crypto === "undefined" ? undefined : crypto.subtle;
    if (!subtle) {
      live.deliver(op, STATUS_UNSUPPORTED, new Uint8Array());
      return;
    }
    try {
      const cryptoKey = await this.#importKey(subtle, key);
      const signature = await subtle.sign({ name: "ECDSA", hash: "SHA-256" }, cryptoKey, data);
      live.deliver(op, 0, new Uint8Array(signature));
    } catch (e) {
      const unsupported = e instanceof Error && e.name === "NotSupportedError";
      live.deliver(op, unsupported ? STATUS_UNSUPPORTED : STATUS_FAILED, encoder.encode(message(e)));
    }
  }

  /** Imported keys, kept for the life of the isolate: an import costs more
   * than a signature. Keyed by the key's own bytes; a handful at most. */
  #importKey(subtle: SubtleCrypto, der: Uint8Array): Promise<CryptoKey> {
    const id = Array.from(der, (b) => b.toString(16).padStart(2, "0")).join("");
    let key = this.#keys.get(id);
    if (!key) {
      if (this.#keys.size >= 8) this.#keys.clear();
      key = subtle.importKey("pkcs8", der, { name: "ECDSA", namedCurve: "P-256" }, false, ["sign"]);
      key.catch(() => this.#keys.delete(id));
      this.#keys.set(id, key);
    }
    return key;
  }

  /** The answer for a call whose instance trapped (a panic) or failed. */
  #crashed(live: Live, error?: unknown): CallAnswer {
    if (error !== undefined && !(error instanceof WebAssembly.RuntimeError)) throw error;
    live.dead = true;
    if (this.#live === live) this.#live = null;
    const why = live.lastPanic ? `: ${live.lastPanic.replace(/\s+/g, " ").trim()}` : "";
    return {
      result: {
        content: [
          {
            type: "text",
            text:
              `other: the ${this.label} module crashed${why}. A fresh copy will serve the next call; ` +
              `if it crashes again, tell the user to check the Worker's logs.`,
          },
        ],
        isError: true,
      },
    };
  }
}
