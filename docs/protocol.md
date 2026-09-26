# RELAY protocol

Three paths. The operator runs signaling only; audio never touches it.

| Path | Transport | Audio |
|---|---|---|
| Plugin → plugin, same LAN | UDP, port 17492+ | f32 PCM, lossless |
| Plugin → plugin, internet | WebRTC (str0m) | Opus 48 kHz stereo, 510 kbps, FEC, 10 ms frames |
| Plugin → browser | WebRTC | same Opus track |

## LAN

A Share instance binds UDP 17492–17507 and advertises
`_relay._udp.local.` over mDNS. The instance name is the room slug, with TXT
`tag=<16 hex>` (see `relay_core::tag`). A joiner browses for its room's
instance, sends `HELLO` once a second and gets `AUDIO` datagrams back.
The wire format is `relay_core::wire`. If mDNS finds nothing in 1.5 s, the
joiner falls back to the internet path, and keeps browsing.

## Signaling

A Cloudflare Worker with one Durable Object per room, at `relay.matari-audio.com`.
The object uses the WebSocket Hibernation API and forwards JSON between the
host and its peers. It never sees audio or the password.

- `GET /<room>` serves the listen page.
- `GET /<room>/host?key=<hex>` (WebSocket) is the plugin that shares. The first
  host's `sha256(key)` is stored. Another key is refused with
  `{"t":"error","code":"taken"}` while that host is connected. When the room
  has no host, a new key replaces the stored one. The same key connecting
  again replaces the old host socket. A key that is not 16–128 hex characters
  gets HTTP 400.
- `GET /<room>/peer` (WebSocket) is a browser listener or a joining plugin.
  With no host connected the peer gets `{"t":"error","code":"no-host"}` and
  stays connected. When a host arrives, the object sends `{"t":"host"}` to its
  peers, and they send `hello` again.
- Room is `relay_core::slug`: `[a-z0-9-]{1,48}`.
- Ping: the object sets `setWebSocketAutoResponse("ping" → "pong")`.
  Clients send the text `ping` every 30 s.

Messages. `id` is a random u32 (a JSON number) the object assigns to each
peer socket. A peer sends `hello` after it receives `ice`, and again on `host`
or when its connection fails.

| Direction | Message |
|---|---|
| object → any, on connect | `{"t":"ice","servers":[RTCIceServer…]}` |
| peer → object → host | `{"t":"hello","kind":"web"\|"plugin","auth":"<16 hex>"}`, forwarded as `{"t":"hello","id","kind","auth"}` |
| host → object → peer | `{"t":"offer","to":id,"sdp"}`, forwarded as `{"t":"offer","sdp"}` |
| host → object → peer | `{"t":"deny","to":id}`, forwarded as `{"t":"error","code":"denied"}` |
| peer → object → host | `{"t":"answer","sdp"}`, forwarded as `{"t":"answer","id","sdp"}` |
| object → host | `{"t":"leave","id"}` when a peer socket closes |
| object → peers | `{"t":"error","code":"no-host"}` when the host socket closes |
| object → peer | `{"t":"error","code":"full"}`, then close: the 33rd peer |
| object → everyone | `{"t":"roster","you":id\|null,"peers":[{"id","kind"}…]}` when the room changes |

- `roster` lists who is in the room: the host as `{"id":0,"kind":"host"}`,
  then every peer the host has sent an offer, with the `kind` from its hello
  (`web` or `plugin`). A deny or a closed socket takes a peer out; when the
  host leaves, the list empties. It is sent to the host and every peer socket
  when the host connects or leaves and when a peer goes in or out. `you` is
  the receiver's own id (`null` for the host). Clients ignore message types
  they do not know, so older plugins and pages are unaffected.
- `auth` is `hex(relay_core::tag(room, password))`, which the host compares
  with its own.
- Offers and answers are complete (no trickle ICE): each side gathers
  candidates before it sends.
- `ice` servers are Cloudflare TURN credentials when `TURN_KEY_ID` and
  `TURN_KEY_TOKEN` are set. They are 24 h credentials, minted at most hourly
  per worker instance, with port-53 URLs dropped. Always included: plus `stun:stun.cloudflare.com:3478`.
  Optional `EXTRA_ICE` (JSON) adds a self-hosted TURN.

Limits in the object:
- At most 32 peers per room.
- Messages over 16 KB are dropped.
- More than 20 messages a second from one socket closes it.

## WebRTC media

The host offers one `sendonly` audio m-line: Opus/48000/2, `stereo=1;
sprop-stereo=1; maxaveragebitrate=510000; useinbandfec=1; minptime=10`, 10 ms
frames, Opus `RESTRICTED_LOWDELAY`. Its candidates are:
- host candidates, IPv4 and IPv6;
- a server-reflexive candidate from STUN;
- a NAT-PMP/PCP or UPnP mapped candidate when the router grants one.

A host at 44.1 kHz resamples to 48 kHz before encoding. A joining plugin
decodes and resamples to its own rate, then feeds the playout buffer, which
absorbs jitter and drift.
