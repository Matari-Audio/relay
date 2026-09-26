---
name: RELAY
colors:
  bg: "#0e1014"
  field: "#1b1f27"
  hover: "#262b36"
  line: "#2c313d"
  well: "#07080a"
  text: "#f4f6fa"
  dim: "#8b93a4"
  accent: "#ff6b1a"
  ink: "#140700"
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
The look is colourful and high contrast, sporty but quiet: a charcoal panel,
one orange accent, and green/yellow/red kept for the meters and status lamps.

## Palette

- **Ground.** `bg` #0e1014 is the panel. `field` #1b1f27 fills every control,
  `hover` #262b36 when the pointer is on it. `line` #2c313d is the only hairline.
  `well` #07080a is the unlit part of a meter.
- **Text.** `text` #f4f6fa for values, `dim` #8b93a4 for icons, labels and
  scale numbers. Both pass AA on `bg` and `field`.
- **Accent.** Orange #ff6b1a is for action and selection: the picked mode
  segment, the Listen and Download buttons, the output fader, focus rings.
  Anything on orange uses `ink` #140700, never white.
- **Signal.** Green #1fe06a, yellow #ffd21a and red #ff2d46 belong to the
  meters. Outside them they only mean status: a green lamp is live, yellow is
  waiting or reconnecting, red is an error.

## Rules

- **Compact.** The plugin is a 440×156 panel. Nothing is nested; 8px padding,
  4px corners on everything.
- **Fields are one container.** A field is a single `field`-coloured box: a
  dim leading icon (`#` room, lock password, link, wifi LAN), the value, and its
  action button inside at the right end (roll, show/hide, copy). No separate
  buttons beside a field, no caption column.
- **Mode.** Off / Share / Join is a segmented control in one `field` box; the
  picked segment is filled orange with `ink` text.
- **Type.** Barlow SemiBold for words, Bold for the RELAY mark and buttons.
  Martian Mono only for numbers (dB, ms, IP, scale). Phosphor Bold icons.
- **Quiet.** Motion and colour only where something changed.

## Meters

- Fat L/R rails, 1px apart, on a -60..0 dB scale with ticks at -6, -12, -24
  and -48. Green up to -12 dB, yellow from -12 to -3, red above -3.
- Each rail has a white peak-hold line (holds 1.5 s, then falls 20 dB/s) and a
  clip lamp above it that lights red while the hold is at 0 dB.
- The plugin shows an IN pair and an OUT pair with the dB scale between them.
  The listen page shows one L/R pair (what arrives) with the same colours,
  scale, hold line and clip lamps.

## Output fader

- The output gain is an orange fader riding over the OUT pair, like Pro-L:
  drag anywhere on the OUT rails; double-click for 0 dB. The gain reads out
  under the pair in orange.
- Above the meters sit three readouts: max IN, max OUT and max true peak since
  the last reset. They turn red above 0 dB. Click them to reset.
