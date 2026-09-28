# RELAY

![The RELAY editor sharing a room](docs/images/relay-editor.png)

An insert that sends and receives the track it sits on, peer to peer. One
plugin shares a room; another joins it. Both send their DAW input and hear
the other plugin. Browser listeners hear their mix. Each source has its own
gain and mute control in the sharing plugin; turn off **THRU** for an FX return.

| To | How | Audio |
|---|---|---|
| Another RELAY on your LAN | UDP, found by room name over mDNS | 32-bit float PCM, no codec |
| Another RELAY over the internet | WebRTC | Opus 510 kbps, self-sizing jitter buffer |
| Anyone's browser | `relay.matari-audio.com/<room>` | Opus 510 kbps, nothing to install |

No account. The only server is a Cloudflare Durable Object that introduces
the two ends; audio never passes through it. LAN PCM is currently unencrypted;
see the [protocol and security notes](docs/protocol.md).

Open source under [MPL-2.0](LICENSE).

## Layout

```text
core/            relay-core: shared state, LAN link, WebRTC, mDNS, port mapping, playout
plugin/          relay-plugin: MOOSE CLAP/VST3/AU insert, MUI editor
apps/relay-web/  signaling worker (Durable Object) and the browser listen page
apps/web/        matari-audio.com/relay product page (Astro)
```

## Build and test

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --git https://github.com/Matari-Audio/moose cargo-moose --locked
cargo moose install --clap --vst3        # from plugin/; add --au2 on macOS
pnpm install && pnpm -r test && pnpm -r build
```

`cargo test -p relay-plugin` also renders the editor to
`$TMPDIR/relay-editor-{share,join,about}.png`, so you can look at it without a DAW.

## Deploying signaling

`apps/relay-web` deploys with `wrangler deploy`. Optional secrets:
`TURN_KEY_ID` and `TURN_KEY_TOKEN` (Cloudflare Realtime TURN) and `EXTRA_ICE`
(JSON, e.g. a self-hosted TURN). Without TURN, peers behind symmetric NAT
cannot connect over the internet. LAN is unaffected.

## License

Mozilla Public License 2.0; see [LICENSE](LICENSE).
