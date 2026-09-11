import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const src = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "../src/worker.ts"),
  "utf8",
);

test("listen click never awaits empty audio.play", () => {
  assert.equal(src.includes("await speaker.play"), false);
  assert.match(src, /speaker\.play\(\)\.catch/);
});

test("unlock hides the gate before any await", () => {
  const unlock = src.slice(src.indexOf("const unlock"), src.indexOf("armAudio();") + 12);
  assert.match(unlock, /gate\.classList\.remove\('show'\)/);
  assert.match(unlock, /speaker\.play\(\)\.catch/);
  assert.doesNotMatch(unlock, /await /);
});

test("offer is answered without waiting for Listen", () => {
  assert.match(src, /pendingOffer = msg\.sdp;\s*acceptOffer\(msg\.sdp\)/);
  assert.doesNotMatch(src, /if \(ctx\) acceptOffer/);
});

test("page advertises listen revision 12", () => {
  assert.match(src, /name="relay-listen" content="13"/);
});

test("root is the listen join box, not a product landing", () => {
  assert.match(src, /function indexHtml/);
  assert.match(src, /listenPage\("session", true\)/);
  assert.equal(src.includes("function homeHtml"), false);
  assert.equal(src.includes("Download Linux"), false);
});

test("host joining a waiting listener is asked for an offer", () => {
  assert.match(src, /askHostForListeners\(\)/);
  assert.match(src, /private askHostForListeners/);
  assert.match(src, /requestOffer\('host on'\)/);
});

test("meters are flat GYR rails without analog LED notches", () => {
  assert.equal(src.includes('class="leds"'), false);
  assert.equal(src.includes('class="tics"'), false);
  assert.equal(src.includes("repeating-linear-gradient"), false);
  assert.match(src, /\.rail\{[^}]*width:8px/);
});

test("empty firefox end-of-candidates is not sent to the host", () => {
  assert.match(src, /!ev\.candidate\.candidate/);
});

test("listen page meters from plugin peak stats", () => {
  assert.match(src, /msg\.peak/);
  assert.match(src, /setMeterPeak\(msg\.peak\)/);
});

test("go and a hot peak kick playback instead of waiting on the jitter buffer", () => {
  assert.match(src, /function kickPlayback/);
  assert.match(src, /msg\.t === 'go'[\s\S]*kickPlayback\(\)/);
  assert.match(src, /msg\.peak > 0\.002[\s\S]*kickPlayback\(\)/);
});

test("disconnected is not treated as a fatal connection failure", () => {
  assert.equal(src.includes("Connection failed"), false);
  assert.match(src, /connectionState === 'disconnected'/);
  assert.match(src, /requestOffer\('ice failed'\)/);
  assert.match(src, /requestOffer\('ice stuck'\)/);
  assert.doesNotMatch(src, /iceConnectionState === 'disconnected'\) bounceSocket/);
});

test("a second offer does not reset ICE that is still checking", () => {
  assert.match(src, /offer ignored/);
  assert.match(src, /ice !== 'failed'/);
  assert.match(src, /ice !== 'closed'/);
});

test("ice-failed handler captures the peer before tearing it down", () => {
  assert.match(src, /const peer = pc;/);
  assert.match(src, /peer\.iceConnectionState === 'failed'/);
  assert.match(src, /requestOffer\('ice failed'\);\s*return;/);
});

test("a retry want hangs up the old pair before asking again", () => {
  assert.match(src, /function sendBye/);
  assert.match(src, /dropPc\(\);\s*sendBye\(\);/);
  assert.match(src, /t: 'bye'/);
});

test("host-on want does not drop an in-flight peer", () => {
  const request = src.slice(src.indexOf("function requestOffer"), src.indexOf("const cap"));
  assert.match(request, /retry/);
  assert.doesNotMatch(request, /wantLock = now;\s*dropPc\(\);\s*sendWant/);
});

test("missing offer asks the host again without closing signaling", () => {
  assert.match(src, /function requestOffer/);
  assert.match(src, /function armOfferWatch/);
  assert.match(src, /t: 'want'/);
  assert.match(src, /sendWant\('no offer'\)/);
  assert.match(src, /4000/);
  assert.match(src, /armOfferWatch\(\)/);
});

