// RELAY signaling: one Durable Object per room forwards JSON between the host
// plugin and its peers. See docs/protocol.md. Audio never passes through here.
import { DurableObject } from "cloudflare:workers";

export interface Env {
  ROOM: DurableObjectNamespace<Room>;
  ASSETS: Fetcher;
  TURN_ENABLED?: string;
  TURN_KEY_ID?: string;
  TURN_KEY_TOKEN?: string;
  EXTRA_ICE?: string;
}

const MAX_PEERS = 32;
const MAX_BYTES = 16 * 1024;
const MAX_PER_SECOND = 20;
const STUN: RTCIceServer = { urls: "stun:stun.cloudflare.com:3478" };

type Msg = Record<string, unknown>;
type RTCIceServer = { urls: string | string[]; username?: string; credential?: string };
// A peer is `in` once the host has sent it an offer; `kind` comes from its hello.
type Tag = { role: "host" } | { role: "peer"; id: number; kind?: string; name?: string; in?: boolean };

const cleanName = (value: unknown) => typeof value === "string"
  ? value.replace(/[\x00-\x1f\x7f]/g, "").trim().slice(0, 24) : "";

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    const m = url.pathname.match(/^\/([a-z0-9-]{1,48})(?:\/(host|peer))?\/?$/);
    if (!m) return new Response("not found", { status: 404 });
    if (!m[2]) return env.ASSETS.fetch(new Request(new URL("/", url), req));
    if (req.headers.get("Upgrade") !== "websocket") {
      return new Response("websocket only", { status: 426 });
    }
    return env.ROOM.getByName(m[1]).fetch(req);
  },
} satisfies ExportedHandler<Env>;

// TURN credentials live 24 h; one mint per isolate per hour is plenty.
let turn: { at: number; servers: RTCIceServer[] } | undefined;

export async function iceServers(env: Env): Promise<RTCIceServer[]> {
  // Relay access is opt-in, even if secrets, extra servers or cached credentials exist.
  if (env.TURN_ENABLED !== "true") return [STUN];
  let servers = [STUN];
  if (env.TURN_KEY_ID && env.TURN_KEY_TOKEN) {
    if (!turn || Date.now() - turn.at > 3600_000) {
      const r = await fetch(
        `https://rtc.live.cloudflare.com/v1/turn/keys/${env.TURN_KEY_ID}/credentials/generate-ice-servers`,
        {
          method: "POST",
          headers: { Authorization: `Bearer ${env.TURN_KEY_TOKEN}`, "Content-Type": "application/json" },
          body: JSON.stringify({ ttl: 86400 }),
        },
      ).catch(() => undefined);
      if (r?.ok) {
        const { iceServers } = (await r.json()) as { iceServers: RTCIceServer[] };
        // Browsers refuse port 53, and trying it only costs gathering time.
        const no53 = (u: string) => !/:53(\?|$)/.test(u);
        turn = {
          at: Date.now(),
          servers: iceServers.map((s) => ({ ...s, urls: [s.urls].flat().filter(no53) })),
        };
      } else {
        console.warn("TURN mint failed", r?.status, await r?.text());
      }
    }
    if (turn) servers = turn.servers;
  }
  if (env.EXTRA_ICE) {
    try {
      servers = servers.concat(JSON.parse(env.EXTRA_ICE));
    } catch {
      // A malformed EXTRA_ICE is ignored rather than breaking every room.
    }
  }
  return servers;
}

