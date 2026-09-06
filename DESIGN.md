---
name: RELAY
description: Polar Night listen chassis — BUFFR Studio Blue on a named session.
colors:
  polar-night: "#191919"
  chassis: "#252525"
  surface: "#353535"
  well: "#101010"
  hairline: "#2e2e2e"
  text: "#ffffff"
  muted: "#b8b8b8"
  studio-blue: "#00aaff"
  ink: "#041018"
  ok: "#5be8b3"
  warn: "#ffc75c"
  hot: "#ff7088"
  gyr-floor: "#3d8f6a"
typography:
  display:
    fontFamily: "Barlow, system-ui, sans-serif"
    fontSize: "22px"
    fontWeight: 600
    lineHeight: 1.15
    letterSpacing: "-0.02em"
  title:
    fontFamily: "Barlow, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: "0.16em"
  body:
    fontFamily: "Barlow, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: 1.35
    letterSpacing: "normal"
  label:
    fontFamily: "Barlow, system-ui, sans-serif"
    fontSize: "11px"
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: "0.12em"
rounded:
  hardware: "4px"
  well: "3px"
  meter: "2px"
spacing:
  xs: "8px"
  sm: "10px"
  md: "16px"
  lg: "20px"
  wrap: "400px"
components:
  button-primary:
    backgroundColor: "{colors.studio-blue}"
    textColor: "{colors.ink}"
    rounded: "{rounded.hardware}"
    padding: "0 22px"
    height: "48px"
  button-primary-hover:
    backgroundColor: "{colors.studio-blue}"
    textColor: "{colors.ink}"
  button-mute:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.muted}"
    rounded: "{rounded.well}"
    height: "44px"
  button-mute-on:
    backgroundColor: "{colors.hot}"
    textColor: "{colors.ink}"
    rounded: "{rounded.well}"
    height: "44px"
  chassis:
    backgroundColor: "{colors.chassis}"
    textColor: "{colors.text}"
    rounded: "{rounded.hardware}"
    padding: "20px 18px 16px"
    width: "{spacing.wrap}"
  well:
    backgroundColor: "{colors.well}"
    rounded: "{rounded.hardware}"
    padding: "16px 12px 14px"
  input-session:
    backgroundColor: "transparent"
    textColor: "{colors.text}"
    typography: "{typography.display}"
    rounded: "0px"
---

# Design System: RELAY

## Overview

**Creative North Star: "Polar Night chassis"**

RELAY listen is a small hardware unit on a studio wall, not a marketing page and not a WebRTC demo card. The visitor meets a BUFFR Studio Blue plate: Polar Night around it, a #252525 chassis, sunken #101010 wells, flat GYR rails, a flat fader cap, and one Studio Blue action (Listen / Open). Barlow carries every label. Corners stay 2–4px.

The same tokens ship in the DAW editor. The website is a listen accessory for that insert: type the session name, tap Listen, watch L and R, pull the fader. Diagnostics live in a tape you open. Product download lives at matari-audio.com/relay.

**Key Characteristics:**
- Polar Night ground, chassis plate, sunken wells — three neutrals, no glass
- Studio Blue only on the primary action, caret, and focus
- Vertical L / fader / R as the first-viewport object
- Barlow 11 / 12 / 13 / 22px; tracked labels, tabular dB
- Hardware radii 2–4px; 44px minimum on touch controls
- Plugin editor: a fixed 400×260 plate on a two-line grid (labels at 16px, controls at 80px, readouts flush right). Header: tracked RELAY wordmark, Share / Join segmented switch. Four labelled rows: wells for text, segmented switches for enums, a flat-cap fader for gain with a stereo L / R GYR bar pair attached under Send. Footer: status lamp plus one sentence, and the lamp is the Live switch. Text-only actions (Copy / Open), no icons.

## Colors

Restrained neutrals plus one accent. State greens / ambers / reds are meter and lamp language, not brand decoration.

