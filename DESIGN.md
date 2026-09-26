---
name: RELAY
colors:
  bg: "#050505"
  pill: "#161616"
  hover: "#262626"
  text: "#ffffff"
  dim: "#6e6e6e"
  volt: "#c8ff00"
  warn: "#ffb400"
  bad: "#ff3b30"
radius: 7px
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

- **Compact.** The plugin is a 300×132 strip. Nothing is nested; 8px padding.
- **Welded pills.** Controls are 24px pills with 7px corners, 4px apart, welded
  (MUI `Weld::all().reach(5).blend(1.5)`) so facing edges fuse with a fillet
  neck. An icon button welds to the field it acts on.
- **Black and white.** Selection is a white pill with black ink. Volt (#c8ff00)
  appears only on the live lamp. Amber and red appear only for problems and clipping.
- **Labels are icons.** `#` room, lock password, link, wifi LAN. No caption column.
- **Meter.** One 9px strip on the right edge, L and R 4px each with a 1px seam,
  -60..0 dB, white with red above -1 dB. No ticks, labels or readout.
- **Type.** Barlow SemiBold for UI, Bold for the RELAY mark, Martian Mono only
  for numbers (ms, IP). Phosphor Bold icons at 12px.
- **Quiet.** Motion and colour only where something changed.
