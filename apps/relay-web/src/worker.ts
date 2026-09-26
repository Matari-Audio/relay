// RELAY signaling: one Durable Object per room forwards JSON between the host
// plugin and its peers. See docs/protocol.md. Audio never passes through here.
import { DurableObject } from "cloudflare:workers";

export interface Env {
  ROOM: DurableObjectNamespace<Room>;
  ASSETS: Fetcher;
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
type Tag = { role: "host" } | { role: "peer"; id: number };

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
    const servers = await iceServers(this.env);
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server, tags);
    server.serializeAttachment(tag);
    send(server, { t: "ice", servers });
    if (isHost) for (const p of this.peers()) send(p, { t: "host" });
    else if (!this.host()) send(server, { t: "error", code: "no-host" });
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, data: string | ArrayBuffer) {
    const second = Math.floor(Date.now() / 1000);
    const rate = this.#rate.get(ws);
    if (rate?.second === second) {
      if (++rate.count > MAX_PER_SECOND) return ws.close(1008, "rate");
    } else this.#rate.set(ws, { second, count: 1 });

    if (typeof data !== "string" || data.length > MAX_BYTES) return;
    let msg: Msg;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    if (!msg || typeof msg !== "object") return;
    const tag = ws.deserializeAttachment() as Tag;

    if (tag.role === "host") {
      const to = this.ctx.getWebSockets(`id:${msg.to}`)[0];
      if (msg.t === "offer" && typeof msg.sdp === "string") send(to, { t: "offer", sdp: msg.sdp });
      else if (msg.t === "deny") send(to, { t: "error", code: "denied" });
      return;
    }
    const host = this.host();
    if (msg.t === "hello") {
      const { kind, auth } = msg;
      if ((kind !== "web" && kind !== "plugin") || typeof auth !== "string" || !/^[0-9a-f]{16}$/.test(auth)) return;
      if (host) send(host, { t: "hello", id: tag.id, kind, auth });
      else send(ws, { t: "error", code: "no-host" });
    } else if (msg.t === "answer" && typeof msg.sdp === "string") {
      send(host, { t: "answer", id: tag.id, sdp: msg.sdp });
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
    if (tag?.role === "peer") send(this.host(), { t: "leave", id: tag.id });
    else if (tag?.role === "host" && !this.host(ws)) {
      for (const p of this.peers()) send(p, { t: "error", code: "no-host" });
    }
  }
}
