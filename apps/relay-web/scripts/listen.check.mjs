import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(new URL("../public/listen.js", import.meta.url), "utf8")
  .replace(/^import .*;$/m, "");
const offer = "v=0\r\na=ice-ufrag:hostGeneration\r\na=sendrecv\r\n";
const candidate = { type: "srflx", candidate: "candidate:1 1 udp 1 203.0.113.1 1234 typ srflx" };

function page(servers = []) {
  const elements = new Map(), sent = [], timers = new Set(), conns = [];
  const element = (id) => {
    if (!elements.has(id)) elements.set(id, {
      value: "", min: -60, options: [], style: { setProperty() {} },
      setAttribute() {}, classList: { toggle() {} },
    });
    return elements.get(id);
  };
  class Peer {
    constructor() { conns.push(this); this.iceGatheringState = "new"; }
    async setRemoteDescription() {}
    getTransceivers() { return [{ sender: { getParameters: () => ({}) } }]; }
    async createAnswer() { return { type: "answer", sdp: "answer with current candidates" }; }
    async setLocalDescription(description) {
      this.localDescription = description;
      this.iceGatheringState = "gathering";
      this.onicecandidate({ candidate }); // can arrive before answer was sent
    }
    close() { this.connectionState = "closed"; }
    async getStats() { return new Map(); }
  }
  const socket = { readyState: 1, send: (message) => sent.push(JSON.parse(message)) };
  const context = vm.createContext({
    document: { getElementById: element }, window: {}, localStorage: {},
    location: { pathname: "/test-room" }, performance: { now: () => 100 },
    RTCPeerConnection: Peer, WebSocket: { OPEN: 1 }, slug: (s) => s,
    setTimeout: (fn) => { timers.add(fn); return fn; }, clearTimeout: (fn) => timers.delete(fn),
    setInterval() {}, requestAnimationFrame() {}, socket, serversForTest: servers,
  });
  vm.runInContext(source, context);
  vm.runInContext("ws = socket; timing = { start: 0 }; servers = serversForTest;", context);
  return { sent, timers, conns, context, answer: (trickle) =>
    vm.runInContext(`answer(${JSON.stringify(offer)}, undefined, ${trickle})`, context) };
}

test("early answers preserve candidate order, legacy relay gathering and stale-connection guards", async () => {
  const modern = page();
  await modern.answer(true);
  assert.deepEqual(modern.sent.map((m) => m.t), ["answer", "candidate"]);
  assert.equal(modern.sent[1].ufrag, "hostGeneration");
  assert.equal(modern.timers.size, 0, "trickle never waits for full gathering");
  const old = modern.conns[0];
  await modern.answer(true);
  const count = modern.sent.length;
  old.onicecandidate({ candidate });
  assert.equal(modern.sent.length, count, "closed peer cannot leak stale candidates");
  modern.conns[1].onicecandidate({ candidate });
  assert.equal(modern.sent.at(-1).t, "candidate", "late candidates are forwarded");
  modern.conns[1].getStats = async () => new Map([
    ["transport", { type: "transport", selectedCandidatePairId: "pair" }],
    ["pair", { type: "candidate-pair", currentRoundTripTime: 0.023, localCandidateId: "local", remoteCandidateId: "remote" }],
    ["local", { candidateType: "srflx" }], ["remote", { candidateType: "host" }],
    ["audio", { type: "inbound-rtp", kind: "audio", jitter: 0.004, jitterBufferDelay: 0.08,
      jitterBufferEmittedCount: 2, packetsReceived: 100, packetsLost: 2 }],
  ]);
  const diagnostics = await modern.context.window.relayDiagnostics();
  assert.equal(diagnostics.rttMs, 23);
  assert.equal(diagnostics.jitterMs, 4);
  assert.equal(diagnostics.jitterBufferMs, 40);
  assert.equal(diagnostics.localCandidate, "srflx");
  assert.equal(diagnostics.packetsLost, 2);

  const legacy = page();
  await legacy.answer(false);
  assert.deepEqual(legacy.sent.map((m) => m.t), ["answer"]);
  assert.equal(legacy.conns[0].iceGatheringState, "gathering", "STUN answer need not wait for complete");
  assert.equal(legacy.timers.size, 0);

  const relay = page([{ urls: "turn:relay.test:3478" }]);
  const pending = relay.answer(false);
  await new Promise((done) => setImmediate(done));
  assert.equal(relay.sent.length, 0, "legacy TURN keeps gathering relay candidates");
  relay.conns[0].iceGatheringState = "complete";
  relay.conns[0].onicegatheringstatechange();
  await pending;
  assert.equal(relay.sent[0].t, "answer");
  assert.equal(relay.timers.size, 0);
});
