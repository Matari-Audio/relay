import { DurableObject } from "cloudflare:workers";
import { forceOpusStereo } from "./opus-stereo.mjs";

export interface Env {
  ROOM: DurableObjectNamespace<SessionRoom>;
}

type Claim = {
  name: string;
  port: number;
  lan: string[];
  lanHttp: number;
  mode: string;
  codec: string;
  rate: number;
  deviceRate: number;
  block: number;
  bitrate: number;
  bits: number;
  compression: number;
  pass: string;
  at: number;
};

const MAX_LISTENERS = 10;

const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET,POST,OPTIONS",
  "access-control-allow-headers": "content-type",
};

export class SessionRoom extends DurableObject {
  seq = 0;
  lastAt = 0;
  lastBytes = 0;
  locked = false;
  silent = false;
  claim: Claim | null = null;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.ctx.blockConcurrencyWhile(async () => {
      this.claim = (await this.ctx.storage.get<Claim>("claim")) ?? null;
      this.locked = Boolean(this.claim?.pass);
      this.silent = this.restoreSilent();
    });
  }

  async fetch(request: Request): Promise<Response> {
    try {
      return await this.handle(request);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return new Response(`room: ${message}`, { status: 500, headers: CORS });
    }
  }

  private async handle(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "OPTIONS") {
      return new Response(null, { headers: CORS });
    }
    if (request.headers.get("Upgrade") === "websocket") {
      const pair = new WebSocketPair();
      const server = pair[1];
      const tag = url.pathname.endsWith("/in") ? "in" : "out";
      if (tag === "out" && this.ctx.getWebSockets("out").length >= MAX_LISTENERS) {
        return new Response("room full", { status: 503, headers: CORS });
      }
      this.ctx.acceptWebSocket(server, [tag]);
      if (tag === "out") {
        const id = crypto.randomUUID().slice(0, 8);
        const open = !this.locked || (await this.isOpen());
        server.serializeAttachment({ ok: open, id });
        if (open) {
          this.tellHost(JSON.stringify({ t: "want", id }));
        }
      } else {
        server.serializeAttachment({ silent: this.silent });
        this.askHostForListeners();
      }
      this.broadcastText(await this.roomEvent());
      return new Response(null, { status: 101, webSocket: pair[0] });
    }
    if (request.method === "POST" && url.pathname.endsWith("/claim")) {
      const body = (await request.json()) as Partial<Claim>;
      const claim: Claim = {
        name: String(body.name ?? "").toLowerCase().replace(/[^a-z0-9-]/g, "").slice(0, 48),
        port: Number(body.port) || 17492,
        lan: Array.isArray(body.lan) ? body.lan.map(String).slice(0, 8) : [],
        lanHttp: Number(body.lanHttp) || 8787,
        mode: String(body.mode ?? "opus"),
        codec: String(body.codec ?? body.mode ?? "opus"),
        rate: Number(body.rate) || 48000,
        deviceRate: Number(body.deviceRate) || Number(body.rate) || 48000,
        block: Number(body.block) || 0,
        bitrate: Number(body.bitrate) || 192,
        bits: Number(body.bits) || 16,
        compression: Number(body.compression) || 0,
        pass: String(body.pass ?? "").toLowerCase().replace(/[^a-f0-9]/g, "").slice(0, 64),
        at: Date.now(),
      };
      this.locked = Boolean(claim.pass);
      this.claim = claim;
      await this.ctx.storage.put("claim", claim);
      this.broadcastText(await this.roomEvent());
      return Response.json({ ok: true, locked: this.locked }, { headers: CORS });
    }
    return gone();
  }

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    const tags = this.ctx.getTags(ws);
    if (typeof message === "string") {
      if (tags.includes("in") && this.forwardRtc("in", ws, message)) {
        return;
      }
      if (tags.includes("in") && (await this.handleCtrl(message))) {
        return;
      }
      if (tags.includes("out") && this.forwardRtc("out", ws, message)) {
        return;
      }
      if (tags.includes("out")) {
        const ok = await this.checkPassword(message);
        try {
          ws.send(JSON.stringify({ t: "auth", ok }));
        } catch {
          /* peer gone */
        }
        if (ok) {
          const att = (ws.deserializeAttachment() as { id?: string; ok?: boolean } | null) ?? {};
          ws.serializeAttachment({ ...att, ok: true });
          if (att.id) {
            this.tellHost(JSON.stringify({ t: "want", id: att.id }));
          }
          this.broadcastText(await this.roomEvent());
        }
      }
      return;
    }
    // Binary PCM is LAN-only. Off-LAN media is P2P WebRTC.
  }

  private isLive(): boolean {
    return this.hasHost() && this.listenerCounts().listeners > 0;
  }

  private hasHost(): boolean {
    return this.ctx.getWebSockets("in").length > 0;
  }

  private restoreSilent(): boolean {
    for (const peer of this.ctx.getWebSockets("in")) {
      const att = peer.deserializeAttachment() as { silent?: boolean } | null;
      if (att?.silent) {
        return true;
      }
    }
    return false;
  }

  private setSilent(silent: boolean): void {
    this.silent = silent;
    for (const peer of this.ctx.getWebSockets("in")) {
      const att = (peer.deserializeAttachment() as { silent?: boolean } | null) ?? {};
      try {
        peer.serializeAttachment({ ...att, silent });
      } catch {
        /* hibernation attachment optional */
      }
    }
  }

  private broadcastText(text: string): void {
    for (const peer of this.ctx.getWebSockets()) {
      try {
        peer.send(text);
      } catch {
        /* peer gone */
      }
    }
  }

  private async handleCtrl(raw: string): Promise<boolean> {
    let msg: { t?: string; codec?: string; bitrate?: number; bits?: number; compression?: number; rate?: number; deviceRate?: number; block?: number };
    try {
      msg = JSON.parse(raw) as typeof msg;
    } catch {
      return false;
    }
    if (!msg.t) {
      return false;
    }
    if (msg.t === "dtx") {
      if (this.silent) {
        return true;
      }
      this.setSilent(true);
    } else if (msg.t === "go") {
      if (!this.silent) {
        return true;
      }
      this.setSilent(false);
    } else if (msg.t === "ping") {
      return true;
    } else if (msg.t === "cfg") {
      await this.mergeClaim(msg);
    } else if (msg.t === "stat") {
      this.broadcastText(raw);
      return true;
    } else {
      return false;
    }
    this.broadcastText(await this.roomEvent());
    return true;
  }

  private async mergeClaim(msg: {
    codec?: string;
    bitrate?: number;
    bits?: number;
    compression?: number;
    rate?: number;
    deviceRate?: number;
    block?: number;
    port?: number;
    lanHttp?: number;
  }): Promise<void> {
    const claim = this.claim;
    if (!claim) {
      return;
    }
    if (msg.codec) {
      claim.codec = String(msg.codec);
      claim.mode = claim.codec;
    }
    if (msg.port !== undefined) {
      claim.port = Number(msg.port) || claim.port;
    }
    if (msg.lanHttp !== undefined) {
      claim.lanHttp = Number(msg.lanHttp) || claim.lanHttp;
    }
    if (msg.bitrate !== undefined) {
      claim.bitrate = Number(msg.bitrate) || claim.bitrate;
    }
    if (msg.bits !== undefined) {
      claim.bits = Number(msg.bits) || claim.bits;
    }
    if (msg.compression !== undefined) {
      claim.compression = Number(msg.compression) || 0;
    }
    if (msg.rate !== undefined) {
      claim.rate = Number(msg.rate) || claim.rate;
    }
    if (msg.deviceRate !== undefined) {
      claim.deviceRate = Number(msg.deviceRate) || claim.deviceRate;
    }
    if (msg.block !== undefined) {
      claim.block = Number(msg.block) || 0;
    }
    this.claim = claim;
    await this.ctx.storage.put("claim", claim);
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    const tags = this.ctx.getTags(ws);
    const att = ws.deserializeAttachment() as { id?: string } | null;
    try {
      ws.close(code || 1000, reason);
    } catch {
      /* already closed */
    }
    if (tags.includes("out") && att?.id) {
      this.tellHost(JSON.stringify({ t: "bye", id: att.id }));
    }
    this.broadcastText(await this.roomEvent());
  }

  private tellHost(text: string): void {
    for (const host of this.ctx.getWebSockets("in")) {
      try {
        host.send(text);
      } catch {
        /* host gone */
      }
    }
  }

  /// Listeners that opened `/out` before the plugin `/in` socket still need an
  /// offer. The original `want` was sent into an empty host set and dropped.
  private askHostForListeners(): void {
    for (const peer of this.ctx.getWebSockets("out")) {
      const att = peer.deserializeAttachment() as { id?: string; ok?: boolean } | null;
      if (!att?.ok || !att.id) {
        continue;
      }
      this.tellHost(JSON.stringify({ t: "want", id: att.id }));
    }
  }

  private forwardRtc(from: "in" | "out", ws: WebSocket, raw: string): boolean {
    let msg: { t?: string; id?: string; sdp?: string; cand?: string };
    try {
      msg = JSON.parse(raw) as typeof msg;
    } catch {
      return false;
    }
    if (!msg.t || !["offer", "answer", "ice", "bye", "want"].includes(msg.t)) {
      return false;
    }
    if (from === "out") {
      const att = ws.deserializeAttachment() as { id?: string; ok?: boolean } | null;
      if (!att?.ok || !att.id) {
        return true;
      }
      msg.id = att.id;
      this.tellHost(JSON.stringify(msg));
      return true;
    }
    if (!msg.id) {
      return true;
    }
    for (const peer of this.ctx.getWebSockets("out")) {
      const att = peer.deserializeAttachment() as { id?: string } | null;
      if (att?.id !== msg.id) {
        continue;
      }
      try {
        peer.send(raw);
      } catch {
        /* peer gone */
      }
    }
    return true;
  }

  private async roomEvent(): Promise<string> {
    const counts = this.listenerCounts();
    const host = this.hasHost();
    const live = host && !this.silent && this.isLive();
    return JSON.stringify({
      t: "room",
      claim: await this.publicClaim(),
      listeners: counts.listeners,
      waiting: counts.waiting,
      silent: this.silent,
      host,
      live,
      asleep: host && !live,
    });
  }

  private async isOpen(): Promise<boolean> {
    return !this.claim?.pass;
  }

  private async checkPassword(password: string): Promise<boolean> {
    if (!this.claim?.pass) {
      return true;
    }
    return (await sha256hex(password.trim())) === this.claim.pass;
  }

  private async publicClaim(): Promise<(Omit<Claim, "pass"> & { locked: boolean }) | null> {
    const claim = this.claim;
    if (!claim) {
      return null;
    }
    const { pass, ...rest } = claim;
    return { ...rest, locked: Boolean(pass) };
  }

  private listenerCounts(): { listeners: number; waiting: number } {
    let listeners = 0;
    let waiting = 0;
    for (const peer of this.ctx.getWebSockets("out")) {
      if (this.outOk(peer)) {
        listeners += 1;
      } else {
        waiting += 1;
      }
    }
    return { listeners, waiting };
  }

  private fanout(message: ArrayBuffer): void {
    const framed = wrapFrame(message, this.seq + 1);
    this.seq = framed.seq;
    this.lastBytes = framed.pcmBytes;
    this.lastAt = Date.now();
    for (const peer of this.ctx.getWebSockets("out")) {
      if (!this.outOk(peer)) {
        continue;
      }
      try {
        peer.send(framed.bytes);
      } catch {
        /* listener gone */
      }
    }
  }

  private outOk(peer: WebSocket): boolean {
    const att = peer.deserializeAttachment() as { ok?: boolean } | null;
    if (att?.ok) {
      return true;
    }
    if (!this.locked && att?.ok !== false) {
      if (!att) {
        try {
          peer.serializeAttachment({ ok: true });
        } catch {
          /* hibernation attachment optional */
        }
      }
      return true;
    }
    return false;
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "OPTIONS") {
      return new Response(null, { headers: CORS });
    }
    if (url.pathname === "/" || url.pathname === "/health") {
      return html(indexHtml());
    }
    if (url.pathname === "/api/claim" && request.method === "POST") {
      const body = (await request.json()) as { name?: string; port?: number; lan?: string[]; mode?: string };
      const slug = slugify(body.name ?? "");
      if (!slug) {
        return new Response("bad name", { status: 400, headers: CORS });
      }
      try {
        return await env.ROOM.getByName(slug).fetch(
          new Request("https://room/claim", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ ...body, name: slug }),
          }),
        );
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        return new Response(`claim: ${message}`, { status: 500, headers: CORS });
      }
    }
    const parts = url.pathname.split("/").filter(Boolean);
    const slug = slugify(parts[0] ?? "");
    if (!slug) {
      return new Response("missing name", { status: 404 });
    }
    if (parts[1] === "in" || parts[1] === "out") {
      if (request.headers.get("Upgrade") !== "websocket") {
        return new Response("expected websocket", { status: 426, headers: CORS });
      }
      return env.ROOM.getByName(slug).fetch(request);
    }
    if (parts[1] === "info" || parts[1] === "pcm" || parts[1] === "ctrl" || parts[1] === "unlock") {
      return gone();
    }
    return html(playerHtml(slug));
  },
};