async function sha256(text: string): Promise<string> {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

const send = (ws: WebSocket | undefined, msg: Msg) => {
  try {
    ws?.send(JSON.stringify(msg));
  } catch {
    // The socket is already closing; its close handler cleans up.
  }
};

// Refuse on an accepted-but-not-hibernated socket, so the client reads the code.
function refuse(code: string): Response {
  const [client, server] = Object.values(new WebSocketPair());
  server.accept();
  send(server, { t: "error", code });
  server.close(4000, code);
  return new Response(null, { status: 101, webSocket: client });
}

export class Room extends DurableObject<Env> {
  // Per-socket rate window. Lost on hibernation, which only happens when idle.
  #rate = new WeakMap<WebSocket, { second: number; count: number }>();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  host(except?: WebSocket) {
    return this.ctx.getWebSockets("host").find((ws) => ws !== except && ws.readyState === WebSocket.OPEN);
  }

  peers(except?: WebSocket) {
    return this.ctx.getWebSockets("peer").filter((ws) => ws !== except && ws.readyState === WebSocket.OPEN);
  }

  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    const isHost = url.pathname.replace(/\/$/, "").endsWith("/host");
    let tags: string[];
    let tag: Tag;
    if (isHost) {
      const key = url.searchParams.get("key") ?? "";
      if (!/^[0-9a-fA-F]{16,128}$/.test(key)) return new Response("bad key", { status: 400 });
      const hash = await sha256(key.toLowerCase());
      const current = this.host();
      if (current && (await this.ctx.storage.get("key")) !== hash) return refuse("taken");
      await this.ctx.storage.put("key", hash);
      current?.close(4001, "replaced");
      tags = ["host"];
      tag = { role: "host" };
    } else {
      if (this.peers().length >= MAX_PEERS) return refuse("full");
      const id = crypto.getRandomValues(new Uint32Array(1))[0];
      tags = ["peer", `id:${id}`];
      tag = { role: "peer", id };
    }
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server, tags);
    server.serializeAttachment(tag);
    // TURN is only needed once the host accepts a browser listener.
    send(server, { t: "ice", servers: [STUN] });
    if (isHost) for (const p of this.peers()) send(p, { t: "host" });
    else if (!this.host()) send(server, { t: "error", code: "no-host" });
    if (isHost) this.roster();
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, data: string | ArrayBuffer) {
    const tag = ws.deserializeAttachment() as Tag;
    const second = Math.floor(Date.now() / 1000);
    const rate = this.#rate.get(ws);
    if (rate?.second === second) {
      // A host negotiates with many peers at once; peer limits stay unchanged.
      if (++rate.count > (tag.role === "host" ? MAX_PER_SECOND * 4 : MAX_PER_SECOND)) return ws.close(1008, "rate");
    } else this.#rate.set(ws, { second, count: 1 });

    if (typeof data !== "string" || data.length > MAX_BYTES) return;
    let msg: Msg;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    if (!msg || typeof msg !== "object") return;

    if (tag.role === "host") {
      const to = this.ctx.getWebSockets(`id:${msg.to}`)[0];
      if (msg.t === "offer" && typeof msg.sdp === "string") {
        if (!to || to.readyState !== WebSocket.OPEN) return;
        const peer = to.deserializeAttachment() as Tag | null;
        const servers = peer?.role === "peer" && peer.kind === "web"
          ? await iceServers(this.env) : undefined;
        // The host or listener may have left while TURN credentials were fetched.
        if (this.host() !== ws || to.readyState !== WebSocket.OPEN) return;
        send(to, { t: "offer", sdp: msg.sdp, ...(servers ? { servers } : {}),
          ...(msg.trickle === true ? { trickle: true } : {}) });
        this.admit(to, true);
      } else if (msg.t === "deny") {
        send(to, { t: "error", code: "denied" });
        this.admit(to, false);
      } else if (msg.t === "candidate" && this.host() === ws
        && to?.readyState === WebSocket.OPEN
        && (to.deserializeAttachment() as { in?: boolean }).in
        && typeof msg.candidate === "string" && msg.candidate.length <= 2048
        && typeof msg.ufrag === "string" && /^[a-zA-Z0-9+/]{4,256}$/.test(msg.ufrag)) {
        send(to, { t: "candidate", candidate: msg.candidate, ufrag: msg.ufrag });
      }
      return;
    }
    const host = this.host();
    if (msg.t === "hello") {
      const { kind, auth } = msg;
      if ((kind !== "web" && kind !== "plugin") || typeof auth !== "string" || !/^[0-9a-f]{16}$/.test(auth)) return;
      ws.serializeAttachment({ ...tag, kind, name: kind === "web" ? cleanName(msg.name) : "" });
      if (host) send(host, { t: "hello", id: tag.id, kind, auth });
      else send(ws, { t: "error", code: "no-host" });
    } else if (msg.t === "name" && tag.kind === "web") {
      const name = cleanName(msg.name);
      if (name !== tag.name) {
        ws.serializeAttachment({ ...tag, name });
        if (tag.in) this.roster();
      }
    } else if (msg.t === "answer" && typeof msg.sdp === "string") {
      send(host, { t: "answer", id: tag.id, sdp: msg.sdp });
    } else if (msg.t === "candidate" && tag.in
      && typeof msg.candidate === "string" && msg.candidate.length <= 2048
      && typeof msg.ufrag === "string" && /^[a-zA-Z0-9+/]{4,256}$/.test(msg.ufrag)) {
      send(host, { t: "candidate", id: tag.id, candidate: msg.candidate, ufrag: msg.ufrag });
    }
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string) {
    this.gone(ws);
    try {
      ws.close(code === 1005 ? 1000 : code, reason);
    } catch {
      // Already closed.
    }
  }

  async webSocketError(ws: WebSocket) {
    this.gone(ws);
  }

  gone(ws: WebSocket) {
    const tag = ws.deserializeAttachment() as Tag | null;
    try {
      ws.serializeAttachment(null); // close and error can both fire; report once
    } catch {
      // Too late to mark; a duplicate leave is harmless.
    }
    if (tag?.role === "peer") {
      send(this.host(), { t: "leave", id: tag.id });
      if (tag.in) this.roster();
    } else if (tag?.role === "host" && !this.host(ws)) {
      for (const p of this.peers()) {
        send(p, { t: "error", code: "no-host" });
        this.admit(p, false, false);
      }
      this.roster();
    }
  }

  admit(ws: WebSocket | undefined, yes: boolean, announce = true) {
    const tag = ws?.deserializeAttachment() as Tag | null;
    if (tag?.role !== "peer" || !!tag.in === yes) return;
    ws!.serializeAttachment({ ...tag, in: yes });
    if (announce) this.roster();
  }

  // Everyone gets who is in: the host (id 0) and every admitted peer, plus
  // `you`, their own id (null for the host).
  roster() {
    const host = this.host();
    const tags = this.peers().map((ws) => [ws, ws.deserializeAttachment() as Tag | null] as const);
    const peers = [
      ...(host ? [{ id: 0, kind: "host" }] : []),
      ...tags.flatMap(([, t]) => (t?.role === "peer" && t.in
        ? [{ id: t.id, kind: t.kind ?? "web", ...(t.name ? { name: t.name } : {}) }] : [])),
    ];
    send(host, { t: "roster", you: null, peers });
    for (const [ws, t] of tags) send(ws, { t: "roster", you: t?.role === "peer" ? t.id : null, peers });
  }
}
