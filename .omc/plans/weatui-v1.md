# weatui v1 — Work Plan

**Status:** `pending approval`
**Mode:** `/oh-my-claudecode:plan --direct --interactive`
**Date:** 2026-07-27
**Target:** `/home/gwohl/code/weatui`

---

## 1. Requirements Summary

A Rust TUI that tells you **weather that can kill you** is coming, with enough lead time to act,
and shows you the radar context behind it.

| # | Requirement | Source |
|---|---|---|
| R1 | Alert on life-threatening NWS products: Tornado Warning, Severe Thunderstorm Warning, Flash Flood Warning, Special Weather Statement, and equivalents | user |
| R2 | Explicitly **exclude** climate/air-quality/marine/heat/surf noise | user |
| R3 | Radar visualization: previous / current / upcoming, scrubable — rewind, fast-forward, pause | user |
| R4 | Keyboard only. No mouse in v1. **Vim keys** for radar pan and timeline | user |
| R5 | YAML config at `~/.config/weatui/config.yaml` | user |
| R6 | Integrate with the user's actual notification system | user ("find out") |
| R7 | `weatui` = TUI; `weatui -d` = headless daemon, alerts only | user |
| R8 | USA-focused, US-gov data sources, ZIP-code addressable | user |
| R9 | Data acquired programmatically, eagerly, JIT, closest-to-realtime available | user |
| R10 | `flake.nix` pinning the toolchain | user (given) |

---

## 2. Reconnaissance Findings (evidence-backed)

All findings below were probed live on 2026-07-27, not assumed.

### 2.1 Host environment (probed)

| Fact | Value | Implication |
|---|---|---|
| Notification daemon | **mako** (`.mako-wrapped` running, owns `org.freedesktop.Notifications` at `:1.24`) | **R6 resolved** |
| `notify-send` | `/run/current-system/sw/bin/notify-send` | shell-out path available |
| mako config | `~/.config/mako/config` already styles `[urgency=critical]` | urgency mapping is free |
| Compositor | Hyprland / Wayland | no X11 assumptions |
| Terminal | kitty 0.47.4 inside tmux 3.6a | `TERM=xterm-256color`, not `xterm-kitty` |
| `allow-passthrough` | `on` | irrelevant under chosen renderer |
| **System rustc** | **1.69.0 (April 2023)** | **too old — blocks build. flake required.** |
| nix | 2.34.8 | flakes available |

### 2.2 Renderer decision — cell rasterization, not images

**Decided.** Radar is a scalar field (dBZ), not a photograph. Rendering it as colored terminal
cells is simpler *and* more capable than blitting rasters.

