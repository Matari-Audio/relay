---
target: listen website
total_score: 11
max_score: 40
na_heuristics: 
p0_count: 3
p1_count: 4
timestamp: 2026-08-19T10-20-44Z
slug: apps-relay-web-src-worker-ts
---
# RELAY listen page critique

Target: `apps/relay-web/src/worker.ts` (`listenPage()`)
Live: https://relay.matari-audio.com/<session>

## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|-----------|-------|-----------|
| 1 | Visibility of System Status | 1 | `.who` can read Waiting · 1 listening while the meter is dead; truth is buried in `#log` |
| 2 | Match System / Real World | 1 | Mix engineers expect vertical L+R meters and a fader; they get a horizontal candy-bar and an OS range |
| 3 | User Control and Freedom | 2 | Volume exists; no mute; any pointerdown dismisses the Listen gate |
| 4 | Consistency and Standards | 1 | Studio Blue tokens, Chrome `accent-color` slider. Plugin is Plugcat knobs |
| 5 | Error Prevention | 1 | Unity gain into unknown phone speakers; password error node never written |
| 6 | Recognition Rather Than Recall | 2 | Listen is obvious. After that, ICE slang. Meter has no scale, clip, or L/R |
| 7 | Flexibility and Efficiency | 1 | 22px vertical range hack; no keys beyond native range |
| 8 | Aesthetic and Minimalist Design | 0 | Debug `<pre>` is the heaviest object; empty 28px trough on a 360px stamp in a void |
| 9 | Error Recovery | 1 | Failures are log slang; `#pwerr` never fills; room-full copy is unreachable |
| 10 | Help and Documentation | 1 | Landing: “Open a session from the plugin.” Session page: no why Listen |
| **Total** | | **11/40** | **Critical — redesign the surface** |

## Design Specificity Verdict

**LLM assessment:** Category-interchangeable with a RELAY sticker. Polar Night / Studio Blue numbers are correct. Everything a listener touches is stock web: native range, 28px horizontal GYR bar, `<pre>` console, Barlow, generic Listen pill. Could be any WebRTC demo with the Matari M swapped in.

**Deterministic scan:** `detect.mjs --json apps/relay-web/src/worker.ts` → exit 0, `[]`. False-negative: page HTML lives in a TypeScript template string, HTML parser modules were unavailable (`DEGRADED`). Do not treat empty as AA-clean.

**Visual overlays:** none. Browser visualization skipped: no browser MCP in this session. Evidence: `apps/relay-web/test-output/listen-chromium.png` and `listen-firefox.png`.

## Overall Impression

A debug build of a listen accessory. Token lock is right; the objects are wrong. Biggest opportunity: a hardware-style channel strip (vertical L+R, tactile fader, typeable session name) instead of a centered card with a log dump.

## What's Working

1. Polar Night / `#00aaff` / Matari mark match the plugin numerically.
2. Listen gate is the correct autoplay policy (gesture before audio).
3. dB readout language (−∞ / n.n dB), not “volume 70%”. Wrap `min(360px,100%)` is the right phone constraint.

## Priority Issues

- **[P0] Meter is the wrong object** — Horizontal 28px GYR strip, combined peak, no L/R. Job is “see level.” Fix: two vertical lanes, sunken rail, GYR bottom-up, independent peaks + holds. Suggested: `$impeccable layout`
- **[P0] Volume is a Chromium souvenir** — Native `accent-color` range, 22px track, Firefox overflow onto the log. Fix: custom vertical fader, 44px target, dB law. Suggested: `$impeccable bolder`
- **[P0] `#log` is a debug tumor** — 11px pre is the main content; `aria-live="polite"` speaks every ICE line. Do not delete (host debugging still needs it). Fix: last-line status + expandable session tape. Suggested: `$impeccable distill`
- **[P1] Status copy lies by concatenation** — Waiting · 1 listening (self counted). Fix: Live / Connecting / Asleep / No host. Suggested: `$impeccable clarify`
- **[P1] Gate vs desk** — Full-viewport dimmer; after Listen the desk still looks empty. Fix: in-layout Listen that becomes the live strip. Suggested: `$impeccable onboard`
- **[P1] Password error never writes `#pwerr`** — Suggested: `$impeccable clarify`
- **[P1] Gate is not a dialog** — focus walks the page under the scrim. Suggested: `$impeccable harden`

## Persona Red Flags

**Casey (phone, other room):** 22px slider miss; log unreadable; Waiting · 1 listening after they already tapped Listen.

**Jordan (first link):** H1 is `room-bbce0eaa`; log looks like a crash; unity gain into AirPods.

**Sam (AT):** live region speaks ICE; meter has no valuetext; global pointerdown duplicates Listen.

**Mix engineer who sent the URL:** page does not look like the RELAY insert they just hosted.

## Minor Observations

- `--ice` unused. `#voln` wraps in a 28px column. 48 dB meter window + 8-bit analyser. Desktop void around a 360px stamp. Dead PCM worklet still embedded. LAN hop `fetch(http://)` blocked by CSP. `#spkr` not aria-hidden.

## Questions to Consider

1. If you ripped the Matari SVG, would anyone know this wasn’t a 2019 WebRTC harness?
2. Would you hang this log box on a hardware unit in the live room?
3. Casey is ten meters away: which one rectangle tells them the chorus is hitting, L vs R?
