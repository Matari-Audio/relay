# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

## Users

Musicians and mix engineers who need to hear a DAW track somewhere else: another
machine in the room, a collaborator across town, or a client's phone.

## Product Purpose

RELAY is an insert. Enter the same room name or number in both plugins to
connect automatically. Every plugin sends and receives; users do not choose
Share or Join. On a LAN equal-rate audio is lossless.
Over the internet it is Opus 510 kbps plugin to plugin, or to a browser link.
Success is audio that stays up with no account and nothing to configure.

## Positioning

Matari Audio, same line as BUFFR. Peer to peer like SonoBus, simple like
Audiomovers' link, and free to run: the operator hosts signaling only.

## Operating Context

- A room is three words (`quiet-dusty-papaya`). The password is optional and is
  checked by the sharing plugin, never by the server.
- Signaling elects the room coordinator automatically. The other plugin finds
  it by mDNS on the LAN or connects over the internet. New instances connect
  by default; editing the room enters it automatically. Off disconnects.
- The share link is `relay.matari-audio.com/<room>`, with up to 16 listeners.
  Each one is streamed directly from the sharing machine.

## Brand Commitments

- Vendor **Matari Audio**, product **RELAY**.
- Visual authority: the live **Matari Audio** landing page. Signal uses charcoal, lime #c6ff1f, Silkscreen and Departure Mono; the plugin offers a saved smooth Standard appearance.
- Voice: direct, technical. A studio tool, not a marketing surface.

## Product Principles

- One flat panel: controls on the left, IN and OUT meters with the output fader
  on the right, like a mastering limiter.
- Icon buttons live inside the field they act on: roll a room name, show or hide
  the password, copy the link, show or hide the LAN address.
- The buffer sizes itself; the editor reports the latency instead of asking
  for it.
- Nothing polls the server. A hibernating Durable Object costs nothing while
  it is idle.
