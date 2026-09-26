// Drives headless Chrome with a fake mic on the listen page: press Talk,
// stay a while. Pair with `RELAY_TALK_ROOM=<room> cargo test -p relay-core
// live_browser_talks_back -- --ignored`.
//   node scripts/talk-e2e.mjs <room> [seconds] [chrome]
import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [room, secs = "45", chrome = "google-chrome-stable"] = process.argv.slice(2);
const port = 9333;
const proc = spawn(chrome, [
  "--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${mkdtempSync(join(tmpdir(), "relay-"))}`,
  "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream", "--autoplay-policy=no-user-gesture-required",
  `https://relay.matari-audio.com/${room}`,
], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let page;
for (let i = 0; i < 50 && !page; i++) {
  await sleep(200);
  page = await fetch(`http://127.0.0.1:${port}/json`).then((r) => r.json()).then((t) => t.find((x) => x.type === "page")).catch(() => {});
}
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const evaluate = (expression) => new Promise((done) => {
  const me = ++id;
  ws.addEventListener("message", function on({ data }) {
    const m = JSON.parse(data);
    if (m.id === me) { ws.removeEventListener("message", on); done(m.result?.result?.value); }
  });
  ws.send(JSON.stringify({ id: me, method: "Runtime.evaluate", params: { expression, userGesture: true, awaitPromise: true } }));
});
await sleep(1500);
await evaluate(`document.getElementById("mic").click()`);
for (let t = 0; t < +secs; t += 3) {
  await sleep(3000);
  console.log(await evaluate(`JSON.stringify({ status: document.getElementById("status").textContent,
    mic: document.getElementById("mic").getAttribute("aria-pressed"), title: document.getElementById("mic").title,
    lvl: document.getElementById("mic").style.getPropertyValue("--lvl") })`));
}
ws.close();
proc.kill();
