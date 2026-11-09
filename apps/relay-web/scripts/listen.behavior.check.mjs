import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(process.env.LISTEN_SOURCE ?? new URL("../public/listen.js", import.meta.url), "utf8")
  .replace(/^import .*;$/m, "");
const offer = { t: "offer", sdp: "v=0\r\na=ice-ufrag:host\r\na=sendrecv\r\n", trickle: true };
const flush = () => new Promise((resolve) => setImmediate(resolve));

function browser({ webRtc = true, constructorThrows = false, rejectOffer = false, rejectPlayback = false } = {}) {
  const elements = new Map(), sockets = [], peers = [], timers = new Map(), micRequests = [];
  let now = 0, nextTimer = 0;
  const element = (id) => {
    if (!elements.has(id)) {
      const attributes = new Map();
      elements.set(id, {
      value: "", min: -60, options: [], textContent: "", hidden: false,
      style: { setProperty() {} }, classList: { toggle() {}, remove() {} },
      setAttribute(key, value) { attributes.set(key, value); },
      getAttribute(key) { return attributes.get(key); },
      focus() {}, replaceChildren() {}, appendChild() { return {}; },
      play: () => rejectPlayback ? Promise.reject(Error("playback blocked")) : Promise.resolve(),
      });
    }
    return elements.get(id);
  };
  class Socket {
    static OPEN = 1;
    constructor() { this.readyState = 1; this.sent = []; sockets.push(this); }
    send(message) { this.sent.push(JSON.parse(message)); }
    close() { this.readyState = 3; this.onclose?.(); }
    message(message) { this.onmessage({ data: JSON.stringify(message) }); }
  }
  class Peer {
    constructor() {
      if (constructorThrows) throw Error("peer connection blocked");
      this.connectionState = "new"; this.iceGatheringState = "complete"; peers.push(this);
    }
    async setRemoteDescription() {
      if (rejectOffer === "deferred") return new Promise((resolve, reject) => { this.rejectOffer = reject; });
      if (rejectOffer) throw Error("offer rejected");
    }
    getTransceivers() { return [{ sender: { getParameters: () => ({}) } }]; }
    async createAnswer() { return { type: "answer", sdp: "v=0\r\n" }; }
    async setLocalDescription(description) { this.localDescription = description; }
    close() { this.connectionState = "closed"; }
    state(state) { this.connectionState = state; this.onconnectionstatechange(); }
    track() { this.ontrack({ track: {}, streams: [{}], receiver: {} }); }
  }
  const node = () => ({ connect() { return node(); }, disconnect() {}, gain: { setTargetAtTime() {} } });
  class Audio {
    constructor() { this.currentTime = 0; this.destination = {}; }
    resume() { return Promise.resolve(); }
    createMediaStreamSource() { return node(); }
    createGain() { return node(); }
    createChannelSplitter() { return node(); }
    createAnalyser() { return node(); }
  }
  const context = vm.createContext({
    document: { getElementById: element, createElement: () => element("created"), documentElement: { classList: { remove() {} } } },
    window: { RTCPeerConnection: webRtc ? Peer : undefined }, localStorage: {}, location: { pathname: "/test-room", protocol: "https:", host: "relay.test" },
    performance: { now: () => now }, WebSocket: Socket, AudioContext: Audio,
    navigator: { mediaDevices: {
      getUserMedia: () => new Promise((resolve, reject) => micRequests.push({ resolve, reject })),
      enumerateDevices: async () => [],
    } },
    RTCPeerConnection: webRtc ? Peer : undefined, slug: (s) => s, auth: async () => "credential",
    setTimeout: (fn, ms) => { const id = ++nextTimer; timers.set(id, { at: now + ms, fn }); return id; },
    clearTimeout: (id) => timers.delete(id), setInterval() {}, requestAnimationFrame() {},
  });
  vm.runInContext(source, context);
  return {
    elements, sockets, micRequests,
    get peers() { return peers.filter((peer) => peer.connectionState !== "closed"); },
    click: () => element("form").onsubmit({ preventDefault() {} }),
    status: () => element("status").textContent,
    advance(ms) {
      const end = now + ms;
      for (;;) {
        const due = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!due) break;
        now = due[1].at; timers.delete(due[0]); due[1].fn();
      }
      now = end;
    },
  };
}

test("disabled WebRTC explains the failure before joining and allows another attempt", () => {
  const page = browser({ webRtc: false });
  page.click();
  assert.match(page.status(), /WebRTC unavailable/i);
  assert.equal(page.sockets.length, 0);
  assert.match(page.elements.get("detail").textContent, /browser/i);
  page.click();
  assert.match(page.status(), /WebRTC unavailable/i);
});