### Primary
- **Studio Blue** (#00aaff): Listen, Open, focus ring, caret, title underline on focus. Used on a small fraction of the surface.

### Neutral
- **Polar Night** (#191919): page ground, the studio wall.
- **Chassis** (#252525): the listen plate.
- **Surface** (#353535): mute rest, hairline neighbors, title rest underline.
- **Well** (#101010): meter trough, fader slot, tape log, password field.
- **Hairline** (#2e2e2e): tape divider.
- **Text** (#ffffff): session name, mark, primary type.
- **Muted** (#b8b8b8): status, hints, tape, channel letters.
- **Ink** (#041018): type on Studio Blue.

### State
- **OK** (#5be8b3): live lamp; GYR mid.
- **Warn** (#ffc75c): asleep lamp; GYR high.
- **Hot** (#ff7088): down / clip / mute-on; GYR clip.
- **GYR floor** (#3d8f6a): meter bottom.

**The Accent Rule.** Studio Blue is the action, the caret, and the focus ring. It is not a fill, a glow, or a meter.

**The GYR Rule.** Level is a bottom-up green–yellow–red rail. Never a single-hue bar and never a horizontal combined peak.

## Typography

**Display Font:** Barlow (system-ui, sans-serif)
**Body Font:** Barlow (same family)
**Label Font:** Barlow 11px / 700 / 0.12–0.16em

**Character:** One family. Tracked small caps energy on product and channel labels; slightly tight 22px for the session nameplate. Tabular numerals on dB and tape.

### Hierarchy
- **Display** (600, 22px, 1.15, -0.02em): session name. It is an editable nameplate, not a heading costume.
- **Title / product** (700, 13px, 0.16em): RELAY in the nav.
- **Body** (500, 13px): status line under the name.
- **Label** (700, 11px, 0.12–0.14em): L, R, MUTE.
- **Data** (500, 12px, tabular): volume readout, tape.

**The Nameplate Rule.** The session name is a text field with a 1px underline. It is how you jump rooms. Do not replace it with a static h1.

## Layout

A compact unit, `min(400px, 100%)`, centered on Polar Night. Body padding 28px / 16px, extra bottom for the home indicator. Inside the chassis: nav 32px, nameplate, 10px to status, 16px to the strip. The desk is a centered flex cluster — L rail (8px in a 24px lane), fader (56px), R rail — 22px gaps — inside a 260px sunken well. Mobile drops the desk to 232px and the rails to 6px. Do not stretch the unit to fill the desktop; the void is the wall.

## Elevation & Depth

Tonal stacking plus one inset highlight and one drop on the chassis. Wells recede with inner shadow, not a 1px ghost border.

### Shadow Vocabulary
- **Chassis** (`inset 0 1px 0 #3f3f3f, 0 18px 40px #00000073`): the plate on the wall.
- **Well** (`inset 0 1px 4px #000000b3`): meter trough, optional.
- **Fader cap**: flat #d8d8d8 rectangle, 28×12, 2px corners. No metal grain.

**The One Plate Rule.** One chassis, one drop. Nested cards are out.

## Shapes

Hardware, not pills. Chassis and primary button 4px; mute and tape well 3px; meter rail and cap 2px. Title underline is square (0). Clip lamps are 8px circles because they are lamps.

## Components

### Buttons
- **Shape:** 4px chassis corners (primary), 3px mute.
- **Primary:** Studio Blue on ink, min-height 44–48px, weight 700. Hover brightens; active nudges 1px. Used for Listen and Open.
- **Mute:** surface on muted; 44px; letterspaced uppercase. On-state is hot on ink.

### Chassis
- **Corner:** 4px
- **Background:** chassis #252525
- **Shadow:** chassis vocabulary
- **Padding:** 20px 18px 16px (16px 14px 14px on small)

### Inputs / Fields
- **Session name:** transparent, 22px Barlow, 1px surface underline, Studio Blue underline and caret on focus. Select-all on focus. Enter commits a slug and assigns `/{slug}`.
- **Password:** well fill, 16px, 4px, Studio Blue outline on focus. Error text is hot.

### Meters
- Vertical stereo GYR rails, 8px (6px small). Flat fill, no LED notches, no analog ticks. Independent peak + hold, clip lamp above each rail. Cover is the well color from the top. Hold is a 1px white tick, hidden when silent. In the plugin the same rails sit on the left and right, from the nav to the bottom edge.

### Fader
- Custom throw, not native range chrome. 4px well slot, 28×12 flat cap. Hidden range remains the accessible control. Pointer maps y → 0–1 with max at the top. Readout is dB, including −∞.

### Tape
- Hairline, last line in a 44px summary, expanded log in a well. `aria-hidden` on the pre. Status lives on the 13px who line (`aria-live="polite"`), never on ICE.

### Navigation
- Mark + RELAY + 8px lamp. Lamp states: wait (dim), live (ok), sleep (warn), down (hot). Hidden on the landing join.

### Gate
- Dialog over the strip only (`role="dialog"`). Title stays usable so a listener can jump rooms before arming audio. Primary control is Listen.

## Do's and Don'ts

### Do:
- **Do** keep L rail | fader | R rail as the first-viewport object.
- **Do** keep meters flat: GYR fill, cover from the top, no LED slots.
- **Do** let the session title be typed; slugify and redirect.
- **Do** put diagnostics in the tape; put human status on the who line.
- **Do** use Studio Blue only for action, caret, and focus.
- **Do** match the plugin tokens (#191919 / #252525 / #101010 / #00aaff, Barlow, GYR).

### Don't:
- **Don't** ship a horizontal combined meter or a native `accent-color` range.
- **Don't** make the log the main content or an `aria-live` dump of ICE.
- **Don't** use 12–16px card radii, glass, or a SaaS dashboard layout.
- **Don't** invent a second typeface. Barlow is the product face.
- **Don't** fill Polar Night with marketing sections; this surface is a listen box.
