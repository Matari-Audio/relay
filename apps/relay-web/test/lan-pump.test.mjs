// The LAN listen page ships inside the plugin binary, but its jitter buffer
// is plain JS and this is the only node runner in the repo.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const page = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "../../plugin/assets/lan/player.html"),
  "utf8",
);

const relayPump = new Function(
  page.slice(page.indexOf("function relayPump"), page.indexOf("async function makeSink")) +
    "return relayPump;",
)();

const chunk = (n, v) => ({ l: new Float32Array(n).fill(v), r: new Float32Array(n).fill(v) });

test("the page never touches audioWorklet without checking for it", () => {
  assert.match(page, /if \(ctx\.audioWorklet\)/);
  assert.match(page, /createScriptProcessor/);
  assert.equal(/await ctx\.audioWorklet\.addModule/.test(page.slice(page.indexOf("async function listen"))), false);
});

test("a full queue plays through after the join crossfade", () => {
  const pump = relayPump(48000, () => assert.fail("no underrun expected"));
  pump.push(chunk(1000, 1));
  const L = new Float32Array(1000);
  const R = new Float32Array(1000);
  pump.fill(L, R);
  assert.ok(L[0] < 0.01, "the join starts from silence, not a click");
  assert.ok(L[500] > 0.99 && R[500] > 0.99, "settles on the source");
});

test("an empty queue counts one dropout and decays instead of clicking", () => {
  const seen = [];
  const pump = relayPump(48000, (n) => seen.push(n));
  pump.push(chunk(1000, 1));
  const L = new Float32Array(1000);
  const R = new Float32Array(1000);
  pump.fill(L, R);
  pump.fill(L, R);
  pump.fill(L, R);
  assert.deepEqual(seen, [1], "one starved episode, not one per sample or per block");
  // 12 ms time constant: ~40 ms of starvation is about -29 dB.
  assert.ok(Math.abs(L[900]) < 0.05, `fades out instead of holding a tone, got ${L[900]}`);
  assert.ok(L.every(Number.isFinite), "no NaN leaks into the output");
  pump.push(chunk(1000, 1));
  pump.fill(L, R);
  assert.deepEqual(seen, [1], "a refill does not re-report the old dropout");
  assert.ok(L[500] > 0.99, "audio resumes");
});

test("clear drops the backlog rather than playing it late", () => {
  const pump = relayPump(48000, () => {});
  pump.push(chunk(1000, 1));
  pump.push({ clear: true });
  const L = new Float32Array(8);
  const R = new Float32Array(8);
  pump.fill(L, R);
  assert.ok(L.every((v) => v === 0), "nothing left to play");
});
