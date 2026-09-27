# Changelog

## Unreleased

- Built on MOOSE 7 (Matari's truce fork) instead of truce 6.3. Sessions and presets from 0.2.0 load unchanged.
- An Audio Unit (AU v2) for macOS, in the same installer as the CLAP and VST3.
- The meters and link status keep moving while the editor is left alone.
- The editor scales with the window when you resize it.
- The about panel's notes no longer overlap, and scroll when they don't fit.
- Typing in the room and password fields works on Windows.

## 0.2.0

- Rewritten from scratch: one small core, one plugin, one worker.
- Internet rooms over WebRTC with Opus up to 510 kbps, adapting to the slowest listener under a ceiling you pick; lossless on a LAN.
- Anyone can listen in a browser at relay.matari-audio.com/<room>, with their own volume.
- Browser listeners can talk back with their mic; the plugin plays it and shows who is talking.
- Nothing is sent over the internet during silence.
- The buffer sizes itself to the network and resyncs in silence.
- IN and OUT meters, each with its own fader.
- The editor keeps its scale when reopened.
- Windows, macOS (signed and notarized, Universal) and Linux.
