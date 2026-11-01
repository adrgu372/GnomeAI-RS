// web-poc-test.js - real tests for the WebSocket frame codec in web-poc.ts.
//
// The previous version of this file made no network requests and asserted
// nothing about the code; it registered mock handlers and printed "PASSED"
// regardless of the outcome. It now exercises the actual encode/decode
// functions exported by web-poc.js and fails loudly on a real mismatch.
//
// Run:  node web-poc-test.js        (after `npm run build`)

const assert = require("assert");
const { encodeFrame, decodeFrame } = require("./dist/web-poc.js");

let failures = 0;
let checks = 0;

function check(name, fn) {
  checks++;
  try {
    fn();
    console.log(`[ok]   ${name}`);
  } catch (err) {
    failures++;
    console.log(`[FAIL] ${name}: ${err.message}`);
  }
}

console.log("Testing the WebSocket frame codec in web-poc.js");
console.log("");

check("a short frame round-trips", () => {
  const decoded = decodeFrame(encodeFrame("hello"));
  assert.ok(decoded, "expected a decoded frame");
  assert.strictEqual(decoded.text, "hello");
});

check("client frames are masked", () => {
  const frame = encodeFrame("hello");
  assert.strictEqual(frame[1] & 0x80, 0x80, "mask bit must be set");
});

check("client frames use the text opcode", () => {
  const frame = encodeFrame("hello");
  assert.strictEqual(frame[0] & 0x0f, 0x1);
});

check("a frame longer than 125 bytes uses the 16-bit length form", () => {
  const text = "x".repeat(300);
  const frame = encodeFrame(text);
  assert.strictEqual(frame[1] & 0x7f, 126, "expected the 126 length marker");
  const decoded = decodeFrame(frame);
  assert.strictEqual(decoded.text, text);
});

check("a long frame round-trips unchanged", () => {
  const text = JSON.stringify({ type: "req", method: "connect", pad: "y".repeat(4000) });
  const decoded = decodeFrame(encodeFrame(text));
  assert.strictEqual(decoded.text, text);
});

check("a truncated frame decodes to null instead of throwing", () => {
  const frame = encodeFrame("truncated");
  assert.strictEqual(decodeFrame(frame.subarray(0, 3)), null);
});

check("decodeFrame reports leftover bytes for pipelined frames", () => {
  const a = encodeFrame("first");
  const b = encodeFrame("second");
  const both = Buffer.concat([a, b]);
  const first = decodeFrame(both);
  assert.strictEqual(first.text, "first");
  const second = decodeFrame(first.rest);
  assert.strictEqual(second.text, "second");
});

check("a server close frame is surfaced with its code", () => {
  // Unmasked close frame, code 1008, reason "auth required".
  const reason = Buffer.from("auth required", "utf8");
  const header = Buffer.from([0x88, reason.length + 2]);
  const code = Buffer.from([0x03, 0xf0]);
  const frame = Buffer.concat([header, code, reason]);
  const decoded = decodeFrame(frame);
  assert.strictEqual(decoded.opcode, 0x8);
  assert.ok(decoded.text.startsWith("close 1008"), decoded.text);
});

console.log("");
console.log(`${checks - failures}/${checks} checks passed`);
process.exitCode = failures === 0 ? 0 : 1;
