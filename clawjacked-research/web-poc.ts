// web-poc.ts - ClawJacked WebSocket accessibility check, from a browser-like
// vantage point.
//
// Reference: Oasis Security "ClawJacked" disclosure (2026-02-26, publicly
// disclosed and patched upstream in OpenClaw 2026.2.25).
//
// What this does:
//   Opens a WebSocket to the gateway, completes the documented handshake
//   (connect.challenge -> req/connect), and reports what the gateway admitted.
//   This is exactly what a malicious page's JavaScript would do, so it is the
//   closest in-repo approximation of the published attack precondition.
//
// What changed from the previous version:
//   * it no longer depends on the third-party `ws` package or on a TypeScript
//     runtime: the WebSocket client is implemented on top of node's built-in
//     `net` module, so `node web-poc.js` works after a plain `tsc`
//   * it no longer hard-codes the wrong port (4000); the gateway default is 18789
//   * it actually parses the handshake reply and reports the verdict, instead of
//     only logging whatever arrived
//
// Build:  tsc web-poc.ts --target es2020 --module commonjs
// Run:    node web-poc.js
//         node web-poc.js --port 18790 --origin https://evil.example

import * as crypto from "crypto";
import * as net from "net";

const DEFAULT_PORT = 18789;
const DEFAULT_HOST = "127.0.0.1";
const PATH = "/gateway";
const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const TIMEOUT_MS = 8000;

export interface Options {
  host: string;
  port: number;
  token?: string;
  origin?: string;
}

export type Verdict =
  | "accepted-unauthenticated"
  | "accepted-authenticated"
  | "refused"
  | "upgrade-refused"
  | "unreachable";

export interface Result {
  verdict: Verdict;
  scopes: string[];
  detail: string;
}