test("blocked peer connection construction is reported before signaling", () => {
  const page = browser({ constructorThrows: true });
  page.click();
  assert.match(page.status(), /WebRTC unavailable/i);
  assert.equal(page.sockets.length, 0);
});

test("connection and host response timeouts stop the spinner and allow retry", async () => {
  const page = browser();
  page.click();
  page.advance(15_001);
  assert.match(page.status(), /timed out/i);
  page.click();
  assert.equal(page.sockets.length, 2);
  page.sockets[1].message({ t: "ice", servers: [] });
  await flush();
  page.advance(20_001);
  assert.match(page.status(), /host did not respond/i);
});

test("no host remains a distinct waiting state and a host can join later", async () => {
  const page = browser();
  page.click();
  const socket = page.sockets[0];
  socket.message({ t: "ice", servers: [] });
  await flush();
  socket.message({ code: "no-host" });
  page.advance(60_000);
  assert.match(page.status(), /waiting for host/i);
  socket.message({ t: "host" });
  await flush();
  assert.equal(socket.sent.filter((m) => m.t === "hello").length, 2);
  socket.message(offer);
  await flush();
  page.peers[0].state("connected");
  page.peers[0].track();
  await flush();
  assert.match(page.status(), /live/i);
});

test("failed negotiation reports an error and a subsequent attempt can connect", async () => {
  const page = browser({ rejectOffer: true });
  page.click();
  page.sockets[0].message({ t: "ice", servers: [] });
  await flush();
  page.sockets[0].message(offer);
  await flush();
  assert.match(page.status(), /connection failed/i);
  page.click();
  assert.equal(page.sockets.length, 2);
  assert.match(page.status(), /connecting/i);
});

test("peer connection timeout and playback rejection produce separate errors", async () => {
  const waiting = browser();
  waiting.click();
  waiting.sockets[0].message({ t: "ice", servers: [] });
  await flush();
  waiting.sockets[0].message(offer);
  await flush();
  waiting.advance(30_001);
  assert.match(waiting.status(), /timed out/i);
  const audio = browser({ rejectPlayback: true });
  audio.click();
  audio.sockets[0].message({ t: "ice", servers: [] });
  await flush();
  audio.sockets[0].message(offer);
  await flush();
  audio.peers[0].state("connected");
  audio.peers[0].track();
  await flush();
  assert.match(audio.status(), /audio blocked/i);
});


test("late failures from a stopped attempt do not disturb the retry", async () => {
  const page = browser({ rejectOffer: "deferred" });
  page.click();
  const oldSocket = page.sockets[0];
  oldSocket.message({ t: "ice", servers: [] });
  await flush();
  oldSocket.message(offer);
  const oldPeer = page.peers[0];
  page.click();
  page.click();
  oldPeer.rejectOffer(Error("late rejection"));
  oldSocket.message({ code: "denied" });
  oldPeer.state("failed");
  await flush();
  assert.match(page.status(), /connecting/i);
  assert.equal(page.sockets.length, 2);
});


test("late microphone grants from stopped or superseded attempts are released", async () => {
  const page = browser();
  const stream = () => {
    const track = { stopped: false, stop() { this.stopped = true; }, getSettings: () => ({}) };
    return { track, getTracks: () => [track], getAudioTracks: () => [track] };
  };
  page.click();
  page.elements.get("mic").onclick();
  page.click();
  page.click();
  page.elements.get("mic").onclick();
  assert.equal(page.micRequests.length, 2);
  const stale = stream(), current = stream();
  page.micRequests[0].resolve(stale);
  await flush();
  assert.equal(stale.track.stopped, true, "a stopped attempt releases its microphone");
  assert.equal(page.elements.get("mic").getAttribute("aria-pressed"), "false");
  page.micRequests[1].resolve(current);
  await flush();
  assert.equal(current.track.stopped, false, "the current attempt keeps its microphone");
  page.click();
  assert.equal(current.track.stopped, true, "stopping releases the current microphone");
});

test("the latest overlapping microphone request wins", async () => {
  const page = browser();
  const stream = () => {
    const track = { stopped: false, stop() { this.stopped = true; }, getSettings: () => ({}) };
    return { track, getTracks: () => [track], getAudioTracks: () => [track] };
  };
  page.click();
  page.elements.get("mic").onclick();
  page.elements.get("mic").onclick();
  assert.equal(page.micRequests.length, 2);
  const stale = stream(), current = stream();
  page.micRequests[1].resolve(current);
  await flush();
  page.micRequests[0].resolve(stale);
  await flush();
  assert.equal(stale.track.stopped, true);
  assert.equal(current.track.stopped, false);
  assert.equal(page.elements.get("mic").getAttribute("aria-pressed"), "true");
  page.click();
  assert.equal(current.track.stopped, true);
});