test("listen unmutes the audio element after the user gesture", () => {
  assert.match(src, /speaker\.muted = !armed/);
  assert.match(src, /speaker\.muted = false/);
});

test("on-page logs never print secrets", () => {
  assert.match(src, /function log\(/);
  assert.match(src, /function iceKind\(/);
  assert.match(src, /function sdpSummary\(/);
  assert.doesNotMatch(src, /log\(msg\.sdp\)/);
  assert.doesNotMatch(src, /log\(msg\.cand\)/);
  assert.doesNotMatch(src, /ice-pwd/);
  assert.match(src, /sdpSummary\(msg\.sdp\)/);
  assert.match(src, /iceKind\(msg\.cand\)/);
});

test("listen desk is a stereo channel strip", () => {
  assert.match(src, /aria-label="Left"/);
  assert.match(src, /aria-label="Right"/);
  assert.match(src, /id="coverL"/);
  assert.match(src, /id="coverR"/);
  assert.match(src, /createChannelSplitter/);
});

test("session title types a new room and redirects", () => {
  assert.match(src, /location\.assign\('\/' \+ slug\)/);
  assert.match(src, /id="title"/);
  assert.match(src, /class="title"/);
});

test("diagnostics live in an expandable tape, not a polite live region", () => {
  assert.match(src, /<details class="tape">/);
  assert.match(src, /id="tapeLine"/);
  assert.match(src, /role="status" aria-live="polite"/);
  assert.match(src, /id="log" aria-hidden="true"/);
});

test("listen gate is a dialog over the strip", () => {
  assert.match(src, /role="dialog"/);
  assert.match(src, /aria-modal="true"/);
});

test("volume is a hardware fader between L and R, not a native range chrome", () => {
  assert.match(src, /id="throw"/);
  assert.match(src, /id="cap"/);
  assert.match(src, /id="mute"/);
  assert.match(src, /coverL[\s\S]*id="vol"[\s\S]*coverR/);
});

test("session name field explains how to jump rooms", () => {
  assert.match(src, /id="titleHint"/);
  assert.match(src, /Type another name and press Enter to jump/);
});

test("Chrome Opus answers are munged to stereo=1 before setLocalDescription", () => {
  assert.match(src, /forceOpusStereo\.toString\(\)/);
  assert.match(src, /answer\.sdp = forceOpusStereo\(answer\.sdp\)/);
  assert.match(src, /setRemoteDescription\(\{ type: 'offer', sdp: forceOpusStereo\(sdp\) \}\)/);
});

test("Chromium and mobile skip the Web Audio tap that steals the element", () => {
  assert.match(src, /function tapIsUnsafe/);
  assert.match(src, /if \(tapIsUnsafe\(\)\) return/);
});

test("a password attempt is always answered, right or wrong", () => {
  const handler = src.slice(src.indexOf("async webSocketMessage"), src.indexOf("// Binary PCM is LAN-only"));
  assert.match(handler, /const ok = await this\.checkPassword\(message\)/);
  assert.match(handler, /ws\.send\(JSON\.stringify\(\{ t: "auth", ok \}\)\)/);
  assert.match(src, /if \(msg\.t === 'auth'\)/);
  assert.match(src, /pwerr\.textContent = 'Wrong password'/);
  assert.match(src, /lock\.classList\.remove\('show'\)/);
});

test("the LAN hand-off is a link, not a probe an https page cannot make", () => {
  assert.equal(src.includes("gatherHostIps"), false);
  assert.equal(src.includes("location.replace('http://'"), false);
  const lan = src.slice(src.indexOf("function maybeLan"), src.indexOf("function flush"));
  assert.match(lan, /a\.href = 'http:\/\/' \+ ip/);
  assert.match(lan, /hint\.hidden = false/);
});

test("peer count changes reach the tape", () => {
  assert.match(src, /const key = String\(msg\.ready\) \+ ':' \+ \(msg\.peers\|0\)/);
});