/** Encode a client frame (always masked, as clients must be). */
export function encodeFrame(text: string): Buffer {
  const payload = Buffer.from(text, "utf8");
  const mask = crypto.randomBytes(4);
  let header: Buffer;
  if (payload.length < 126) {
    header = Buffer.from([0x81, 0x80 | payload.length]);
  } else if (payload.length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 0x80 | 126;
    header.writeUInt16BE(payload.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 0x80 | 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  const masked = Buffer.alloc(payload.length);
  for (let i = 0; i < payload.length; i++) {
    masked[i] = payload[i] ^ mask[i % 4];
  }
  return Buffer.concat([header, mask, masked]);
}

/** Parse one unmasked server frame. Returns null when more bytes are needed. */
export function decodeFrame(
  buf: Buffer
): { opcode: number; text: string; rest: Buffer } | null {
  if (buf.length < 2) return null;
  const opcode = buf[0] & 0x0f;
  const masked = (buf[1] & 0x80) !== 0;
  let length = buf[1] & 0x7f;
  let offset = 2;
  if (length === 126) {
    if (buf.length < 4) return null;
    length = buf.readUInt16BE(2);
    offset = 4;
  } else if (length === 127) {
    if (buf.length < 10) return null;
    length = Number(buf.readBigUInt64BE(2));
    offset = 10;
  }
  let mask: Buffer | null = null;
  if (masked) {
    if (buf.length < offset + 4) return null;
    mask = buf.subarray(offset, offset + 4);
    offset += 4;
  }
  if (buf.length < offset + length) return null;
  const payload = Buffer.from(buf.subarray(offset, offset + length));
  if (mask) {
    for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
  }
  if (opcode === 0x8) {
    const code = payload.length >= 2 ? payload.readUInt16BE(0) : 1005;
    const reason = payload.subarray(2).toString("utf8");
    return { opcode, text: `close ${code} ${reason}`.trim(), rest: buf.subarray(offset + length) };
  }
  return { opcode, text: payload.toString("utf8"), rest: buf.subarray(offset + length) };
}

/** Run the accessibility check and return a structured verdict. */
export function check(opts: Options): Promise<Result> {
  return new Promise((resolve) => {
    const key = crypto.randomBytes(16).toString("base64");
    let header = "";
    header += `GET ${PATH} HTTP/1.1\r\n`;
    header += `Host: ${opts.host}:${opts.port}\r\n`;
    header += "Upgrade: websocket\r\n";
    header += "Connection: Upgrade\r\n";
    header += `Sec-WebSocket-Key: ${key}\r\n`;
    header += "Sec-WebSocket-Version: 13\r\n";
    if (opts.origin) header += `Origin: ${opts.origin}\r\n`;
    header += "\r\n";

    let settled = false;
    const done = (r: Result) => {
      if (!settled) {
        settled = true;
        socket.destroy();
        resolve(r);
      }
    };

    const socket = net.connect({ host: opts.host, port: opts.port }, () => {
      socket.write(header);
    });
    socket.setTimeout(TIMEOUT_MS);
    socket.on("timeout", () => done({ verdict: "unreachable", scopes: [], detail: "timed out" }));
    socket.on("error", (err: Error) =>
      done({ verdict: "unreachable", scopes: [], detail: err.message })
    );

    let upgraded = false;
    let buffer: Buffer = Buffer.alloc(0);
    let sentConnect = false;

    socket.on("data", (chunk: Buffer) => {
      buffer = Buffer.concat([buffer, chunk]);
      if (!upgraded) {
        const end = buffer.indexOf("\r\n\r\n");
        if (end === -1) return;
        const head = buffer.subarray(0, end).toString("latin1");
        buffer = buffer.subarray(end + 4);
        if (!head.startsWith("HTTP/1.1 101")) {
          done({
            verdict: "upgrade-refused",
            scopes: [],
            detail: head.split("\r\n")[0] || "no status line",
          });
          return;
        }
        upgraded = true;
      }
      while (true) {
        const frame = decodeFrame(buffer);
        if (!frame) return;
        buffer = frame.rest;
        if (frame.opcode === 0x8) {
          done({
            verdict: sentConnect ? "refused" : "upgrade-refused",
            scopes: [],
            detail: frame.text,
          });
          return;
        }
        if (!sentConnect) {
          sentConnect = true;
          const auth = opts.token ? { token: opts.token } : {};
          socket.write(
            encodeFrame(
              JSON.stringify({
                type: "req",
                id: "web-poc",
                method: "connect",
                params: {
                  minProtocol: 3,
                  maxProtocol: 3,
                  client: { id: "web-poc", version: "1.0.0", mode: "operator" },
                  role: "operator",
                  scopes: [],
                  auth,
                },
              })
            )
          );
          continue;
        }
        try {
          const reply = JSON.parse(frame.text);
          const payload = reply.payload || {};
          if (reply.ok && payload.type === "hello-ok") {
            const scopes: string[] = payload.scopes || [];
            done({
              verdict: opts.token ? "accepted-authenticated" : "accepted-unauthenticated",
              scopes,
              detail: `granted ${JSON.stringify(scopes)}`,
            });
          } else {
            done({ verdict: "refused", scopes: [], detail: frame.text.slice(0, 160) });
          }
        } catch {
          done({ verdict: "refused", scopes: [], detail: frame.text.slice(0, 160) });
        }
        return;
      }
    });

    socket.on("close", () =>
      done({ verdict: upgraded ? "refused" : "upgrade-refused", scopes: [], detail: "connection closed" })
    );
  });
}

function parseArgs(argv: string[]): Options {
  const opts: Options = { host: DEFAULT_HOST, port: DEFAULT_PORT };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--host") opts.host = argv[++i] ?? DEFAULT_HOST;
    else if (arg === "--port") opts.port = Number(argv[++i] ?? DEFAULT_PORT);
    else if (arg === "--token") opts.token = argv[++i];
    else if (arg === "--origin") opts.origin = argv[++i];
  }
  return opts;
}

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));
  console.log("=".repeat(64));
  console.log("ClawJacked WebSocket accessibility check (TypeScript)");
  console.log("Public disclosure, patched upstream. No exploitation performed.");
  console.log("=".repeat(64));
  console.log(`[*] Target: ws://${opts.host}:${opts.port}${PATH}`);

  const result = await check(opts);
  for (const line of [
    `[*] Verdict: ${result.verdict}`,
    `[*] Detail: ${result.detail}`,
  ]) {
    console.log(line);
  }
  console.log("=".repeat(64));

  switch (result.verdict) {
    case "accepted-unauthenticated":
      console.log("RESULT: an unauthenticated caller was admitted.");
      console.log("[i] This is the ClawJacked precondition, not proof of exploitation.");
      process.exitCode = 0;
      break;
    case "accepted-authenticated":
      console.log("RESULT: admitted with the supplied credential.");
      process.exitCode = 0;
      break;
    default:
      console.log("RESULT: not admitted - consistent with a patched gateway.");
      process.exitCode = 1;
  }
}

if (require.main === module) {
  void main();
}
