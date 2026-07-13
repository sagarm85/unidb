#!/usr/bin/env node
// Render an HTML file to a page-numbered PDF using headless Google Chrome
// over the DevTools protocol. Chrome's print CSS cannot place a page-counter
// in a margin box, so we drive Page.printToPDF with a footer template.
//
// Usage: node render_pdf.mjs input.html output.pdf
// Requires: Google Chrome + Node >= 22 (built-in global WebSocket).

import { spawn } from "node:child_process";
import { writeFileSync, existsSync, realpathSync } from "node:fs";
import { resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

const [, , inArg, outArg] = process.argv;
if (!inArg || !outArg) {
  console.error("usage: node render_pdf.mjs input.html output.pdf");
  process.exit(2);
}
const inPath = resolve(inArg);
const outPath = resolve(outArg);
if (!existsSync(inPath)) {
  console.error(`input not found: ${inPath}`);
  process.exit(2);
}

const CHROME =
  process.env.CHROME_PATH ||
  [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
  ].find((p) => existsSync(p));
if (!CHROME) {
  console.error("Google Chrome not found; set CHROME_PATH");
  process.exit(2);
}

const PORT = 9222 + Math.floor(Math.random() * 1000);
const chrome = spawn(CHROME, [
  "--headless=new",
  "--disable-gpu",
  "--no-first-run",
  "--no-default-browser-check",
  `--remote-debugging-port=${PORT}`,
  "--user-data-dir=" + resolve(process.env.TMPDIR || "/tmp", `unidb-pdf-${PORT}`),
]);
chrome.on("error", (e) => {
  console.error("failed to launch Chrome:", e.message);
  process.exit(1);
});

async function cdpEndpoint() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${PORT}/json/version`);
      const j = await r.json();
      if (j.webSocketDebuggerUrl) return j.webSocketDebuggerUrl;
    } catch {}
    await sleep(100);
  }
  throw new Error("Chrome DevTools endpoint never came up");
}

function connect(url) {
  return new Promise((res, rej) => {
    const ws = new WebSocket(url);
    ws.onopen = () => res(ws);
    ws.onerror = (e) => rej(new Error("ws error: " + (e.message || "unknown")));
  });
}

async function main() {
  const ws = await connect(await cdpEndpoint());
  let id = 0;
  const pending = new Map();
  const events = [];
  const waiters = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) {
      const { res, rej } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? rej(new Error(msg.error.message)) : res(msg.result);
    } else if (msg.method) {
      events.push(msg);
      for (const w of waiters.splice(0)) w();
    }
  };
  // The /json/version socket is the *browser* endpoint; it has no Page domain.
  // Create a page target and talk to it via a flattened session.
  const rawSend = (method, params = {}, sessionId) =>
    new Promise((res, rej) => {
      const mid = ++id;
      pending.set(mid, { res, rej });
      const m = { id: mid, method, params };
      if (sessionId) m.sessionId = sessionId;
      ws.send(JSON.stringify(m));
    });

  const fileUrl = "file://" + realpathSync(inPath);
  const { targetId } = await rawSend("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await rawSend("Target.attachToTarget", {
    targetId,
    flatten: true,
  });
  const send = (method, params = {}) => rawSend(method, params, sessionId);
  const waitFor = async (method) => {
    while (true) {
      const e = events.find((x) => x.method === method && x.sessionId === sessionId);
      if (e) return e;
      await new Promise((r) => waiters.push(r));
    }
  };

  await send("Page.enable");
  await send("Page.navigate", { url: fileUrl });
  await waitFor("Page.loadEventFired");
  await sleep(300); // let fonts/SVG settle

  const footer = `<div style="width:100%;font-size:8px;color:#5a6b7c;
    font-family:Arial,sans-serif;text-align:center;">
    <span class="pageNumber"></span> / <span class="totalPages"></span></div>`;

  const { data } = await send("Page.printToPDF", {
    printBackground: true,
    preferCSSPageSize: true,
    displayHeaderFooter: true,
    headerTemplate: "<span></span>",
    footerTemplate: footer,
    marginTop: 0.55,
    marginBottom: 0.63,
    marginLeft: 0.47,
    marginRight: 0.47,
  });

  writeFileSync(outPath, Buffer.from(data, "base64"));
  console.log(`wrote ${outPath}`);
  ws.close();
  chrome.kill();
}

main().catch((e) => {
  console.error(e);
  chrome.kill();
  process.exit(1);
});
