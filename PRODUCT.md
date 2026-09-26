# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

## Users

Musicians and mix engineers who need to hear a DAW track somewhere else: another
machine in the room, a collaborator across town, or a client's phone.

## Product Purpose

RELAY is an insert. **Share** sends the track; **Join** plays someone else's. On
a LAN it is lossless and adds no latency beyond the receiver's jitter buffer.
Over the internet it is Opus 510 kbps plugin to plugin, or to a browser link.
Success is audio that stays up with no account and nothing to configure.

## Positioning

Matari Audio, same line as BUFFR. Peer to peer like SonoBus, simple like
Audiomovers' link, and free to run: the operator hosts signaling only.

## Operating Context

- A room is three words (`quiet-dusty-papaya`). The password is optional and is
  checked by the sharing plugin, never by the server.
- Join finds the room on the LAN by mDNS first, then over the internet.
- The share link is `relay.matari-audio.com/<room>`, with up to 16 listeners.
  Each one is streamed directly from the sharing machine.

## Brand Commitments

- Vendor **Matari Audio**, product **RELAY**.
- Visual authority: **BUFFR**. Dark charcoal, high contrast, orange for action.
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
