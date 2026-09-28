import { SELF } from "cloudflare:test";
import { expect, it } from "vitest";

type Msg = Record<string, any>;

// A socket plus queues of parsed messages, so tests can await the next one.
// Rosters go to their own queue, so the signaling tests read as before.
function inbox() {
  const queue: Msg[] = [];
  const waiters: ((m: Msg) => void)[] = [];
  return {
    push: (m: Msg) => (waiters.length ? waiters.shift()!(m) : queue.push(m)),
    next: () => (queue.length ? Promise.resolve(queue.shift()!) : new Promise<Msg>((r) => waiters.push(r))),
  };
}

async function open(path: string) {
  const res = await SELF.fetch(`https://relay.test${path}`, { headers: { Upgrade: "websocket" } });
  const ws = res.webSocket!;
  const msgs = inbox();
  const rosters = inbox();
  let closed: ((code: number) => void) | undefined;
  const closedP = new Promise<number>((r) => (closed = r));
  ws.addEventListener("message", (e) => {
    const m = JSON.parse(e.data as string);
    (m.t === "roster" ? rosters : msgs).push(m);
  });
  ws.addEventListener("close", (e) => closed!(e.code));
  ws.accept();
  return {
    ws,
    closed: closedP,
    send: (m: Msg) => ws.send(JSON.stringify(m)),
    next: msgs.next,
    roster: rosters.next,
  };
}

let n = 0;
const room = () => `room-${++n}-${Date.now()}`;
const KEY = "00112233445566778899aabbccddeeff";
const AUTH = "0123456789abcdef";

it("serves the listen page at /<room> and rejects bad paths", async () => {
  const page = await SELF.fetch("https://relay.test/big-filthy-papaya");
  expect(page.status).toBe(200);
  expect(await page.text()).toContain("<audio");
  expect((await SELF.fetch("https://relay.test/Bad_Room")).status).toBe(404);
  expect((await SELF.fetch("https://relay.test/x/peer")).status).toBe(426);
});

it("claims the host, refuses another key while connected, lets it in after", async () => {
  const r = room();
  const a = await open(`/${r}/host?key=${KEY}`);
  expect((await a.next()).t).toBe("ice");
  const b = await open(`/${r}/host?key=ffffffffffffffff`);
  expect(await b.next()).toEqual({ t: "error", code: "taken" });
  a.ws.close(1000);
  await a.closed;
  const c = await open(`/${r}/host?key=ffffffffffffffff`);
  expect((await c.next()).t).toBe("ice");
});

it("peer without a host gets no-host, then host, then hello is forwarded with an id", async () => {
  const r = room();
  const p = await open(`/${r}/peer`);
  expect((await p.next()).servers[0].urls).toBe("stun:stun.cloudflare.com:3478");
  expect(await p.next()).toEqual({ t: "error", code: "no-host" });
  const h = await open(`/${r}/host?key=${KEY}`);
  await h.next(); // ice
  expect(await p.next()).toEqual({ t: "host" });
  p.send({ t: "hello", kind: "web", auth: AUTH });
  const hello = await h.next();
  expect(hello).toMatchObject({ t: "hello", kind: "web", auth: AUTH });
  expect(typeof hello.id).toBe("number");
});

it("routes offer to the peer and answer back to the host", async () => {
  const r = room();
  const h = await open(`/${r}/host?key=${KEY}`);
  await h.next();
  const p1 = await open(`/${r}/peer`);
  const p2 = await open(`/${r}/peer`);
  await p1.next();
  await p2.next();
  p1.send({ t: "hello", kind: "plugin", auth: AUTH });
  p2.send({ t: "hello", kind: "web", auth: AUTH });
  const id1 = (await h.next()).id;
  const id2 = (await h.next()).id;
  expect(id1).not.toBe(id2);
  h.send({ t: "offer", to: id2, sdp: "v=0 offer" });
  expect(await p2.next()).toEqual({ t: "offer", sdp: "v=0 offer" });
  p2.send({ t: "answer", sdp: "v=0 answer" });
  expect(await h.next()).toEqual({ t: "answer", id: id2, sdp: "v=0 answer" });
});

it("forwards deny as denied", async () => {
  const r = room();
  const h = await open(`/${r}/host?key=${KEY}`);
  await h.next();
  const p = await open(`/${r}/peer`);
  await p.next();
  p.send({ t: "hello", kind: "web", auth: AUTH });
  const { id } = await h.next();
  h.send({ t: "deny", to: id });
  expect(await p.next()).toEqual({ t: "error", code: "denied" });
});

it("tells the host when a peer leaves, and peers when the host leaves", async () => {
  const r = room();
  const h = await open(`/${r}/host?key=${KEY}`);
  await h.next();
  const p = await open(`/${r}/peer`);
  const q = await open(`/${r}/peer`);
  await p.next();
  await q.next();
  p.send({ t: "hello", kind: "web", auth: AUTH });
  const { id } = await h.next();
  p.ws.close(1000);
  expect(await h.next()).toEqual({ t: "leave", id });
  h.ws.close(1000);
  expect(await q.next()).toEqual({ t: "error", code: "no-host" });
});

it("drops oversized and malformed messages, closes a flooding socket", async () => {
  const r = room();
  const h = await open(`/${r}/host?key=${KEY}`);
  await h.next();
  const p = await open(`/${r}/peer`);
  await p.next();
  p.send({ t: "answer", sdp: "x".repeat(17 * 1024) });
  p.ws.send("not json");
  p.send({ t: "answer", sdp: "ok" });
  expect((await h.next()).sdp).toBe("ok");
  for (let i = 0; i < 30; i++) p.ws.send("{}");
  expect(await p.closed).toBe(1008);
});

it("sends everyone the roster when the host arrives, a peer is admitted or leaves", async () => {
  const r = room();
  const h = await open(`/${r}/host?key=${KEY}`);
  await h.next();
  expect(await h.roster()).toEqual({ t: "roster", you: null, peers: [{ id: 0, kind: "host" }] });
  const p = await open(`/${r}/peer`);
  const q = await open(`/${r}/peer`);
  await p.next();
  await q.next();
  p.send({ t: "hello", kind: "web", auth: AUTH, name: "Maya" });
  q.send({ t: "hello", kind: "plugin", auth: AUTH });
  const idP = (await h.next()).id;
  const idQ = (await h.next()).id;
  h.send({ t: "offer", to: idP, sdp: "v=0" });
  const one = [{ id: 0, kind: "host" }, { id: idP, kind: "web", name: "Maya" }];
  expect(await h.roster()).toEqual({ t: "roster", you: null, peers: one });
  expect(await p.roster()).toEqual({ t: "roster", you: idP, peers: one });
  expect(await q.roster()).toEqual({ t: "roster", you: idQ, peers: one });
  p.send({ t: "name", name: " Alex " });
  one[1].name = "Alex";
  expect((await h.roster()).peers).toEqual(one);
  expect((await p.roster()).peers).toEqual(one);
  expect((await q.roster()).peers).toEqual(one);
  h.send({ t: "deny", to: idQ }); // not admitted: no roster change
  h.send({ t: "offer", to: idQ, sdp: "v=0" });
  const two = [...one, { id: idQ, kind: "plugin" }];
  expect((await p.roster()).peers).toEqual(two);
  expect((await q.roster()).peers).toEqual(two);
  p.ws.close(1000);
  expect((await q.roster()).peers).toEqual([{ id: 0, kind: "host" }, { id: idQ, kind: "plugin" }]);
  h.ws.close(1000);
  expect((await q.roster()).peers).toEqual([]);
});
