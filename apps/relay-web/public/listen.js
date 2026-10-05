// RELAY listen page. Signaling per docs/protocol.md; audio is P2P WebRTC.
import { auth, slug } from "/tag.js";

const $ = (id) => document.getElementById(id);
let room = slug(decodeURIComponent(location.pathname.slice(1)));
const out = $("out");
$("room").value = room;
try { $("name").value = localStorage.listenerName ?? ""; } catch {}
$("name").onchange = () => {
  const name = $("name").value.trim().slice(0, 24);
  $("name").value = name;
  try { localStorage.listenerName = name; } catch {}
  if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ t: "name", name }));
};
window.onpopstate = () => location.reload();

let ws, pc, ctx, src, gain, analysers = [], servers = [], tries = 0, retry, on = false;
let timing;
const elapsed = (at) => at == null || !timing ? null : Math.round(at - timing.start);
out.onplaying = () => { if (timing && pc?.connectionState === "connected") timing.audio ??= performance.now(); };

// Local diagnostics only: no recurring telemetry or media is sent to Cloudflare.
window.relayDiagnostics = async () => {
  const conn = pc, report = await conn?.getStats();
  const stats = report ? [...report.values()] : [];
  const transport = stats.find((s) => s.type === "transport");
  const pair = report?.get(transport?.selectedCandidatePairId)
    ?? stats.find((s) => s.type === "candidate-pair" && s.nominated && s.state === "succeeded");
  const audio = stats.find((s) => s.type === "inbound-rtp" && s.kind === "audio");
  const ms = (seconds) => seconds == null ? null : Math.round(seconds * 1000);
  return {
    state: conn?.connectionState ?? "idle",
    setupMs: timing ? Object.fromEntries(["socket", "offer", "answer", "ice", "connected", "audio"]
      .map((phase) => [phase, elapsed(timing[phase])])) : null,
    rttMs: ms(pair?.currentRoundTripTime),
    localCandidate: report?.get(pair?.localCandidateId)?.candidateType ?? null,
    remoteCandidate: report?.get(pair?.remoteCandidateId)?.candidateType ?? null,
    jitterMs: ms(audio?.jitter),
    jitterBufferMs: audio?.jitterBufferEmittedCount > 0
      ? ms(audio.jitterBufferDelay / audio.jitterBufferEmittedCount) : null,
    packetsReceived: audio?.packetsReceived ?? 0,
    packetsLost: audio?.packetsLost ?? 0,
  };
};
// Talk: the mic stream while on, its level meter, and whether this room's
// plugin can hear browsers (older RELAYs offer a send-only track).
let mic, micLevel, canTalk = true;

// The volume fader, dB; the bottom stop mutes. Remembered per browser.
const vol = $("vol");
const linear = () => (+vol.value <= +vol.min ? 0 : 10 ** (vol.value / 20));
function volume() {
  const db = +vol.value;
  $("gain").textContent = db <= +vol.min ? "−∞" : `${db >= 0 ? "+" : ""}${db.toFixed(1).replace("-", "−")}`;
  gain?.gain.setTargetAtTime(linear(), ctx.currentTime, 0.02);
  try { localStorage.volume = vol.value; } catch {}
}
try { if (localStorage.volume) vol.value = localStorage.volume; } catch {}
vol.oninput = volume;
vol.ondblclick = () => { vol.value = 0; volume(); };
volume();

function status(text, lamp = "") {
  $("status").textContent = text;
  $("lamp").className = `lamp ${lamp}`;
  $("card").classList.toggle("live", lamp === "ok");
}

// Who is in: the host first, then listeners in join order.
function roster({ you, peers }) {
  const kinds = { host: "plugin", web: "browser", plugin: "plugin" };
  const rows = peers.map((p, i) => {
    const li = document.createElement("li");
    const name = p.kind === "host" ? "Host" : p.name
      ? `${p.name}${p.id === you ? " (you)" : ""}` : p.id === you ? "You" : `Listener ${i}`;
    for (const t of [name, kinds[p.kind] ?? p.kind]) li.appendChild(document.createElement("span")).textContent = t;
    return li;
  });
  $("count").textContent = peers.length;
  $("who").replaceChildren($("who").firstElementChild, ...rows);
  $("who").hidden = !peers.length;
}

