// web-poc.ts - Educational demonstration only
// Requires OpenClaw running locally on 127.0.0.1:4000

import { WebSocket } from "ws";

async function demonstrateClawJacked() {
  console.log("[+] Attempting ClawJacked-style connection...\n");

  const ws = new WebSocket("ws://127.0.0.1:4000");

  ws.on("open", () => {
    console.log("[+] WebSocket connected to localhost");
    ws.send(JSON.stringify({ type: "hello", from: "research-poc" }));
  });

  ws.on("message", (data) => {
    console.log("[*] Received:", data.toString());
  });

  ws.on("error", (err) => {
    console.log("[!] Connection refused:", err.message);
    console.log("[i] OpenClaw may be bound to loopback only or using auth");
  });

  ws.on("close", () => console.log("[+] Connection closed"));
}

demonstrateClawJacked();