function slugify(raw: string): string {
  return raw.toLowerCase().replace(/[^a-z0-9-]/g, "").slice(0, 48);
}

function html(body: string): Response {
  return new Response(body, {
    headers: {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      // Block Cloudflare's injected insights beacon. Firefox tracking
      // protection fetches an empty body for it, then SRI fails.
      "content-security-policy":
        "default-src 'self'; script-src 'unsafe-inline' 'self' blob:; connect-src 'self' https: wss:; media-src 'self' blob: mediastream:; img-src 'self' data:; style-src 'unsafe-inline' 'self' https://fonts.googleapis.com; font-src https://fonts.gstatic.com data:; worker-src 'self' blob:;",
    },
  });
}

function gone(): Response {
  return new Response("gone — connect over websocket", {
    status: 410,
    headers: {
      ...CORS,
      "cache-control": "public, max-age=86400",
    },
  });
}

async function sha256hex(value: string): Promise<string> {
  const bytes = new TextEncoder().encode(value);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function isFramed(bytes: Uint8Array): boolean {
  return bytes.byteLength >= 5
    && bytes[0] === 0x52
    && bytes[1] === 0x4c
    && bytes[2] === 0x59
    && (bytes[3] === 0x31 || bytes[3] === 0x42 || bytes[3] === 0x4f);
}

function wrapFrame(message: ArrayBuffer, nextSeq: number): { bytes: ArrayBuffer; seq: number; pcmBytes: number } {
  const src = new Uint8Array(message);
  if (isFramed(src)) {
    const seq = new DataView(src.buffer, src.byteOffset, src.byteLength).getUint32(4, true);
    return { bytes: src.slice().buffer, seq, pcmBytes: src.byteLength - 8 };
  }
  const seq = nextSeq >>> 0;
  const out = new Uint8Array(8 + src.byteLength);
  out[0] = 0x52;
  out[1] = 0x4c;
  out[2] = 0x59;
  out[3] = 0x31;
  new DataView(out.buffer).setUint32(4, seq, true);
  out.set(src, 8);
  return { bytes: out.buffer, seq, pcmBytes: src.byteLength };
}

const RELAY_MARK = '<svg class="mark" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" fill="none" stroke="currentColor" stroke-width="13" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="24" cy="50" r="9" fill="currentColor" stroke="none"/><path d="M42 26L56 50L42 74"/><path d="M66 26L80 50L66 74"/></svg>';

function indexHtml(): string {
  return listenPage("session", true);
}

function playerHtml(name: string): string {
  return listenPage(name, false);
}

function listenPage(name: string, landing: boolean): string {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><script>try{document.documentElement.dataset.relayTheme=localStorage.getItem('relay-theme')||'dark'}catch{}</script>
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="relay-listen" content="13">
<meta name="theme-color" content="#191919">
<title>${landing ? "RELAY" : `RELAY · ${name}`}</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Barlow:wght@500;600;700&display=swap" rel="stylesheet">
<style>
:root{color-scheme:dark;--bg:#191919;--chassis:#252525;--surface:#353535;--well:#101010;--hair:#2e2e2e;--text:#ffffff;--muted:#b8b8b8;--accent:#00aaff;--accent-hi:#2bbaff;--ink:#041018;--ok:#5be8b3;--warn:#ffc75c;--hot:#ff7088;--gyr0:#3d8f6a;--trough:#1c1c1c;--r:4px;--rw:3px;--rm:2px;--plate:inset 0 1px 0 #3f3f3f,0 18px 40px #00000073}
html[data-relay-theme=light]{color-scheme:light;--bg:#f4f6f8;--chassis:#ffffff;--surface:#dbe6f2;--well:#edf3fb;--hair:#d9e3ed;--text:#142b3b;--muted:#4c6376;--accent:#0866e8;--accent-hi:#3a8bf0;--ink:#ffffff;--ok:#15836e;--warn:#8a5800;--hot:#c23852;--gyr0:#15836e;--trough:#dce7f3;--plate:inset 0 1px 0 #ffffff,0 18px 40px #142b3b1f}
*{box-sizing:border-box}html,body{margin:0;min-height:100%;background:var(--bg);color:var(--text);font-family:Barlow,system-ui,sans-serif;font-weight:500;font-size:13px;line-height:1.35}body{display:flex;flex-direction:column;min-height:100vh;padding:20px 16px calc(28px + env(safe-area-inset-bottom))}button,input{font:inherit;color:inherit}button,a,input{-webkit-tap-highlight-color:transparent}button{cursor:pointer}a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}input{caret-color:var(--accent)}::selection{background:var(--accent);color:var(--ink)}:focus-visible{outline:2px solid var(--accent);outline-offset:3px;border-radius:2px}.sr-only{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0,0,0,0)}
.wrap{width:min(560px,100%);margin:0 auto;flex:1}.nav{display:flex;align-items:center;gap:10px;min-height:44px}.home-link{display:flex;align-items:center;gap:10px;color:var(--text)}.home-link:hover{text-decoration:none;color:var(--accent)}.mark{width:22px;height:22px;flex-shrink:0}.product{font-size:13px;font-weight:700;letter-spacing:.16em}.nav-link{display:inline-flex;align-items:center;min-height:36px;margin-left:auto;color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.nav-link:hover{color:var(--accent);text-decoration:none}.theme-toggle{display:inline-grid;place-items:center;flex-shrink:0;width:36px;height:36px;padding:8px;border:1px solid var(--hair);border-radius:var(--rw);background:transparent;color:var(--muted)}.theme-toggle svg{width:18px;height:18px}.theme-toggle:hover{color:var(--accent);border-color:var(--accent)}.lamp{width:8px;height:8px;border-radius:50%;background:var(--surface);flex-shrink:0}.lamp[data-state=live]{background:var(--ok)}.lamp[data-state=sleep]{background:var(--warn)}.lamp[data-state=down]{background:var(--hot)}.home .lamp{display:none}
.join-shell{width:min(400px,100%);margin:clamp(24px,8vh,72px) auto 0;padding:20px 18px 16px;border-radius:var(--r);background:var(--chassis);box-shadow:var(--plate)}.join-intro h1{margin:0 0 8px;font-size:22px;font-weight:600;line-height:1.15;letter-spacing:-.02em}.join-intro p{margin:0;color:var(--muted)}.join-caption{display:block;margin-top:12px;color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.join{display:flex;flex-direction:column;margin-top:18px;padding-top:16px;border-top:1px solid var(--hair)}.join label{margin-bottom:6px;color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.join-input{display:flex;align-items:center;gap:8px;border-bottom:1px solid var(--surface)}.join-input:focus-within{border-bottom-color:var(--accent)}.join-input svg{width:16px;height:16px;flex-shrink:0;color:var(--muted)}
.title{min-width:0;width:100%;padding:6px 0;border:0;background:transparent;color:var(--text);font-size:22px;font-weight:600;line-height:1.15;letter-spacing:-.02em;outline:none}.title::placeholder{color:var(--muted)}.who{margin:10px 0 16px;min-height:18px;color:var(--muted)}.hint{margin:8px 0 0;color:var(--muted);font-size:12px}
.open,.gate button,.lock button{display:inline-flex;align-items:center;justify-content:center;gap:10px;min-height:48px;padding:0 22px;border:0;border-radius:var(--r);background:var(--accent);color:var(--ink);font-size:13px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.open:hover,.gate button:hover,.lock button:hover{background:var(--accent-hi)}.open:active,.gate button:active,.lock button:active{transform:translateY(1px)}.open svg{width:16px;height:16px;transition:transform .18s ease-out}.open:hover svg{transform:translateX(3px)}.join-help{margin:16px 0 0;color:var(--muted);font-size:12px}
.session-heading{margin-top:24px}.listen .title{padding-bottom:6px;border-bottom:1px solid var(--surface)}.listen .title:focus{border-bottom-color:var(--accent)}
.lock{display:none;margin:16px 0;padding:16px 12px 14px;border-radius:var(--r);background:var(--well);box-shadow:inset 0 1px 4px #00000059}.lock.show{display:block}.lock label{display:block;margin-bottom:6px;color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.lock input{width:min(240px,100%);min-height:44px;padding:0 10px;border:1px solid var(--hair);border-radius:var(--rw);background:var(--chassis);color:var(--text)}.lock input:focus-visible{border-color:var(--accent)}.lock button{margin-top:12px}.err{margin:10px 0 0;color:var(--hot);font-size:12px}
.stage{position:relative;margin-top:16px;padding:16px 14px 14px;border-radius:var(--r);background:var(--chassis);box-shadow:var(--plate)}.scope-head{display:flex;justify-content:space-between;gap:12px;color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.scope{display:block;width:100%;height:180px;margin:10px 0 8px;border-radius:var(--rw);background:#101010}.scope-idle{margin:0 0 14px;color:var(--muted);font-size:12px}
.desk{display:flex;align-items:stretch;justify-content:center;gap:22px;height:260px;padding:16px 12px 14px;border-radius:var(--r);background:var(--well);box-shadow:inset 0 1px 4px #000000b3}.lane{display:flex;flex-direction:column;align-items:center;gap:8px;width:24px}.clip{order:1;width:8px;height:8px;border-radius:50%;background:var(--surface)}.clip.on{background:var(--hot)}.ch{order:3;color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em}
.rail{position:relative;order:2;width:8px;flex:1;border-radius:var(--rm);overflow:hidden;background:linear-gradient(to top,var(--gyr0) 0%,var(--ok) 58%,var(--warn) 84%,var(--hot) 100%)}.cover{position:absolute;inset:0 0 auto;height:100%;background:var(--trough)}.peak{position:absolute;left:0;right:0;height:1px;background:var(--text);opacity:0}
.fader{display:flex;flex-direction:column;align-items:center;gap:10px;width:56px}.sr-vol{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0,0,0,0);opacity:0}.throw{flex:1;width:56px;touch-action:none;cursor:ns-resize}.slot{position:relative;width:4px;height:100%;margin:0 auto;border-radius:var(--rm);background:var(--surface)}.cap{position:absolute;left:-12px;bottom:0;width:28px;height:12px;border-radius:var(--rm);background:#d8d8d8}.fader:focus-within .slot{box-shadow:0 0 0 2px var(--accent)}#voln{white-space:nowrap;color:var(--muted);font-size:12px;font-variant-numeric:tabular-nums}#mute{width:56px;min-height:44px;border:0;border-radius:var(--rw);background:var(--surface);color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}#mute.on{background:var(--hot);color:var(--ink)}
.gate{display:none;position:absolute;left:14px;right:14px;bottom:14px;height:260px;place-items:center;border-radius:var(--r);background:#101010e6}.gate.show{display:grid}.gate button{min-width:160px}html[data-relay-theme=light] .gate{background:#edf3fbe6}
.tape{margin-top:16px;border-top:1px solid var(--hair);color:var(--muted)}.tape summary{display:flex;align-items:center;min-height:44px;cursor:pointer;font-size:12px;font-variant-numeric:tabular-nums}.log{margin:0 0 14px;padding:12px;border-radius:var(--rw);background:var(--well);max-height:180px;overflow:auto;white-space:pre-wrap;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;font-variant-numeric:tabular-nums}
.listener-footer{display:flex;flex-wrap:wrap;gap:8px 20px;width:min(560px,100%);margin:40px auto 0;padding-top:16px;border-top:1px solid var(--hair);color:var(--muted);font-size:11px;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.listener-footer nav{display:flex;flex-wrap:wrap;gap:8px 20px}.listener-footer a{color:var(--muted)}.listener-footer a:hover{color:var(--accent)}
@media(max-width:600px){body{padding:16px 12px calc(24px + env(safe-area-inset-bottom))}.nav{flex-wrap:wrap}.theme-toggle{width:44px;height:44px}.nav-link{min-height:44px}.join-shell{padding:16px 14px 14px}.stage{padding:14px 12px 12px}.scope{height:140px}.desk{height:232px;gap:14px}.gate{left:12px;right:12px;bottom:12px;height:232px}.rail{width:6px}.lane{width:20px}.fader,.throw{width:48px}}
@media(prefers-reduced-motion:reduce){.open svg{transition:none}}</style>
</head>
<body class="${landing ? "home" : "listen"}">
<main class="wrap">
  <header class="nav">
    <a class="home-link" href="/" aria-label="RELAY home">${RELAY_MARK}<span class="product">RELAY</span></a>
    <a class="nav-link" href="https://matari-audio.com/relay">Get the plugin</a>
    <button class="theme-toggle" type="button" aria-label="Switch to dark mode" aria-pressed="false"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true"><path d="M20 14A8 8 0 0 1 10 4a8 8 0 1 0 10 10Z"/></svg></button><span class="lamp" id="lamp" data-state="wait" aria-hidden="true"></span>
  </header>
  ${landing ? `<section class="join-shell" aria-labelledby="join-heading">
    <div class="join-intro">
      <h1 id="join-heading">Join a session.</h1>
      <p>The sound from their DAW.<br>Right here in your browser.</p>
      <span class="join-caption">No account. No installation.</span>
    </div>
    <form class="join" id="join">
      <label for="title">Session name or link</label>
      <div class="join-input"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true"><path d="m10 13 4-4M8 16l-1 1a4 4 0 0 1-6-6l4-4a4 4 0 0 1 6 0m2 1 1-1a4 4 0 0 1 6 6l-4 4a4 4 0 0 1-6 0" transform="translate(1 0)"/></svg><input id="title" class="title" placeholder="e.g. midnight-studio" autocomplete="off" spellcheck="false" enterkeyhint="go" required aria-describedby="who"></div>
      <p class="who" id="who" role="status">Ask your host for the name or link shown in RELAY.</p>
      <button class="open" type="submit">Listen<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 12h16m-6-6 6 6-6 6"/></svg></button>
      <p class="join-help">Sharing your own audio? <a href="https://matari-audio.com/relay">Get RELAY</a></p>
    </form>
  </section>` : `<div class="session-heading"><label class="sr-only" for="title">Session name</label>
  <input id="title" class="title" value="${name}" spellcheck="false" autocomplete="off" enterkeyhint="go" aria-describedby="titleHint">
  <p class="hint" id="titleHint">Type another name and press Enter to jump.</p>
  <p class="who" id="who" role="status" aria-live="polite">Waiting for the host</p>
  <p class="hint" id="lanHint" hidden></p></div>
  <form class="lock" id="lock">
    <label for="pw">Password</label>
    <input id="pw" type="password" autocomplete="current-password" />
    <button type="submit">Unlock</button>
    <p class="err" id="pwerr"></p>
  </form>
  <div class="stage">
    <div class="scope-head"><span>Incoming audio</span><span>Stereo · L / R</span></div>
    <canvas id="scope" class="scope" width="1056" height="260" aria-label="Live incoming audio level history"></canvas>
    <p class="scope-idle" id="scope-note">The signal appears here when your host starts playing.</p>
    <div class="desk" role="group" aria-label="Listen levels">
      <div class="lane">
        <span class="ch">L</span>
        <i class="clip" id="clipL"></i>
        <div class="rail" id="railL" role="meter" aria-label="Left" aria-valuemin="-60" aria-valuemax="0" aria-valuenow="-60" aria-valuetext="silent">
          <div class="cover" id="coverL"></div>
          <div class="peak" id="holdL"></div>
        </div>
      </div>
      <div class="fader">
        <input id="vol" class="sr-vol" type="range" min="0" max="1" step="0.01" value="1" orient="vertical" aria-label="Volume">
        <div class="throw" id="throw">
          <div class="slot" id="slot"><div class="cap" id="cap"></div></div>
        </div>
        <span id="voln">0.0 dB</span>
        <button type="button" id="mute" aria-pressed="false">Mute</button>
      </div>
      <div class="lane">
        <span class="ch">R</span>
        <i class="clip" id="clipR"></i>
        <div class="rail" id="railR" role="meter" aria-label="Right" aria-valuemin="-60" aria-valuemax="0" aria-valuenow="-60" aria-valuetext="silent">
          <div class="cover" id="coverR"></div>
          <div class="peak" id="holdR"></div>
        </div>
      </div>
    </div>
    <div class="gate show" id="gate" role="dialog" aria-modal="true" aria-labelledby="go"><button id="go" type="button">Listen</button></div>
  </div>
  <details class="tape">
    <summary id="tapeLine">Waiting for the host</summary>
    <pre class="log" id="log" aria-hidden="true"></pre>
  </details>`}
<footer class="listener-footer"><span>RELAY by Matari Audio</span><nav><a href="https://matari-audio.com/relay">About RELAY</a><a href="https://matari-audio.com/support">Support</a><a href="https://matari-audio.com/privacy">Privacy</a></nav></footer>
</main><script>(()=>{const b=document.querySelector('.theme-toggle');if(!b)return;function sync(){const dark=document.documentElement.dataset.relayTheme==='dark';b.setAttribute('aria-pressed',String(dark));b.setAttribute('aria-label',dark?'Switch to light mode':'Switch to dark mode')}sync();b.addEventListener('click',()=>{const next=document.documentElement.dataset.relayTheme==='dark'?'light':'dark';document.documentElement.dataset.relayTheme=next;try{localStorage.setItem('relay-theme',next)}catch{}sync()})})();</script>
<audio id="spkr" playsinline webkit-playsinline autoplay aria-hidden="true"></audio>
${landing ? `<script>
const join = document.getElementById('join');
const title = document.getElementById('title');
title.oninput = () => {
  title.removeAttribute('aria-invalid');
  document.getElementById('who').textContent = 'Ask your host for the name or link shown in RELAY.';
};
join.onsubmit = (ev) => {
  ev.preventDefault();
  let value = title.value.trim();
  try {
    if (/^https?:/i.test(value)) {
      const link = new URL(value);
      if (link.hostname !== location.hostname && link.hostname !== 'relay.matari-audio.com') throw new Error();
      value = link.pathname.slice(1);
    }
    if (!/^[a-z0-9-]{1,48}$/i.test(value)) throw new Error();
    location.assign('/' + value.toLowerCase());
  } catch {
    document.getElementById('who').textContent = 'Enter a RELAY session name or a valid session link.';
    title.setAttribute('aria-invalid', 'true');
    title.focus();
  }
};
title.focus();
</script>` : `<script>
const name = ${JSON.stringify(name)};
const logEl = document.getElementById('log');
const tapeLine = document.getElementById('tapeLine');
const logLines = [];
function log(msg) {
  const line = new Date().toISOString().slice(11, 23) + ' ' + String(msg);
  logLines.push(line);
  if (logLines.length > 40) logLines.shift();
  if (logEl) {
    logEl.textContent = logLines.join('\\n');
    logEl.scrollTop = logEl.scrollHeight;
  }
  if (tapeLine) tapeLine.textContent = String(msg);
  console.log('[relay]', msg);
}
function iceKind(cand) {
  const t = String(cand || '');
  if (t.indexOf(' typ srflx') >= 0) return 'srflx';
  if (t.indexOf(' typ relay') >= 0) return 'relay';
  if (t.indexOf(' typ prflx') >= 0) return 'prflx';
  if (t.indexOf(' typ host') >= 0) return 'host';
  return t ? 'ice' : 'end';
}
function sdpSummary(sdp) {
  const lines = String(sdp || '').split(/\\r?\\n/);
  const bits = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line.indexOf('m=') === 0 || line.indexOf('a=rtpmap:') === 0 || line.indexOf('a=sendonly') === 0 || line.indexOf('a=recvonly') === 0 || line.indexOf('a=sendrecv') === 0 || (line.indexOf('a=fmtp:') === 0 && /stereo/i.test(line))) {
      bits.push(line);
    }
  }
  return bits.join(' ') || 'sdp';
}
var forceOpusStereo = ${forceOpusStereo.toString()};
function listenTargetSec(claim) {
  const device = Number(claim && claim.deviceRate) || Number(claim && claim.rate) || 48000;
  const block = Number(claim && claim.block) || 0;
  const dawMs = block > 0 ? (block * 1000 / device) : 20;
  return Math.min(0.16, Math.max(0.04, (dawMs * 2 + 20) / 1000));
}
function linToDb(v) {
  return v < 1e-6 ? -60 : Math.max(-60, Math.min(0, 20 * Math.log10(v)));
}
function dbToPos(db) {
  return Math.max(0, Math.min(1, (db + 60) / 60));
}
const coverL = document.getElementById('coverL');
const coverR = document.getElementById('coverR');
const holdLel = document.getElementById('holdL');
const holdRel = document.getElementById('holdR');
const railL = document.getElementById('railL');
const railR = document.getElementById('railR');
const clipL = document.getElementById('clipL');
const clipR = document.getElementById('clipR');
const who = document.getElementById('who');
const lamp = document.getElementById('lamp');
const vol = document.getElementById('vol');
const voln = document.getElementById('voln');
const gate = document.getElementById('gate');
const titleEl = document.getElementById('title');
let ctx = null;
let gainNode = null;
let lastSeq = -1;
let rate = 48000;
const holdL = { p: 0, a: 0 };
const holdR = { p: 0, a: 0 };
const pending = [];
function parseFrame(buf) {
  const bytes = new Uint8Array(buf);
  if (bytes.byteLength >= 8 && bytes[0] === 0x52 && bytes[1] === 0x4c && bytes[2] === 0x59 && bytes[3] === 0x31) {
    const seq = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(4, true);
    return [{ kind: 'pcm', seq, pcm: bytes.buffer.slice(bytes.byteOffset + 8, bytes.byteOffset + bytes.byteLength) }];
  }
  if (bytes.byteLength >= 5 && bytes[0] === 0x52 && bytes[1] === 0x4c && bytes[2] === 0x59 && bytes[3] === 0x42) {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const parts = [];
    let off = 5;
    const n = bytes[4];
    for (let i = 0; i < n && off + 9 <= bytes.byteLength; i++) {
      const kind = bytes[off] === 2 ? 'opus' : 'pcm';
      const seq = view.getUint32(off + 1, true);
      const len = view.getUint32(off + 5, true);
      off += 9;
      if (off + len > bytes.byteLength) break;
      const payload = bytes.buffer.slice(bytes.byteOffset + off, bytes.byteOffset + off + len);
      parts.push({ kind, seq, pcm: payload });
      off += len;
    }
    return parts;
  }
  return [{ kind: 'pcm', seq: lastSeq + 1, pcm: buf }];
}
function decodeI16(buf) {
  const view = new DataView(buf);
  const n = view.byteLength >> 1;
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) out[i] = view.getInt16(i * 2, true) / 32767;
  return out;
}
let opusDec = null;
let opusTs = 0;
let opusReady = false;
function resetOpus() {
  if (opusDec) try { opusDec.close(); } catch (e) {}
  opusDec = null;
  opusTs = 0;
  opusReady = false;
}
function armOpus() {
  if (opusDec || typeof AudioDecoder === 'undefined') return;
  try {
    opusDec = new AudioDecoder({
      output: (audio) => {
        const frames = audio.numberOfFrames;
        const l = new Float32Array(frames);
        const r = new Float32Array(frames);
        audio.copyTo(l, { planeIndex: 0, format: 'f32-planar' });
        audio.copyTo(r, { planeIndex: audio.numberOfChannels > 1 ? 1 : 0, format: 'f32-planar' });
        audio.close();
        const interleaved = new Float32Array(frames * 2);
        for (let i = 0; i < frames; i++) {
          interleaved[i * 2] = l[i];
          interleaved[i * 2 + 1] = r[i];
        }
        pushSamples(interleaved, 48000);
      },
      error: () => { resetOpus(); }
    });
    opusDec.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 });
    opusReady = true;
  } catch (e) {
    resetOpus();
  }
}
function decodeOpus(packet) {
  armOpus();
  if (!opusDec || !opusReady) return;
  try {
    opusDec.decode(new EncodedAudioChunk({
      type: 'key',
      timestamp: opusTs,
      duration: 20_000,
      data: packet
    }));
    opusTs += 20_000;
  } catch (e) {
    resetOpus();
  }
}
function bumpHold(hold, peak) {
  if (peak >= hold.p) { hold.p = peak; hold.a = 0; }
  else {
    hold.a += 0.04;
    if (hold.a > 0.9) hold.p *= 0.82;
  }
  return hold.p;
}
function paintRail(cover, peakEl, rail, clip, hold, peak) {
  const db = linToDb(peak);
  const held = linToDb(bumpHold(hold, peak));
  const pos = dbToPos(db);
  if (cover) cover.style.height = ((1 - pos) * 100) + '%';
  if (peakEl) {
    peakEl.style.bottom = (dbToPos(held) * 100) + '%';
    peakEl.style.opacity = held <= 1e-6 ? '0' : '1';
  }
  if (clip) clip.classList.toggle('on', peak >= 0.89);
  if (rail) {
    rail.setAttribute('aria-valuenow', String(Math.round(db)));
    rail.setAttribute('aria-valuetext', db <= -59 ? 'silent' : Math.round(db) + ' dB');
  }
}
const scope = document.getElementById('scope');
const scopeContext = scope.getContext('2d');
const scopeHistory = new Float32Array(240);
let scopeAt = 0, scopeLast = 0;
function drawScope(left, right) {
  if (document.hidden || !scopeContext) return;
  const now = performance.now();
  if (now - scopeLast < 40) return;
  scopeLast = now;
  scopeHistory[scopeAt * 2] = Math.max(0, Math.min(1, Number.isFinite(left) ? left : 0));
  scopeHistory[scopeAt * 2 + 1] = Math.max(0, Math.min(1, Number.isFinite(right) ? right : 0));
  scopeAt = (scopeAt + 1) % 120;
  const g = scopeContext, w = scope.width, h = scope.height;
  g.clearRect(0, 0, w, h);
  for (let channel = 0; channel < 2; channel++) {
    const center = h * (channel ? 0.72 : 0.28);
    g.strokeStyle = '#ffffff30'; g.lineWidth = 1;
    g.beginPath(); g.moveTo(0, center); g.lineTo(w, center); g.stroke();
    g.fillStyle = channel ? '#90c6ff' : '#ffffff';
    for (let i = 0; i < 120; i++) {
      const level = scopeHistory[((scopeAt + i) % 120) * 2 + channel];
      const height = Math.max(2, Math.sqrt(level) * h * 0.37);
      g.fillRect(i * w / 120, center - height / 2, w / 120 * 0.58, height);
    }
  }
  document.getElementById('scope-note').textContent = left > 0.001 || right > 0.001 ? 'Live input · stereo level history' : 'Listening for audio from your host.';
}
drawScope(0, 0);
function setMeterPeak(peak) {
  drawScope(peak, peak);
  pluginPeak = peak;
  paintRail(coverL, holdLel, railL, clipL, holdL, peak);
  paintRail(coverR, holdRel, railR, clipR, holdR, peak);
}
function setMeterLR(l, r) {
  drawScope(l, r);
  if (l < 0.002 && pluginPeak > l) l = pluginPeak;
  if (r < 0.002 && pluginPeak > r) r = pluginPeak;
  paintRail(coverL, holdLel, railL, clipL, holdL, l);
  paintRail(coverR, holdRel, railR, clipR, holdR, r);
}
function setMeter(samples) {
  let l = 0, r = 0;
  for (let i = 0; i + 1 < samples.length; i += 2) {
    l = Math.max(l, Math.abs(samples[i]));
    r = Math.max(r, Math.abs(samples[i + 1]));
  }
  setMeterLR(l, r);
}
let node = null;
let targetSec = 0.04;
let socket = null;
let dtxMode = false;
let lastCfg = '';
let secret = '';
function resamplePlanar(interleaved, srcRate, dstRate) {
  const srcFrames = interleaved.length >> 1;
  if (!srcFrames) return { l: new Float32Array(0), r: new Float32Array(0) };
  const dstFrames = Math.max(1, Math.round(srcFrames * dstRate / srcRate));
  const l = new Float32Array(dstFrames);
  const r = new Float32Array(dstFrames);
  for (let i = 0; i < dstFrames; i++) {
    const srcPos = i * srcRate / dstRate;
    const j = Math.min(srcFrames - 1, srcPos | 0);
    const f = srcPos - j;
    const j2 = Math.min(srcFrames - 1, j + 1);
    l[i] = interleaved[j * 2] * (1 - f) + interleaved[j2 * 2] * f;
    r[i] = interleaved[j * 2 + 1] * (1 - f) + interleaved[j2 * 2 + 1] * f;
  }
  return { l, r };
}
function takeSeq(seq) {
  if (lastSeq >= 0 && seq <= lastSeq) {
    if (lastSeq - seq > 32) lastSeq = seq;
    else return false;
  } else {
    lastSeq = seq;
  }
  return true;
}
function fadeIn(samples) {
  const frames = samples.length >> 1;
  const n = Math.min(frames, Math.round(0.02 * rate));
  for (let i = 0; i < n; i++) {
    const t = i / Math.max(1, n - 1);
    const g = 0.5 - 0.5 * Math.cos(Math.PI * t);
    samples[i * 2] *= g;
    samples[i * 2 + 1] *= g;
  }
}
function pushSamples(samples, srcRate) {
  setMeter(samples);
  if (!ctx || !node) {
    pending.push(samples);
    if (pending.length > 24) pending.shift();
    return;
  }
  node.port.postMessage(resamplePlanar(samples, srcRate, ctx.sampleRate));
}
function enqueue(buf, srcRate, resume) {
  let first = !!resume;
  for (const frame of parseFrame(buf)) {
    if (!takeSeq(frame.seq)) continue;
    if (frame.kind === 'opus') {
      decodeOpus(frame.pcm, srcRate);
      first = false;
      continue;
    }
    const samples = decodeI16(frame.pcm);
    if (first) { fadeIn(samples); first = false; }
    pushSamples(samples, srcRate);
  }
}
let lanShown = false;
let gotPcm = false;
let browserDrops = 0;
let lastRoom = { host: false, live: false, silent: false, asleep: false, listeners: 0, dropouts: 0, peers: 0, claimLocked: false };
function renderWho() {
  let line = 'Waiting for the host';
  let state = 'wait';
  if (lastRoom.claimLocked && !secret) {
    line = 'Password required';
    state = 'down';
  } else if (!lastRoom.host) {
    line = 'No host on this name';
    state = 'down';
  } else if (lastRoom.asleep || lastRoom.silent) {
    line = 'Host asleep';
    state = 'sleep';
  } else if (pc && pc.connectionState === 'connecting') {
    line = 'Connecting';
    state = 'wait';
  } else if ((pc && pc.connectionState === 'connected') || lastRoom.live || gotPcm) {
    line = 'Live';
    state = 'live';
  } else if (lastRoom.host) {
    line = 'Host ready';
    state = 'wait';
  }
  const extra = [];
  if (lastRoom.listeners > 1) extra.push(lastRoom.listeners + ' listening');
  const drops = Math.max(browserDrops, Number(lastRoom.dropouts) || 0);
  if (drops) extra.push(drops + ' dropouts');
  if (who) who.textContent = extra.length ? line + ' · ' + extra.join(' · ') : line;
  if (lamp) lamp.setAttribute('data-state', state);
}
function maybeLan(claim) {
  const hint = document.getElementById('lanHint');
  if (!hint || lanShown || !claim || !claim.lan || !claim.lan.length) return;
  lanShown = true;
  const port = Number(claim.lanHttp) || 8787;
  // ponytail: an https page cannot probe a plain-http LAN address — mixed
  // content blocks every subresource — so offer the link and let the
  // listener pick. Auto-redirect again if the LAN server ever serves https.
  hint.textContent = 'Same network as the host? Listen direct: ';
  claim.lan.slice(0, 4).forEach((ip, i) => {
    if (i) hint.appendChild(document.createTextNode(' · '));
    const a = document.createElement('a');
    a.href = 'http://' + ip + ':' + port + '/' + encodeURIComponent(name);
    a.rel = 'noopener';
    a.textContent = ip;
    hint.appendChild(a);
  });
  hint.hidden = false;
}
function flush(srcRate) {
  if (!ctx || !node) return;
  while (pending.length) {
    node.port.postMessage(resamplePlanar(pending.shift(), srcRate, ctx.sampleRate));
  }
}
function resetListen(claim) {
  lastSeq = -1;
  pending.length = 0;
  resetOpus();
  if (node) node.port.postMessage({ clear: 1 });
  if (claim) applyClaim(claim);
}
function applyClaim(claim) {
  if (!claim || !claim.name) return;
  rate = Number(claim.rate) || 48000;
  targetSec = listenTargetSec(claim);
  if (node && ctx) node.port.postMessage({ target: Math.round(targetSec * ctx.sampleRate) });
}
function onCtrl(raw) {
  let msg;
  try { msg = JSON.parse(raw); } catch (e) { log('bad signal'); return; }
  if (msg.t === 'stat') {
    lastRoom.dropouts = Number(msg.dropouts) || lastRoom.dropouts;
    lastRoom.peers = Number(msg.peers) || lastRoom.peers;
    lastRoom.listeners = Number(msg.web || msg.lan || msg.listeners) || lastRoom.listeners;
    if (typeof msg.peak === 'number') {
      pluginPeak = msg.peak;
      setMeterPeak(msg.peak);
      if (msg.peak > 0.002 && (lastRoom.silent || lastRoom.asleep)) {
        lastRoom.silent = false;
        lastRoom.asleep = false;
        lastRoom.live = true;
        lastRoom.host = true;
        log('host audio');
        kickPlayback();
      }
    }
    const key = String(msg.ready) + ':' + (msg.peers|0) + ':' + Math.floor(Number(msg.sent) / 50) + ':' + (msg.peak > 0.002 ? 'hot' : 'quiet');
    if (key !== lastStatLog) {
      lastStatLog = key;
      log('host ready=' + (msg.ready|0) + ' peers=' + (msg.peers|0) + ' sent=' + (msg.sent|0) + ' peak=' + Number(msg.peak || 0).toFixed(3));
    }
    renderWho();
    return;
  }
  if (msg.t === 'room') {
    const hadHost = lastRoom.host;
    lastRoom = {
      host: !!msg.host,
      live: !!msg.live,
      silent: !!msg.silent,
      asleep: !!msg.asleep,
      listeners: Number(msg.listeners) || 0,
      dropouts: Number(msg.dropouts) || lastRoom.dropouts,
      peers: Number(msg.peers) || lastRoom.peers,
      claimLocked: !!(msg.claim && msg.claim.locked),
    };
    if (msg.asleep || msg.silent) dtxMode = true;
    log(msg.host ? (msg.silent ? 'host silent' : 'host on') : 'no host');
    if (lastRoom.host && !hadHost) requestOffer('host on');
    renderWho();
    if (msg.claim) {
      maybeLan(msg.claim);
      const nextRate = Number(msg.claim.rate) || 48000;
      if (lastCfg && nextRate !== rate) resetListen(msg.claim);
      else applyClaim(msg.claim);
      lastCfg = String(nextRate);
    }
    const lock = document.getElementById('lock');
    if (msg.claim && msg.claim.locked && lock && !secret) {
      lock.classList.add('show');
      log('password required');
      renderWho();
    }
    return;
  }
  if (msg.t === 'auth') {
    const lock = document.getElementById('lock');
    const pwerr = document.getElementById('pwerr');
    if (msg.ok) {
      if (lock) lock.classList.remove('show');
      if (pwerr) pwerr.textContent = '';
      log('unlocked');
      requestOffer('unlocked');
    } else {
      secret = '';
      if (lock) lock.classList.add('show');
      if (pwerr) pwerr.textContent = 'Wrong password';
      log('wrong password');
    }
    renderWho();
    return;
  }
  if (msg.t === 'dtx') {
    dtxMode = true;
    lastRoom.silent = true;
    lastRoom.asleep = true;
    log('host silent');
    renderWho();
    return;
  }
  if (msg.t === 'go') {
    lastRoom.silent = false;
    lastRoom.asleep = false;
    lastRoom.live = true;
    lastRoom.host = true;
    log('host audio');
    kickPlayback();
    return;
  }
  if (msg.t === 'cfg') {
    const nextRate = Number(msg.rate) || 48000;
    const next = Object.assign({ name: name }, msg);
    if (lastCfg && nextRate !== rate) resetListen(next);
    else applyClaim(next);
    lastCfg = String(nextRate);
    log('cfg ' + nextRate + ' Hz');
    return;
  }
  if (msg.t === 'offer' && msg.sdp) {
    log('offer ' + sdpSummary(msg.sdp));
    if (pc && pc.remoteDescription) {
      const ice = pc.iceConnectionState;
      if (ice !== 'failed' && ice !== 'closed') {
        log('offer ignored — ice ' + ice);
        return;
      }
      log('new offer — reset peer');
      dropPc();
    }
    pendingOffer = msg.sdp;
    acceptOffer(msg.sdp).catch(() => { log('offer failed'); who.textContent = 'Offer failed'; });
    return;
  }
  if (msg.t === 'ice' && msg.cand) {
    log('host ice ' + iceKind(msg.cand));
    const cand = { candidate: msg.cand, sdpMid: msg.mid || '0' };
    if (pc && pc.remoteDescription) {
      pc.addIceCandidate(cand).catch(() => { log('ice dropped'); });
    } else {
      pendingIce.push(cand);
    }
    return;
  }
  if (msg.t === 'bye') {
    log('host bye');
    dropPc();
  }
}
let pc = null;
let pendingOffer = null;
let pendingIce = [];
let hooked = false;
let armed = false;
let analyserL = null;
let analyserR = null;
let meterRaf = 0;
let statsTimer = 0;
let pluginPeak = 0;
let lastStatLog = '';
let offerWatch = 0;
let wantLock = 0;
let takingOffer = false;
const speaker = document.getElementById('spkr');
function kickPlayback() {
  speaker.muted = false;
  if (speaker.srcObject) {
    const tracks = speaker.srcObject.getAudioTracks ? speaker.srcObject.getAudioTracks() : [];
    for (let i = 0; i < tracks.length; i++) tracks[i].enabled = true;
    speaker.play().catch(() => {});
  }
  renderWho();
}
function hasRemote() {
  return !!(pc && pc.remoteDescription);
}
function sendWant(reason) {
  log('want ' + reason);
  if (!socket || socket.readyState !== 1) return false;
  try { socket.send(JSON.stringify({ t: 'want' })); } catch (e) { return false; }
  return true;
}
function sendBye() {
  if (!socket || socket.readyState !== 1) return;
  try { socket.send(JSON.stringify({ t: 'bye' })); } catch (e) {}
}
function armOfferWatch() {
  clearTimeout(offerWatch);
  offerWatch = setTimeout(() => {
    if (hasRemote() || pendingOffer) return;
    if (sendWant('no offer')) armOfferWatch();
  }, 4000);
}
function requestOffer(reason) {
  const now = Date.now();
  if (now - wantLock < 800) return;
  wantLock = now;
  const retry = reason === 'ice failed' || reason === 'ice stuck' || reason === 'peer failed' || reason === 'peer stuck';
  if (retry) {
    dropPc();
    sendBye();
  } else if (hasRemote() || pendingOffer || pc) {
    return;
  }
  sendWant(reason);
  armOfferWatch();
}
const cap = document.getElementById('cap');
const slot = document.getElementById('slot');
const throwEl = document.getElementById('throw');
const muteBtn = document.getElementById('mute');
let preMute = 1;
function placeCap(v) {
  if (!cap || !slot) return;
  const travel = Math.max(0, slot.clientHeight - cap.offsetHeight);
  cap.style.bottom = (v * travel) + 'px';
}
function applyVol() {
  const v = Math.max(0, Math.min(1, Number(vol.value)));
  speaker.volume = v;
  if (gainNode && ctx) {
    gainNode.gain.setTargetAtTime(v, ctx.currentTime, 0.012);
  }
  const label = v <= 0 ? '−∞ dB' : (20 * Math.log10(v)).toFixed(1) + ' dB';
  voln.textContent = label;
  vol.setAttribute('aria-valuetext', label);
  placeCap(v);
  if (muteBtn) {
    const on = v <= 0;
    muteBtn.classList.toggle('on', on);
    muteBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
}
function snapshotRelay() {
  window.relay = {
    pc,
    armed,
    hooked,
    gotPcm,
    hasOffer: !!pendingOffer,
    hasRemote: !!(pc && pc.remoteDescription),
    conn: pc ? pc.connectionState : null,
    ice: pc ? pc.iceConnectionState : null,
    cover: coverL ? coverL.style.height : '',
    coverL: coverL ? coverL.style.height : '',
    coverR: coverR ? coverR.style.height : '',
    ctx: ctx ? ctx.state : null,
    logs: logLines.slice(),
  };
}
function startStatsMeter() {
  if (statsTimer) return;
  const poll = async () => {
    statsTimer = 0;
    if (!pc) return;
    startStatsMeter();
    snapshotRelay();
    if (analyserL) return;
    try {
      const stats = await pc.getStats();
      let peak = 0;
      stats.forEach((r) => {
        if (typeof r.audioLevel === 'number') peak = Math.max(peak, r.audioLevel);
      });
      setMeterPeak(peak);
    } catch (e) {}
  };
  statsTimer = setTimeout(poll, 80);
}
function attachStream(stream) {
  speaker.srcObject = stream;
  speaker.muted = !armed;
  speaker.playsInline = true;
  speaker.play().catch(() => {});
  gotPcm = true;
  lastRoom.live = true;
  lastRoom.host = true;
  hooked = true;
  log('audio attached');
  renderWho();
  startStatsMeter();
  snapshotRelay();
  if (armed) wirePlayback(stream);
}
function tapIsUnsafe() {
  const ua = String(navigator.userAgent || '');
  if (/Mobile|Android|iPhone|iPad|iPod/.test(ua)) return true;
  if (/\\b(Chrome|CriOS|Edg)\\/\\d/.test(ua) && ua.indexOf('Firefox') < 0) return true;
  return false;
}
function wirePlayback(stream) {
  speaker.srcObject = stream;
  speaker.muted = false;
  speaker.playsInline = true;
  applyVol();
  speaker.play().catch(() => {});
  if (tapIsUnsafe()) return;
  if (!ctx) {
    const AC = window.AudioContext || window.webkitAudioContext;
    if (AC) ctx = new AC();
  }
  if (!ctx) return;
  try {
    if (!analyserL) {
      const tap = stream.clone ? stream.clone() : stream;
      const src = ctx.createMediaStreamSource(tap);
      const split = ctx.createChannelSplitter(2);
      src.connect(split);
      analyserL = ctx.createAnalyser();
      analyserR = ctx.createAnalyser();
      analyserL.fftSize = 256;
      analyserR.fftSize = 256;
      split.connect(analyserL, 0);
      split.connect(analyserR, 1);
      if (!meterRaf) meterRaf = requestAnimationFrame(tickMeter);
    }
  } catch (e) {}
}
function peakOf(an) {
  const bins = new Uint8Array(an.frequencyBinCount);
  an.getByteTimeDomainData(bins);
  let peak = 0;
  for (let i = 0; i < bins.length; i++) {
    peak = Math.max(peak, Math.abs((bins[i] - 128) / 128));
  }
  return peak;
}
function tickMeter() {
  meterRaf = requestAnimationFrame(tickMeter);
  if (!analyserL || !analyserR) return;
  setMeterLR(peakOf(analyserL), peakOf(analyserR));
}
function ensurePc() {
  if (pc) return pc;
  pc = new RTCPeerConnection({ iceServers: [{ urls: 'stun:stun.cloudflare.com:3478' }] });
  pc.ontrack = (ev) => {
    if (ev.track.kind !== 'audio') return;
    ev.track.enabled = true;
    const ch = (ev.track.getSettings && ev.track.getSettings().channelCount) || '?';
    log('track audio' + (ev.track.muted ? ' muted' : '') + ' ch=' + ch);
    attachStream(ev.streams[0] || new MediaStream([ev.track]));
  };
  pc.onicecandidate = (ev) => {
    if (!ev.candidate || !ev.candidate.candidate || !socket || socket.readyState !== 1) {
      if (ev && ev.candidate && !ev.candidate.candidate) log('local ice end');
      return;
    }
    log('local ice ' + iceKind(ev.candidate.candidate));
    socket.send(JSON.stringify({ t: 'ice', cand: ev.candidate.candidate, mid: ev.candidate.sdpMid }));
  };
  pc.oniceconnectionstatechange = () => {
    const peer = pc;
    if (!peer) return;
    log('ice ' + peer.iceConnectionState);
    if (peer.iceConnectionState === 'failed') {
      requestOffer('ice failed');
      return;
    }
    if (peer.iceConnectionState === 'connected' || peer.iceConnectionState === 'completed') {
      lastRoom.live = true;
      lastRoom.host = true;
      renderWho();
    }
    if (peer.iceConnectionState === 'disconnected') {
      const seen = peer;
      setTimeout(() => {
        if (pc === seen && seen.iceConnectionState === 'disconnected') requestOffer('ice stuck');
      }, 8000);
    }
  };
  pc.onconnectionstatechange = () => {
    snapshotRelay();
    const peer = pc;
    if (!peer) return;
    log('peer ' + peer.connectionState);
    if (peer.connectionState === 'disconnected') {
      const seen = peer;
      setTimeout(() => {
        if (pc === seen && seen.connectionState === 'disconnected') requestOffer('peer stuck');
      }, 8000);
      return;
    }
    if (peer.connectionState === 'failed') {
      requestOffer('peer failed');
      return;
    }
    if (peer.connectionState === 'connected' || peer.connectionState === 'connecting') {
      lastRoom.live = peer.connectionState === 'connected';
      lastRoom.host = true;
      renderWho();
    }
  };
  snapshotRelay();
  return pc;
}
function dropPc() {
  const old = pc;
  pc = null;
  pendingOffer = null;
  pendingIce = [];
  takingOffer = false;
  hooked = false;
  analyserL = null;
  analyserR = null;
  if (meterRaf) {
    cancelAnimationFrame(meterRaf);
    meterRaf = 0;
  }
  speaker.srcObject = null;
  if (old) try { old.close(); } catch (e) {}
  snapshotRelay();
}
async function acceptOffer(sdp) {
  clearTimeout(offerWatch);
  if (takingOffer) return;
  takingOffer = true;
  try {
    const peer = ensurePc();
    if (peer.remoteDescription) return;
    await peer.setRemoteDescription({ type: 'offer', sdp: forceOpusStereo(sdp) });
    pendingOffer = null;
    const held = pendingIce.splice(0);
    log('answer + ' + held.length + ' held ice');
    for (let i = 0; i < held.length; i++) {
      peer.addIceCandidate(held[i]).catch(() => {});
    }
    const answer = await peer.createAnswer();
    answer.sdp = forceOpusStereo(answer.sdp);
    await peer.setLocalDescription(answer);
    log('answer ' + sdpSummary(answer.sdp));
    if (socket && socket.readyState === 1) {
      socket.send(JSON.stringify({ t: 'answer', sdp: answer.sdp }));
    }
    snapshotRelay();
  } finally {
    takingOffer = false;
  }
}
async function armAudio() {
  armed = true;
  gate.classList.remove('show');
  vol.tabIndex = 0;
  if (muteBtn) muteBtn.tabIndex = 0;
  speaker.muted = false;
  applyVol();
  speaker.play().catch(() => {});
  if (!ctx) {
    const AC = window.AudioContext || window.webkitAudioContext;
    if (AC) ctx = new AC();
  }
  if (ctx && ctx.state === 'suspended') {
    ctx.resume().catch(() => {});
  }
  if (speaker.srcObject) {
    const tracks = speaker.srcObject.getAudioTracks ? speaker.srcObject.getAudioTracks() : [];
    for (let i = 0; i < tracks.length; i++) tracks[i].enabled = true;
    speaker.muted = false;
    wirePlayback(speaker.srcObject);
  }
  if (pendingOffer) {
    try {
      await acceptOffer(pendingOffer);
    } catch (e) {
      who.textContent = 'Offer failed';
    }
  }
  renderWho();
  snapshotRelay();
}
async function start() {
  log('page ' + name);
  gate.classList.add('show');
  who.textContent = 'Waiting for the host';
  function slugifyName(raw) {
    return String(raw || '').toLowerCase().replace(/[^a-z0-9-]/g, '').slice(0, 48);
  }
  function goRoom(next) {
    const slug = slugifyName(next);
    if (!slug || slug === name) {
      if (titleEl) titleEl.value = name;
      return;
    }
    log('room ' + slug);
    location.assign('/' + slug);
  }
  if (titleEl) {
    titleEl.addEventListener('focus', () => titleEl.select());
    titleEl.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        titleEl.blur();
      }
      if (ev.key === 'Escape') {
        ev.preventDefault();
        titleEl.value = name;
        titleEl.blur();
      }
    });
    titleEl.addEventListener('blur', () => goRoom(titleEl.value));
  }
  vol.tabIndex = -1;
  if (muteBtn) muteBtn.tabIndex = -1;
  vol.oninput = applyVol;
  if (muteBtn) {
    muteBtn.onclick = () => {
      if (Number(vol.value) > 0) {
        preMute = Number(vol.value);
        vol.value = '0';
      } else {
        vol.value = String(preMute || 1);
      }
      applyVol();
    };
  }
  if (throwEl && slot) {
    let dragging = false;
    const fromPointer = (ev) => {
      const r = slot.getBoundingClientRect();
      const t = (r.bottom - ev.clientY) / Math.max(1, r.height);
      vol.value = String(Math.max(0, Math.min(1, t)));
      applyVol();
    };
    throwEl.addEventListener('pointerdown', (ev) => {
      if (ev.button) return;
      dragging = true;
      try { throwEl.setPointerCapture(ev.pointerId); } catch (e) {}
      fromPointer(ev);
    });
    throwEl.addEventListener('pointermove', (ev) => { if (dragging) fromPointer(ev); });
    throwEl.addEventListener('pointerup', () => { dragging = false; });
    throwEl.addEventListener('pointercancel', () => { dragging = false; });
  }
  requestAnimationFrame(() => applyVol());
  window.addEventListener('resize', () => placeCap(Math.max(0, Math.min(1, Number(vol.value)))));
  const lock = document.getElementById('lock');
  const pwerr = document.getElementById('pwerr');
  if (lock) {
    lock.onsubmit = (ev) => {
      ev.preventDefault();
      const pw = document.getElementById('pw').value;
      if (!String(pw).trim()) {
        if (pwerr) pwerr.textContent = 'Enter the password';
        return;
      }
      if (pwerr) pwerr.textContent = '';
      secret = pw;
      log('unlock');
      if (socket && socket.readyState === 1) socket.send(pw);
    };
  }
  const unlock = (ev) => {
    if (!gate.classList.contains('show')) return;
    if (ev && ev.target && ev.target.closest && ev.target.closest('#vol, #throw, #mute')) return;
    gate.classList.remove('show');
    document.getElementById('go').onclick = null;
    armed = true;
    speaker.muted = false;
    applyVol();
    speaker.play().catch(() => {});
    if (!ctx) {
      const AC = window.AudioContext || window.webkitAudioContext;
      if (AC) ctx = new AC();
    }
    if (ctx && ctx.resume) ctx.resume();
    log('listen');
    armAudio();
  };
  gate.addEventListener('pointerdown', unlock);
  document.getElementById('go').onclick = unlock;
  try { document.getElementById('go').focus(); } catch (e) {}
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  let retryMs = 1000;
  const openSocket = () => {
    const ws = new WebSocket(proto + '://' + location.host + '/' + name + '/out');
    socket = ws;
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      retryMs = 1000;
      log('signal open');
      if (secret) ws.send(secret);
      armOfferWatch();
    };
    ws.onclose = () => {
      dropPc();
      gotPcm = false;
      lastRoom.live = false;
      const wait = retryMs;
      retryMs = Math.min(retryMs * 2, 8000);
      log('signal closed — retry ' + wait + 'ms');
      who.textContent = wait <= 400 ? 'Room full or reconnecting' : 'Reconnecting';
      setTimeout(openSocket, wait);
    };
    ws.onerror = () => { log('signal error'); };
    ws.onmessage = (ev) => {
      if (typeof ev.data === 'string') onCtrl(ev.data);
    };
  };
  openSocket();
}
start();
</script>`}
</body></html>`;
}