function setOn(v) {
  on = v;
  $("listen").setAttribute("aria-pressed", String(v));
  $("label").textContent = v ? "Stop" : "Listen";
}

async function hello() {
  ws?.send(JSON.stringify({ t: "hello", kind: "web", auth: await auth(room, $("pw").value), name: $("name").value.trim() }));
}

function connect() {
  clearTimeout(retry);
  timing = { start: performance.now() };
  const sock = (ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/${room}/peer`));
  sock.onopen = () => { if (sock === ws) timing.socket = performance.now(); };
  sock.onmessage = async ({ data }) => {
    if (data === "pong") return;
    const m = JSON.parse(data);
    if (m.t === "ice") { servers = m.servers; tries = 0; status("Connecting"); hello(); }
    else if (m.t === "host") hello();
    else if (m.t === "offer") answer(m.sdp, m.servers, m.trickle === true);
    else if (m.t === "roster") roster(m);
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

async function answer(sdp, acceptedServers, trickle = false) {
  pc?.close();
  if (timing) timing.offer = performance.now();
  const sock = ws;
  const conn = (pc = new RTCPeerConnection({ iceServers: acceptedServers ?? servers }));
  const send = (msg) => {
    if (conn === pc && sock === ws && sock?.readyState === WebSocket.OPEN) sock.send(JSON.stringify(msg));
  };
  const ufrag = /^a=ice-ufrag:(.+)$/m.exec(sdp)?.[1].trim();
  const hasRelay = (acceptedServers ?? servers).some((s) => [s.urls].flat().some((url) => /^turns?:/.test(url)));
  let answerSent = false, gathered;
  const candidates = [];
  const usable = new Promise((done) => { gathered = done; });
  conn.onicecandidate = ({ candidate }) => {
    if (conn !== pc) return;
    if (!candidate || (!hasRelay && candidate.type === "srflx")) gathered();
    if (trickle && candidate) {
      const msg = { t: "candidate", candidate: candidate.candidate, ufrag };
      if (answerSent) send(msg); else candidates.push(msg);
    }
  };
  conn.onicegatheringstatechange = () => {
    if (conn !== pc || conn.iceGatheringState !== "complete") return;
    if (timing) timing.ice = performance.now();
    gathered();
  };
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
    if (s === "connected") { if (timing) timing.connected = performance.now(); status("Live", "ok"); }
    else if (s === "failed") { status("Reconnecting", "warn"); hello(); }
  };
  await conn.setRemoteDescription({ type: "offer", sdp });
  const t = conn.getTransceivers()[0];
  canTalk = /a=sendrecv/.test(sdp);
  if (t && canTalk) {
    t.direction = "sendrecv"; // sends nothing until a mic track is set
    if (mic) t.sender.replaceTrack(mic.getAudioTracks()[0]);
  }
  if (!canTalk && mic) talk(false);
  micButton();
  await conn.setLocalDescription(await conn.createAnswer());
  // New hosts can start ICE immediately; discoveries follow over the same socket.
  // Legacy hosts get a gathered answer as soon as a public route is available.
  if (!trickle && conn.iceGatheringState !== "complete") {
    const timeout = setTimeout(gathered, 5000);
    await usable;
    clearTimeout(timeout);
  }
  if (conn !== pc || sock !== ws) return;
  send({ t: "answer", sdp: conn.localDescription.sdp });
  if (timing) timing.answer = performance.now();
  answerSent = true;
  for (const candidate of candidates) send(candidate);
  // Voice needs far less than music: cap the mic at 32 kbps.
  const sender = t?.sender, params = sender?.getParameters();
  if (params?.encodings?.length) {
    params.encodings[0].maxBitrate = 32_000;
    sender.setParameters(params).catch(() => {});
  }
}

// Play through Web Audio so the fader works everywhere (iOS ignores
// <audio>.volume). The element stays attached, muted: Chrome only feeds a
// remote stream to Web Audio while an element holds it. Meters read after
// the fader, like the plugin's OUT.
function meter(stream) {
  src?.disconnect();
  out.muted = true;
  src = ctx.createMediaStreamSource(stream);
  gain ??= ctx.createGain();
  gain.gain.value = linear();
  gain.connect(ctx.destination);
  const split = ctx.createChannelSplitter(2);
  src.connect(gain).connect(split);
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
  if (micLevel) {
    micLevel.getFloatTimeDomainData(buf);
    let p = 0;
    for (const s of buf) p = Math.max(p, Math.abs(s));
    $("mic").style.setProperty("--lvl", 1 - frac(20 * Math.log10(p || 1e-9)));
  }
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
  src?.disconnect();
  src = undefined;
  analysers = [];
  talk(false);
  peaks.fill(0);
  roster({ peers: [] });
  status("Ready");
}

$("form").onsubmit = (e) => {
  e.preventDefault();
  const want = slug($("room").value);
  if (want !== room) {
    if (!want) return $("room").focus();
    if (on) stop();
    room = want;
    $("room").value = room;
    history.pushState(null, "", `/${room}`);
    document.documentElement.classList.remove("home");
  }
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

// Talk. The browser asks for the mic (and lets you pick one) on the first
// press; after that the menu below switches inputs.
function micButton(state = "") {
  const b = $("mic"), live = !!mic;
  b.className = state;
  b.disabled = on && !canTalk;
  b.setAttribute("aria-pressed", String(live));
  b.style.setProperty("--lvl", 0);
  b.title = !canTalk && on ? "This room's RELAY can't hear browsers yet. It needs an update."
    : state === "denied" ? "Mic blocked. Allow it in the address bar, then press again."
    : live ? "Talking: the room hears your mic. Press to stop."
    : "Talk: send your mic to the room";
  b.setAttribute("aria-label", b.title);
  $("input").hidden = !live || $("device").options.length < 2;
}

async function openMic(deviceId) {
  ctx ??= new AudioContext({ latencyHint: "interactive" });
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { deviceId: deviceId ? { exact: deviceId } : undefined, channelCount: 1,
      echoCancellation: true, noiseSuppression: true, autoGainControl: true },
  });
  mic?.getTracks().forEach((t) => t.stop());
  mic = stream;
  const track = stream.getAudioTracks()[0];
  await pc?.getTransceivers()[0]?.sender.replaceTrack(track).catch(() => {});
  micLevel = ctx.createAnalyser();
  micLevel.fftSize = 1024;
  ctx.createMediaStreamSource(stream).connect(micLevel);
  // Labels only show once permission is granted.
  const inputs = (await navigator.mediaDevices.enumerateDevices()).filter((d) => d.kind === "audioinput");
  $("device").replaceChildren(...inputs.map((d, i) => new Option(d.label || `Microphone ${i + 1}`, d.deviceId)));
  $("device").value = track.getSettings().deviceId ?? "";
  micButton();
}

function talk(want) {
  if (!want) {
    mic?.getTracks().forEach((t) => t.stop());
    mic = micLevel = undefined;
    pc?.getTransceivers()[0]?.sender.replaceTrack(null).catch(() => {});
    return micButton();
  }
  micButton("asking");
  openMic($("device").value || undefined).catch((e) => micButton(e.name === "NotAllowedError" ? "denied" : ""));
}

$("mic").onclick = () => {
  if (mic) return talk(false);
  if (!on) $("form").requestSubmit(); // talking implies listening
  talk(true);
};
$("device").onchange = () => mic && openMic($("device").value).catch(() => {});

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
// Incoming bitrate: the host adapts it to the slowest listener's line.
let rxBytes = 0;
setInterval(async () => {
  let bytes = 0;
  (await pc?.getStats().catch(() => undefined))?.forEach((r) => {
    if (r.type === "inbound-rtp" && r.kind === "audio") bytes = r.bytesReceived;
  });
  const kbps = Math.round(((bytes - rxBytes) * 8) / 1000);
  rxBytes = bytes;
  $("kbps").textContent = pc && kbps > 0 ? `${kbps} kbps` : "";
}, 1000);
if (!room) { $("label").textContent = "Join"; $("room").focus(); }
micButton();
