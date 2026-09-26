---
name: RELAY
colors:
  bg: "#0a0b0d"
  field: "#06070a"
  line: "#22252c"
  text: "#f3f5f8"
  dim: "#8b93a1"
  studio-blue: "#00aaff"
  on-accent: "#00131d"
  ok: "#2fd67b"
  warn: "#ffb020"
  bad: "#ff4d4f"
radius: 3px
fonts:
  ui: Barlow 600
  heading: Barlow 700
  numbers: Martian Mono
  icons: Phosphor Bold
---

# Design System: RELAY

The plugin editor (`plugin/src/ui.rs`), the listen page (`apps/relay-web/public`)
and the product page (`apps/web`) share one set of tokens, listed above.

## Rules

- **Flat.** One panel on the background, never a card inside a card. Blocks are
  padded 8–12px, with 16px page gutters on mobile.
- **Hardware corners.** 3px on fields and panels, 2px on segments. No pills.
- **Welded groups.** A field and its icon buttons share one bordered well. The
  mode switch is one well with segments inset 2px.
- **Accent.** Studio Blue marks action, selection, focus and the caret. It is
  never a meter.
- **Meters.** Vertical L and R rails on the right edge on a -60..0 dB scale:
  green, amber above -6 dB, red above -1 dB. Ticks at -6, -12, -18, -24 and
  -48. A mono peak readout sits below the rails.
- **Type.** Barlow SemiBold for UI, Barlow Bold for the RELAY mark and headings.
  Labels are small caps in `dim`. Numbers (dB, ms, IP addresses) are Martian
  Mono. URLs and words stay in Barlow.
- **Icons.** Phosphor Bold, 13px, `dim`, turning `text` on hover. Every icon
  button has an accessible label.
- **Status.** One line at the bottom-left: a 6px lamp (ok / warn / bad / accent
  while waiting) plus words. Latency is at the right, in mono.
