---
name: RELAY
colors:
  bg: "#121212"
  field: "#1e1e1e"
  hover: "#292929"
  line: "#2e2e2e"
  well: "#0a0a0a"
  text: "#f2f2f2"
  dim: "#8c8c8c"
  accent: "#1fe06a"
  ink: "#04140a"
  green: "#1fe06a"
  yellow: "#ffd21a"
  red: "#ff2d46"
radius: 4px
fonts:
  ui: Barlow 600
  heading: Barlow 700
  numbers: Martian Mono
  icons: Phosphor Bold
---

# Design System: RELAY

The plugin editor (`plugin/src/ui.rs`), the listen page (`apps/relay-web/public`)
and the product page (`apps/web`) share one set of tokens, listed above.
The look is colourful and high contrast, sporty but quiet: a neutral charcoal
panel with no blue tint, the meter green as the one accent, and yellow and red
kept for the meters and problems. No orange anywhere.

## Palette

- **Ground.** `bg` #121212 is the panel. `field` #1e1e1e fills every control,
  `hover` #292929 when the pointer is on it. `line` #2e2e2e is the only hairline.
  `well` #0a0a0a is the unlit part of a meter. All neutral grey.
- **Text.** `text` #f2f2f2 for values, `dim` #8c8c8c for icons, labels and
  scale numbers. Both pass AA on `bg` and `field`.
- **Accent.** Green #1fe06a, the same green as the meters, is for action and
  selection: the picked mode segment, the Listen and Download buttons, links, focus rings, text selection, the logo mark and the live
  lamp. Anything on green uses `ink` #04140a, never white.
- **Signal.** Yellow #ffd21a and red #ff2d46 appear in the meters and, outside
  them, only for status: yellow is waiting or reconnecting, red is an error.

## Rules

- **Compact.** The plugin is a 440×156 panel. Nothing is nested; 8px padding,
  4px corners on everything.
- **Fields are one container.** A field is a single `field`-coloured box: a
  dim leading icon (`#` room, lock password, link, wifi LAN), the value, and its
  action button inside at the right end (roll, show/hide, copy). Each icon sits
  centred in a square box as tall as the field, so its padding is equal on all
  sides. No separate buttons beside a field, no caption column.
- **Mode.** Off / Share / Join is a segmented control in one `field` box; the
  picked segment is filled green with `ink` text.
- **Type.** Barlow SemiBold for words, Bold for the RELAY mark and buttons.
  Martian Mono only for numbers (dB, ms, IP, scale). Phosphor Bold icons.
- **Quiet.** Motion and colour only where something changed.

## Meters

- Fat L/R rails, 1px apart, on a -60..0 dB scale. Dim scale numbers sit beside
  the rails (0, -6, -12, -24, -48); nothing crosses the rails, no tick lines.
- Each rail is a `well` with 2px corners holding one smooth vertical gradient:
  red at the top (0 dB), yellow 15% down, green from 35% down to the bottom.
  It is revealed only up to the level; above it the dark `well` shows.
- A thin white peak-hold line rides each rail (holds 1.5 s, then falls 20 dB/s).
- The plugin shows an IN pair and an OUT pair with the dB scale between them.
  The listen page shows one L/R pair (what arrives) the same way.

## Output fader

- The output gain is a white bar riding over the OUT pair (green would vanish into the meter), like Pro-L:
  drag anywhere on the OUT rails; double-click for 0 dB. The gain reads out
  under the pair.
- Above the meters sit three readouts: max IN, max OUT and max true peak since
  the last reset. They turn red above 0 dB. Click them to reset.