- Each cell carries two independent 24-bit colors. `▀` (U+2580) makes **fg = top pixel, bg = bottom pixel**.
- All block glyphs give exactly **2 colors per cell** regardless of subcell count. Sextants (2×3) and
  octants (2×4, [Unicode 16.0](https://www.unicode.org/charts/PDF/Unicode-16.0/U160-1CC00.pdf))
  offer more *shape* positions but must quantize 6–8 samples to 2 colors — a fidelity **loss** for a
  continuous colormapped field. Half-blocks sample 2 and keep both: zero quantization.
- Half-blocks are universally supported — no font-coverage risk, no protocol detection, no tmux
  passthrough, no Unicode placeholders.
- Octants/box-drawing are still used for the **overlay** layer (warning polygons, borders) where
  shape resolution matters and 2 colors is plenty.

Rejected: kitty graphics protocol. Would require passthrough + `U+10EEEE` placeholders
([kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)), protocol detection
against a lying `TERM`, and surrenders compositing and colormap control.

Rejected: Braille (U+2800–28FF). 8 dots but only **one** color per cell — strictly worse than
half-blocks for a colormapped field.

**Accepted tradeoff:** a 240×60 terminal yields ~240×120 addressable pixels. Coarse next to a real
radar mosaic; sufficient for "where is the storm relative to me, and is it coming."

### 2.3 Alert transport — polling is sufficient (finding overturns a common assumption)

`api.weather.gov` is poll-only ([NWS API docs](https://www.weather.gov/documentation/services-web-api)).
This initially looked like a latency problem against a ~13-minute tornado lead time. It is not:

```
$ curl -sI 'https://api.weather.gov/alerts/active?point=35.2226,-97.4395'
etag: W/"bb2eb99d6d89c4a9f1b5d62e6d97a06a:..."
cache-control: public, max-age=4, s-maxage=5
```

**NWS itself sanctions ~5-second polling and supports ETags.** Conditional requests
(`If-None-Match`) make steady-state polls near-free 304s. Mean added latency ≈ 2.5s — negligible.

**NWWS-OI rejected for v1.** It is NWS's fastest text path
([NWWS-OI setup](https://www.weather.gov/nwws/OISetup)), but: requires credentialed access with 10+
day approval, suffers planned/unplanned outages up to 3×/month (unplanned have exceeded 10 hours),
delivers raw WMO bulletins requiring hand-rolled VTEC/UGC parsing rather than structured CAP, and
NWS recommends pairing it with a satellite feed for >98% availability. Enormous cost, ~2.5s benefit.
Revisit only if v1 polling proves inadequate.

### 2.4 Alert filtering — CAP `response` is NOT a usable filter

The elegant-looking filter fails. Live enumeration of active alerts nationwide:

| CAP `response` | Wanted | Unwanted sharing the same value |
|---|---|---|
| `Shelter` | Severe Thunderstorm Warning | — |
| `Avoid` | Flash Flood Warning | Small Craft Advisory, Beach Hazards, Rip Current, Gale, High Surf |
| `Execute` | Special Weather Statement | Heat Advisory, Winter Weather Advisory, Wind Advisory |
| `Monitor` | Severe Thunderstorm Watch | **Air Quality Alert** |

Signal-to-noise, live snapshot: **134 Small Craft Advisories** vs **3 Severe Thunderstorm Warnings**.
R2 is not a preference — it is load-bearing.

**Filter must be a two-path allowlist:**

**Path A — VTEC** (`properties.parameters.VTEC`, e.g. `/O.NEW.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/`).
Parse `phenomenon.significance`. Robust against NWS renaming display strings.

| Code | Product | Tier |
|---|---|---|
| `TO.W` | Tornado Warning | LETHAL |
| `EW.W` | Extreme Wind Warning | LETHAL |
| `FF.W` | Flash Flood Warning | LETHAL |
| `SV.W` | Severe Thunderstorm Warning | SEVERE |
| `SQ.W` | Snow Squall Warning | SEVERE |
| `DS.W` | Dust Storm Warning | SEVERE |
| `TO.A` | Tornado Watch | WATCH |
| `SV.A` | Severe Thunderstorm Watch | WATCH |

**Path B — event-string allowlist for non-VTEC products.** Probed: `Special Weather Statement` and
`Air Quality Alert` carry **no VTEC**. SPS is explicitly wanted; AQA is explicitly not. SPS still
carries `eventMotionDescription`, `maxWindGust`, `maxHailSize` — full threat telemetry for
sub-severe-but-dangerous storms.

### 2.5 Lead-time computation — the field that makes R1 work

`properties.parameters.eventMotionDescription`, probed live:

```
2026-07-27T07:00:00-00:00...storm...319DEG...30KT...46.93,-91.76
```

→ timestamp, bearing 319°, speed 30 kt, origin lat/lon. This yields **time-to-impact at the user's
coordinates**, which is the difference between "a warning exists" and "it hits you in 11 minutes."

Impact-based warning tags also present: `windThreat` / `hailThreat` (`RADAR INDICATED` vs `OBSERVED`),
`maxWindGust`, `maxHailSize`. Tornado warnings additionally carry `tornadoDetection` and
`damageThreat` (`CONSIDERABLE` / `CATASTROPHIC` = PDS / tornado emergency) — **spike required**, not
observed live (no active tornado warnings at probe time).

`geometry.type` is `Polygon` — real warning polygon geometry for the overlay layer, and for precise
"am I actually inside it" testing rather than county-level approximation.

### 2.6 Radar data — buy, don't build

NEXRAD Level II is raw polar volume data, free, no sign-in, no requester-pays, "added as soon as it
is available", with SNS topics ([AWS Open Data](https://registry.opendata.aws/noaa-nexrad/)):

| Bucket | Use |
|---|---|
| `unidata-nexrad-level2-chunks` | **real-time** — primary |
| `unidata-nexrad-level2` | archive — backfill on startup |

Maintained Rust crates exist (crates.io, updated 2026). **Do not write a Level II decoder.**

| Crate | Role | Version |
|---|---|---|
| `nexrad-data` | S3 access / AWS integration | 1.0.0-rc.7 |
| `nexrad-decode` | Level II binary decode | 1.0.0-rc.3 |
| `nexrad-model` | shared data model | 1.0.0-rc.2 |

Prior art: `rustywx` 0.8.0 — "NEXRAD Level II weather radar scope with alerts". Worth reading before
writing; not a dependency.

---

## 3. Architecture

```
weatui/
  flake.nix                  # pinned toolchain — system rustc 1.69 is too old
  Cargo.toml
  src/
    main.rs                  # arg parse: weatui | weatui -d
    config.rs                # ~/.config/weatui/config.yaml (serde_yaml)
    geo.rs                   # ZIP -> lat/lon, nearest WSR-88D site selection
    alert/
      poll.rs                # api.weather.gov, ETag conditional GET, 5s tick
      vtec.rs                # P-VTEC parser -> (phenomenon, significance, action, ETN)
      filter.rs              # two-path allowlist (VTEC | event-string)
      motion.rs              # eventMotionDescription -> bearing/speed -> ETA at home
      state.rs               # dedup by (office, phenomenon, ETN); NEW/CON/CAN/EXP lifecycle
    radar/
      fetch.rs               # nexrad-data: chunks + archive backfill
      grid.rs                # polar sweep -> cartesian grid sized to viewport
      ring.rs                # frame ring buffer (decoded dBZ arrays)
    render/
      raster.rs              # dBZ grid -> half-block cells (fg=top, bg=bottom)
      colormap.rs            # dBZ -> RGB, tuned for threat not prettiness
      overlay.rs             # warning polygons, home marker, range rings
      timeline.rs            # scrubber widget
      hud.rs                 # active alerts, ETA, threat tags
    notify.rs                # notify-send -u critical (shell-out)
    daemon.rs                # -d: alert pipeline only, no TTY
```

**Shared core, two front-ends.** `alert/` is front-end agnostic; `daemon.rs` and the TUI both consume it.

**Notification: shell out to `notify-send`.** mako is running and already styles
`[urgency=critical]`. `notify-send -u critical` maps threat tier onto existing config with **zero
dependencies** — no zbus, no libnotify FFI. Alerts are rare; process spawn cost is irrelevant.

**No IPC between daemon and TUI in v1.** Both poll independently. NWS permits 5s polling; two
clients is not a problem. Skipped: a socket protocol. Add when running both simultaneously proves
actually annoying.

### 3.1 Keybindings (R4)

Unambiguous, modeless, vim-flavored.

| Key | Action |
|---|---|
| `h` `j` `k` `l` | pan radar view W / S / N / E |
| `C-d` `C-u` | pan half-viewport down / up |
| `zi` `zo` | zoom in / out |
| `gh` | recenter on home coordinates |
| `[` `]` | timeline: step to previous / next frame |
| `gg` | timeline: jump to oldest frame |
| `G` | timeline: jump to newest (live) |
| `space` | timeline: play / pause |
| `<` `>` | playback speed down / up |
| `?` | help overlay |
| `q` / `ZZ` | quit |

Rationale: `h/j/k/l` is spatial pan; timeline gets bracket-step + `gg`/`G` so no key means two
things depending on invisible focus state. No modal focus switching in v1.

### 3.2 Config sketch

```yaml
location:
  zip: "73019"            # REQUIRED — user must supply
  # or: { lat: 35.2226, lon: -97.4395 }

alerts:
  poll_interval_secs: 5
  tiers:
    lethal:  [TO.W, EW.W, FF.W]
    severe:  [SV.W, SQ.W, DS.W]
    watch:   [TO.A, SV.A]
  extra_events: ["Special Weather Statement"]   # non-VTEC allowlist
  notify:
    lethal: critical
    severe: critical
    watch:  normal

radar:
  site: auto              # or explicit, e.g. KTLX
  frames: 12
  refresh_secs: 60

render:
  colormap: threat        # threat | nws | mono
```

---

## 4. Acceptance Criteria

| # | Criterion | Verification |
|---|---|---|
| A1 | `nix develop` yields rustc ≥ 1.80; `cargo build --release` succeeds | user runs |
| A2 | Given a fixture CAP payload containing `SV.W`, filter returns tier `SEVERE` | unit test |
| A3 | Given a fixture containing `Air Quality Alert` (no VTEC), filter returns **reject** | unit test |
| A4 | Given a fixture containing `Special Weather Statement` (no VTEC), filter returns **accept** | unit test |
| A5 | VTEC `/O.NEW.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/` parses to office `KDLH`, phenomenon `SV`, significance `W`, action `NEW`, ETN `0087` | unit test |
| A6 | `eventMotionDescription` string from §2.5 parses to bearing 319°, speed 30 kt, origin (46.93, −91.76) | unit test |
| A7 | ETA computed for a storm at known bearing/speed/origin vs known home coords is within ±1 min of hand-computed value | unit test |
| A8 | Same alert seen twice (identical office+phenomenon+ETN) notifies **once**; a `CON` update does not re-notify; a `CAN` clears it | unit test |
| A9 | Poller issues `If-None-Match` and treats `304` as no-change without re-parsing | unit test w/ mock |
| A10 | dBZ grid → cell buffer: cell at (r,c) has fg = colormap(grid[2r][c]) and bg = colormap(grid[2r+1][c]) | unit test |
| A11 | `weatui -d` runs with no TTY attached and emits a desktop notification on a qualifying fixture | user runs |
| A12 | `notify-send -u critical` fires for `lethal`/`severe` tiers and renders with mako's `[urgency=critical]` styling | user observes |
| A13 | `[` / `]` / `gg` / `G` / `space` move the timeline; `h/j/k/l` pan without disturbing frame index | user runs |
| A14 | Missing `location.zip` in config produces an actionable error naming the file path, not a panic | unit test |
| A15 | NWS request carries a `User-Agent` with contact info per NWS requirement | unit test |

Non-goals for v1: mouse, non-US sources, historical archive browsing, multi-location, TUI/daemon IPC.

---

## 5. Implementation Steps

**Phase 0 — Toolchain (blocking)**
1. `flake.nix` pinning rust ≥ 1.80 + `pkg-config`, `openssl`. System rustc 1.69 cannot build the dependency set.
2. `Cargo.toml`: `ratatui`, `crossterm`, `tokio`, `reqwest`(rustls), `serde`/`serde_yaml`, `nexrad-data`, `nexrad-decode`, `nexrad-model`.

**Phase 1 — Alert core (the product; ship-blocking)**
3. `config.rs` — load/validate YAML, actionable errors (A14).
4. `alert/vtec.rs` — P-VTEC parser (A5).
5. `alert/filter.rs` — two-path allowlist (A2, A3, A4).
6. `alert/poll.rs` — ETag conditional polling, User-Agent (A9, A15).
7. `alert/state.rs` — dedup + NEW/CON/CAN/EXP lifecycle (A8).
8. `alert/motion.rs` — motion parse + ETA (A6, A7).
9. `notify.rs` — `notify-send -u critical` shell-out (A12).
10. `daemon.rs` + `weatui -d` (A11).

> **Phase 1 is independently shippable.** If radar slips, the thing that saves your life still works.

**Phase 2 — Radar**
11. `geo.rs` — ZIP → lat/lon, nearest WSR-88D.
12. `radar/fetch.rs` — real-time chunks + archive backfill.
13. `radar/grid.rs` — polar → cartesian resample to viewport.
14. `radar/ring.rs` — frame ring buffer.

**Phase 3 — Render**
15. `render/colormap.rs` — dBZ → RGB, threat-tuned.
16. `render/raster.rs` — half-block widget (A10).
17. `render/overlay.rs` — warning polygons, home marker, range rings.
18. `render/timeline.rs` + `render/hud.rs`.
19. Keybinding wiring (A13).

**Phase 4 — Spikes**
20. Verify tornado-warning-specific params (`tornadoDetection`, `damageThreat`) against a live TO.W — unverifiable at plan time.
21. Read `rustywx` for Level II handling lessons.

---

## 6. Risks and Mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| `nexrad-*` crates are `1.0.0-rc`, modest download counts | Medium | Confine to `radar/fetch.rs` + `radar/grid.rs` behind an internal trait. Phase 1 has zero dependency on them, so radar breakage never blocks alerting. |
| Tornado-warning param names unverified (no live TO.W at plan time) | **High** — this is the flagship case | Phase 4 spike #20 **before** claiming R1 complete. Filter on VTEC `TO.W`, which is confirmed-format, rather than on unverified params. Treat threat tags as enrichment, not gating. |
| Silent alert-pipeline death (network, DNS, API change) → false sense of safety | **Critical** | Heartbeat: track last successful poll; TUI shows a stale indicator; daemon emits a `critical` notification if no successful poll in N minutes. **Silence must never be indistinguishable from safety.** |
| NWS rate-limit is unpublished; 5s polling may trip it | Medium | ETag conditional GET; exponential backoff on 429; respect the documented ~5s retry window; honor `cache-control` rather than hard-coding 5s. |
| Polygon containment errors → missed or spurious alerts | High | Use `geometry` polygon point-in-polygon, not county/zone name matching. Unit-test with a known polygon and points inside/outside/on-edge. |
| Radar frame decode is CPU-heavy, blocks UI | Medium | Decode/resample off-thread; UI reads completed frames from the ring buffer only. |
| Terminal too small for useful radar | Low | Minimum viable viewport check; degrade to alert-only HUD with a message. |
| ZIP → coordinates needs a data source | Medium | Resolve via Census/NWS point lookup at config time, cache in config dir. Allow explicit lat/lon bypass. |
| Scope creep into non-lethal products | Medium | R2 is encoded as an allowlist, not a blocklist. Unknown products are rejected by default. |

---

## 7. Verification Steps

**These are for the user to run. Per project rules, this plan does not build, test, or commit.**

```bash
nix develop
cargo build --release
cargo test                      # A2–A10, A14, A15

./target/release/weatui -d      # A11 — daemon, watch for notification
./target/release/weatui         # A13 — keybindings

# Filter sanity against live data (no build required):
curl -s -H 'User-Agent: (weatui, andrew@grathwohl.me)' \
  'https://api.weather.gov/alerts/active?point=<LAT>,<LON>' | jq '.features[].properties.event'
```

Manual observation required for A12 (mako `[urgency=critical]` styling) and A13.

---

## 8. Open Questions

1. **ZIP code** — `location.zip` has no default. Needed before first run.
2. **Watch tier notifications** — should `TO.A` / `SV.A` (watches, hours of lead time) raise a desktop
   notification, or only appear in the TUI? Watches fire often and could train you to ignore them.
3. **Radar site vs. mosaic** — v1 assumes a single nearest WSR-88D. Sites go down for maintenance and
   storms cross coverage boundaries. Multi-site mosaic is deferred; confirm that's acceptable.
4. **"Upcoming" radar (R3)** — NEXRAD provides observed data only. "Upcoming" requires either
   extrapolation from storm motion vectors (cheap, honest, ±) or an HRRR/MRMS forecast product
   (accurate, much heavier). v1 proposes **motion-vector extrapolation, clearly labeled as projected**.

---

<!-- Generated by /oh-my-claudecode:plan --direct --interactive on 2026-07-27 -->
