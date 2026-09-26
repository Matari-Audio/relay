// RELAY listen page. Signaling per docs/protocol.md; audio is P2P WebRTC.
import { auth, slug } from "/tag.js";

const $ = (id) => document.getElementById(id);
const room = slug(decodeURIComponent(location.pathname.slice(1)));
const out = $("out");
$("room").value = room;

let ws, pc, ctx, analysers = [], servers = [], tries = 0, retry, on = false;

function status(text, lamp = "") {
  $("status").textContent = text;
  $("lamp").className = `lamp ${lamp}`;
}

function setOn(v) {
  on = v;
  $("listen").setAttribute("aria-pressed", String(v));
  $("listen").textContent = v ? "Stop" : "Listen";
}

async function hello() {
  ws?.send(JSON.stringify({ t: "hello", kind: "web", auth: await auth(room, $("pw").value) }));
}

function connect() {
  clearTimeout(retry);
  const sock = (ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/${room}/peer`));
  sock.onmessage = async ({ data }) => {
    if (data === "pong") return;
    const m = JSON.parse(data);
    if (m.t === "ice") { servers = m.servers; tries = 0; status("Connecting"); hello(); }
    else if (m.t === "host") hello();
    else if (m.t === "offer") answer(m.sdp);
    else if (m.code === "no-host") status("Waiting for host", "warn");
    else if (m.code === "denied") { stop(); status("Wrong password", "bad"); $("pw").focus(); }
    else if (m.code === "full") { stop(); status("Room is full", "bad"); }
  };
  sock.onclose = () => {
    if (!on || sock !== ws) return;
    const wait = Math.min(30_000, 1000 * 2 ** tries++) * (0.5 + Math.random() / 2);
    status("Reconnecting", "warn");
    retry = setTimeout(connect, wait);
  };
}

async function answer(sdp) {
  pc?.close();
  const conn = (pc = new RTCPeerConnection({ iceServers: servers }));
  conn.ontrack = ({ track, streams, receiver }) => {
    // Ask for the smallest jitter buffer the browser allows.
    if ("playoutDelayHint" in receiver) receiver.playoutDelayHint = 0;
    if ("jitterBufferTarget" in receiver) receiver.jitterBufferTarget = 0;
    const stream = streams[0] ?? new MediaStream([track]);
    out.srcObject = stream;
    out.play().catch(() => {});
    meter(stream);
  };
  conn.onconnectionstatechange = () => {
    if (conn !== pc) return;
    const s = conn.connectionState;
    if (s === "connected") status("Live", "ok");
    else if (s === "failed") { status("Reconnecting", "warn"); hello(); }
  };
  await conn.setRemoteDescription({ type: "offer", sdp });
  await conn.setLocalDescription(await conn.createAnswer());
  // No trickle: send the answer once every candidate is in it.
  if (conn.iceGatheringState !== "complete") {
    await new Promise((done) => {
      conn.onicegatheringstatechange = () => conn.iceGatheringState === "complete" && done();
      setTimeout(done, 5000); // a dead STUN/TURN server must not stall us forever
    });
  }
  if (conn === pc) ws?.send(JSON.stringify({ t: "answer", sdp: conn.localDescription.sdp }));
}

function meter(stream) {
  const split = ctx.createChannelSplitter(2);
  ctx.createMediaStreamSource(stream).connect(split);
  analysers = [0, 1].map((ch) => {
    const a = ctx.createAnalyser();
    a.fftSize = 1024;
    split.connect(a, ch);
    return a;
  });
}

const buf = new Float32Array(1024);
const peaks = [0, 0];
// Peak hold per rail, dB, like the plugin: holds 1.5 s, then falls 20 dB/s.
const holds = [-60, -60], held = [0, 0];
const frac = (db) => Math.min(1, Math.max(0, -db / 60)); // 0 at 0 dB, 1 at the -60 dB floor
let shown = 0, last = 0;
function draw(now) {
  requestAnimationFrame(draw);
  const dt = Math.min(0.1, (now - last) / 1000);
  last = now;
  [0, 1].forEach((i) => {
    let p = 0;
    if (analysers[i]) {
      analysers[i].getFloatTimeDomainData(buf);
      for (const s of buf) p = Math.max(p, Math.abs(s));
    }
    peaks[i] = Math.max(p, peaks[i] * 0.93); // fast attack, ~0.5 s release
    const db = 20 * Math.log10(peaks[i] || 1e-9);
    if (db >= holds[i] || now - held[i] > 1500) {
      holds[i] = Math.max(db, holds[i] - 20 * dt, -60);
      if (db >= holds[i]) held[i] = now;
    }
    const lr = i ? "R" : "L";
    $(`m${lr}`).style.clipPath = `inset(${frac(db) * 100}% 0 0 0)`;
    $(`h${lr}`).style.transform = `translateY(${frac(holds[i]) * 100}%)`;
  });
  if (now - shown > 100) {
    shown = now;
    const db = 20 * Math.log10(Math.max(...peaks) || 1e-9);
    $("db").firstChild.textContent = db < -90 ? "−∞ " : `${db.toFixed(1).replace("-", "−")} `;
  }
}
requestAnimationFrame(draw);

function stop() {
  setOn(false);
  clearTimeout(retry);
  const sock = ws;
  ws = undefined;
  sock?.close();
  pc?.close();
  pc = undefined;
  out.srcObject = null;
  analysers = [];
  peaks.fill(0);
  status("Press Listen");
}

$("form").onsubmit = (e) => {
  e.preventDefault();
  const want = slug($("room").value);
  if (want !== room) return want && location.assign(`/${want}`);
  if (on) return stop();
  if (!room) return $("room").focus();
  // Inside the click: unlock audio before any await.
  ctx ??= new AudioContext({ latencyHint: "interactive" });
  ctx.resume();
  out.play().catch(() => {});
  setOn(true);
  status("Connecting");
  connect();
};

$("eye").onclick = () => {
  const show = $("pw").type === "password";
  $("pw").type = show ? "text" : "password";
  $("eye").setAttribute("aria-pressed", String(show));
  $("eye").setAttribute("aria-label", show ? "Hide password" : "Show password");
  $("slash").style.display = show ? "" : "none";
};

$("copy").onclick = async () => {
  await navigator.clipboard.writeText(`${location.origin}/${slug($("room").value)}`).catch(() => {});
  if (!on) status("Link copied");
};

setInterval(() => ws?.readyState === 1 && ws.send("ping"), 30_000);
if (!room) { status("Enter a room name"); $("room").focus(); }
