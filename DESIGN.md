---
name: RELAY
colors:
  bg: "#101011"
  field: "#1c1c1e"
  hover: "#28282b"
  line: "#3b3b3e"
  well: "#0a0a0b"
  text: "#f2f2f2"
  dim: "#a4a4a9"
  accent: "#c6ff1f"
  ink: "#101011"
  yellow: "#ffd21a"
  red: "#ff2d46"
radius: 1px Pixel, 4px Standard
fonts:
  pixel_heading: Silkscreen
  pixel_body: Departure Mono
  standard_ui: Barlow 600/700
  standard_numbers: Martian Mono
  icons: Phosphor Bold
---

# Design System: RELAY Signal

The live Matari Audio landing page supplies RELAY's color and type. The
plugin editor and browser listen page use the same dark charcoal, lime signal,
and pixel typography. The plugin also offers a saved Standard appearance:
Barlow and Martian Mono with 4px corners, retaining the Signal palette.

## Type and surfaces

- **Pixel, default.** Silkscreen carries the wordmark, mode labels and main
  actions. Departure Mono carries fields, status, room roster and readouts.
  Corners are 1px. Keep text large enough to read at the plugin's default
  440×156 size and the browser card's content-sized width.
- **Standard, plugin only.** Barlow 600/700 carries words and Martian Mono
  carries values. Corners are 4px. The setting is saved with the DAW project.
- **Color.** Charcoal `bg`, darker meter `well`, one `field` fill and one
  `hover` fill. Lime `accent` marks primary action, selection, live status,
  logo and the low meter range. Use dark `ink` on lime. Yellow and red belong
  to hot meter levels and warning/error states.
- **Fonts.** Departure Mono and Silkscreen use SIL OFL 1.1; their notices ship
  beside the plugin and browser assets.

## Editor

- One flat panel: room, password, link and status on the left; IN and OUT L/R
  meters with gain handles on the right. The meter column remains visible at
  the 380×150 minimum editor size.
- The RELAY wordmark opens changelog and Appearance controls. Pixel and
  Standard are the two explicit choices. The microphone icon stays in the
  header while sharing and opens listener strips. Each narrow charcoal strip
  has a name and microphone mute button above live L/R level rails with the
  same peak hold and gain fader as IN/OUT. A thin line across the rails marks
  0 dB gain; the fader turns red above it. Gain spans -24…+12 dB; double click
  restores 0 dB. A vertical wheel over the panel moves through overflowing
  strips horizontally.
- The logo is a circular dot sending two chevrons. While live, they ripple
  outward on the original damped spring, one behind the other. Pixel mode
  snaps their drawn coordinates to the pixel grid; Standard draws smoothly.
- The output fader remains a white bar over the OUT rails so it stays visible
  across the lime, yellow and red meter gradient.

## Browser listen page

- The card sizes to its content and stops at the available width. Grid tracks
  may shrink to zero minimum so the mic picker and roster do not expand past
  the card in Firefox. At very narrow widths, compact the dB readout.
- The logo keeps its dot and two chevrons, and the live ripple respects
  `prefers-reduced-motion`. The browser page always uses the Pixel appearance.
- Keep room, password and listener name in one-piece fields, a single
  Listen/Stop action, mic button, L/R meter and room roster. No nested cards.
