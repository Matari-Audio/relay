# RELAY plugin

Truce 7.0 shell around `relay-session`. The host audio callback only copies into preallocated buffers, publishes atomics, and renders playback; it never allocates, locks, or touches a string. One supervised fan-out thread owns the LAN listen page, the browser P2P peers, and the cloud signalling socket. The editor is a resizable 680×480 vizia window in the Polar Night palette, with a centred Share|Join switch, Barlow typography, right-side stereo meters, and a send spectrum.

Home-network sharing uses **5 ms uncompressed stereo PCM** on LAN (no Opus, no FEC lookahead). That is the lowest delay this path can do: one 5 ms packet plus your DAW buffer. It is not zero — nothing in a DAW insert can be — but it is the blazing LAN path.

## You can test now

```bash
cd apps/plugin
cargo truce install
```

| Format | Path | Notes |
|---|---|---|
| CLAP | `~/.clap/RELAY.clap` | default |
| VST3 | `~/.vst3/RELAY.vst3` | default |
| VST2 | `~/.vst/RELAY.so` | Linux; macOS/Windows use the host VST folder |
| LV2 | `~/.lv2/relay.lv2` | default |
| AU v2 | `~/Library/Audio/Plug-Ins/Components/RELAY.component` | macOS |
| AU v3 | `cargo truce install --au3` | macOS + Xcode |
| Standalone | `target/release/relay-plugin-standalone` | `cargo truce run` |
| AAX | `cargo truce install --aax` | needs Avid SDK; not default |

Public listen: https://relay.matari-audio.com/`<session-name>`

Downloads: [matari-audio.com/relay](https://matari-audio.com/relay). Licensed [MPL-2.0](../../LICENSE). Rescan plugins in the DAW.

## Home LAN (use this)

1. Both machines on the same Wi-Fi / Ethernet.
2. Instance A: **Share**, leave it on. Copy the listen link or tell the other machine the session name (`big-filthy-papaya`).
3. Instance B: **Join**, Peer = that session name (or `192.168.x.x:17492`).
4. Monitor defaults to **Mix** (dry plus remote, so an underrun is not silence). **Hear** is the return only. **Dry** is a tap.

Share is always a tap: DAW output is the incoming buffer, unchanged. Send only scales the stream. Hear only exists on Join. The status lamp in the footer is the **Live** switch: on by default, click it to take the whole session offline. When the listen page has gone to sleep the lamp reads ASLEEP and a click wakes it. Nobody connected means no audio leaves.

## Browser listen

https://relay.matari-audio.com/`<session-name>` — click **Listen**. Cloudflare carries SDP/ICE only (cap 10). Audio is plugin→browser WebRTC P2P: sendonly Opus on a native audio track, played by the browser's `<audio>` element. Same-/24 browsers hop to the plugin LAN page and hear local PCM.

## Products

| Mode | What it does |
|---|---|
| Share | Host a named session. DAW audio passes through. Listeners use the copied link or another RELAY on Join. |
| Join | Attach to `host:port` or a LAN session name. Hear the remote. Mix is the default monitor. |

Paid TURN / subscriptions are not implemented. Cross-NAT without a port forward will fail; same LAN does not need TURN.
