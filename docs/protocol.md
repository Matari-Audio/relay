# RELAY protocol

Three paths. The operator runs signaling only; audio never touches it.

| Path | Transport | Audio |
|---|---|---|
| Plugin ↔ plugin, same LAN | UDP, port 17492+ | f32 PCM, lossless |
| Plugin ↔ plugin, internet | WebRTC (str0m) | Opus 48 kHz stereo, 192 kbps default ceiling, 10 ms frames |
| Plugins → browser | WebRTC | host and joined-plugin mix |

## LAN

A Share instance binds UDP 17492–17507 and advertises
`_relay._udp.local.` over mDNS. The instance name is the room slug, with TXT
`tag=<16 hex>` (see `relay_core::tag`). A joiner browses for its room's
instance, sends `HELLO` once a second and exchanges `AUDIO` datagrams.
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
| peer → object → host | `{"t":"hello","kind":"web"\|"plugin","auth":"<16 hex>","name":"optional browser name"}`, forwarded as `{"t":"hello","id","kind","auth"}` |
| browser → object | `{"t":"name","name":"new browser name"}` updates the roster while connected |
| host → object → peer | `{"t":"offer","to":id,"sdp","trickle"?:true}`, forwarded as `{"t":"offer","sdp","servers"?:[RTCIceServer…],"trickle"?:true}`; browser offers include ICE servers |
| host → object → peer | `{"t":"deny","to":id}`, forwarded as `{"t":"error","code":"denied"}` |
| peer → object → host | `{"t":"answer","sdp"}`, forwarded as `{"t":"answer","id","sdp"}` |
| accepted peer → object → host | `{"t":"candidate","candidate":"candidate:…","ufrag":"<offer ICE username>"}`, forwarded with `id` |
| host → object → accepted peer | `{"t":"candidate","to":id,"candidate":"candidate:…","ufrag":"<offer ICE username>"}`, forwarded without `to` |
| object → host | `{"t":"leave","id"}` when a peer socket closes |
| object → peers | `{"t":"error","code":"no-host"}` when the host socket closes |
| object → peer | `{"t":"error","code":"full"}`, then close: the 33rd peer |
| object → everyone | `{"t":"roster","you":id\|null,"peers":[{"id","kind","name"?}…]}` when the room changes or a name changes |

- `roster` lists who is in the room: the host as `{"id":0,"kind":"host"}`,
  then every peer the host has sent an offer, with the `kind` from its hello
  (`web` or `plugin`). Browser names are optional, stripped of control characters
  and capped at 24 characters by the object. A deny or a closed socket takes a peer out; when the
  host leaves, the list empties. It is sent to the host and every peer socket
  when the host connects or leaves and when a peer goes in or out. `you` is
  the receiver's own id (`null` for the host). Clients ignore message types
  they do not know, so older plugins and pages are unaffected.
- `auth` is `hex(relay_core::tag(room, password))`, which the host compares
  with its own.
- Hosts include known candidates in offers. With `trickle:true`, browsers send
  answers immediately, then forward candidates as they are discovered. The host
  checks `ufrag` against its current offer, rejecting stale connection candidates.
  Hosts and joining plugins forward late STUN/router candidates too. Receivers
  check the offer generation before adding them; browsers also wait for the remote
  description to be installed. Unknown messages are ignored by older clients.
  Older plugins receive an answer once STUN yields a public candidate or gathering
  finishes, with a 5 s ceiling. If TURN was deliberately enabled, legacy answers
  wait for full gathering so relay candidates are included. Plugin joiners include
  their known candidates in their answer and then forward later discoveries.
- Initial `ice` messages contain only `stun:stun.cloudflare.com:3478`.
  Once the host accepts a browser by sending an offer, the object includes
  `servers` in that offer. TURN is disabled by default: only the exact string
  `TURN_ENABLED="true"` opts in to Cloudflare credentials and `EXTRA_ICE`.
  With the flag missing or disabled, only free STUN is returned, even if
  credentials were previously cached. Cloudflare credentials additionally
  require `TURN_KEY_ID` and `TURN_KEY_TOKEN`. They are 24 h credentials, cached hourly per
  isolate, with port-53 URLs dropped. Optional `EXTRA_ICE` (JSON) adds a
  self-hosted TURN. Waiting peers, denied peers and plugins do not fetch or
  receive TURN credentials. str0m plugins gather their own candidates and
  have no TURN client. Updated browsers also accept older workers' initial
  ICE lists; older browser pages need a reload to use TURN with this worker.

Limits in the object:
- At most 32 peers per room.
- Messages over 16 KB are dropped.
- More than 20 messages a second from a peer socket closes it. Hosts get 80
  to allow simultaneous offers and candidate discoveries across a room.

## WebRTC media

The host offers one `sendrecv` audio m-line: Opus/48000/2, `stereo=1;
sprop-stereo=1; maxaveragebitrate=510000; useinbandfec=1; minptime=10`, 10 ms
frames, Opus `RESTRICTED_LOWDELAY`. Its candidates are:
- host candidates, IPv4 and IPv6;
- server-reflexive candidates from STUN on IPv4 and IPv6 sockets;
- a NAT-PMP/PCP or UPnP mapped candidate when the router grants one.

STUN sends up to three probes, 500 ms apart, on each socket with a matching
server address family. Router mapping begins concurrently. Discoveries are
added to each existing ICE agent and forwarded over signaling once.

Audio reordering waits for at most four packets or 40 ms before moving past a
missing packet, replacing str0m's 15-packet/one-second defaults. Contiguous audio
is delivered immediately. The adaptive playout buffer still absorbs jitter.

Plugin decoders accept mono and stereo Opus. Missing single-frame packets
covering up to 60 ms use the native decoder's concealment when the next packet
arrives, without adding a recovery wait. Longer gaps, including intentional silence, add no concealment
backlog. The restricted-low-delay encoder uses CELT; `useinbandfec=1` negotiates
Opus capability with browsers but does not mean plugin packets contain SILK FEC.
Joining plugins honor their own quality ceiling and gate sustained silence,
keeping the encoder and RTP clock running for immediate resumption. An ICE
disconnection gets five seconds to recover before a plugin join retries; the
browser requests a new offer after three seconds of persistent disconnection.

A host at 44.1 kHz resamples to 48 kHz before encoding. A joining plugin
does the same for its DAW input and decodes the host stream to its own rate.
The host hears each joined plugin and browser microphone through an
independent gain and mute control. Browser listeners hear the host plus
joined plugins, excluding browser microphones. Joined plugins hear the host
only; with more than two plugins, joined plugins do not hear one another.
Each plugin can mute its outgoing input or local output, and disable local
passthrough for an FX return without muting its upstream send.

## Security review

WebRTC media uses DTLS-SRTP encryption, including when TURN relays it.
The signaling service handles room names, peer metadata, SDP, and the
64-bit room/password tag; it does not receive media. The host checks the
tag before offering a track. Room passwords should be strong: the tag is
visible in signaling and mDNS, so a guessed password can be checked offline.

**LAN audio is not private or authenticated.** Its UDP PCM is cleartext and
the tag is broadcast in mDNS, so a device on the local network can listen,
inject, or replay packets. Received samples are bounded and non-finite
samples are discarded, but this does not secure the transport. Do not use
RELAY on an untrusted LAN: Join currently selects LAN automatically, without
a switch to force WebRTC. Encrypting and authenticating LAN packets requires
a new wire version and a secret that is never advertised; this is still
outstanding. This is a code review, not an independent security audit.
