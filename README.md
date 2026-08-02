# weatui

A terminal weather radar and severe-weather alerting application. Live NEXRAD
Level II radar, NOAA's own model forecast of where the storms go next, and
desktop notifications for the warnings that can actually kill you — all in a
truecolor TUI with vim keys.

![weatui main view](assets/main.png)

*Live composite reflectivity from KPAH with distance rings, the home
crosshair, and current surface conditions in the HUD. The layer line at the
bottom shows the default stack: reflectivity base with velocity and
debris-detection augmentations standing by.*

## What it does

- **Live radar** — NEXRAD Level II volumes fetched straight from the NOAA
  realtime feed, rendered at full resolution as half-block pixels (two
  independently coloured pixels per terminal cell). Composite reflectivity
  with a dual-polarization correlation mask that scrubs insects, birds and
  ground clutter without touching real precipitation.
- **The future, from the model that knows** — forecast frames come from NOAA
  HRRR's simulated composite reflectivity (`REFC`). HRRR assimilates live radar every 15 minutes
  and can grow, decay and initiate storms — it is a real forecast, not a
  smear of the current picture. Scrub seamlessly from an hour or more of
  observed history to eighteen hours ahead on one timeline.
- **Severe weather alerts** — polls api.weather.gov every few seconds,
  classifies by P-VTEC into lethal / severe / watch tiers, draws warning
  polygons on the map, estimates time-of-arrival from the storm motion
  vector, and pushes desktop notifications.
- **Current conditions** — temperature, dew point, humidity, wind,
  visibility and precipitation from the nearest NWS observation station,
  refreshed every five minutes.
- **A feed watchdog** — if alert polling fails long enough, weatui tells you
  loudly. Silence is never allowed to look like safety.

![forecast frame](assets/forecast.png)

*A forecast frame (`FCST` in the timeline): HRRR's prediction for 90 minutes
from now, rendered through the same colormap as the observed frames so the
timeline never changes meaning as it crosses "now".*

## Install

Requirements:

- A truecolor terminal (kitty, alacritty, wezterm, foot, ghostty…)
- A [Nerd Font](https://www.nerdfonts.com/) for the HUD glyphs
- `notify-send` (libnotify) with any notification daemon — mako, dunst,
  swaync — for desktop alerts
- Rust ≥ 1.85, or Nix

With Nix (the repo pins the toolchain via `flake.nix`):

```sh
git clone https://github.com/agrathwohl/weatui
cd weatui
nix develop -c cargo build --release
./target/release/weatui
```

With a plain Rust toolchain:

```sh
cargo build --release
```

No system libraries are needed: nothing links OpenSSL, GRIB2 decoding is
pure Rust, and the map projection is hand-rolled.

## Configure

`~/.config/weatui/config.toml` (honours `XDG_CONFIG_HOME`). The minimum is a
location:

```toml
[location]
zip = "37025"        # US ZIP code, resolved offline from embedded centroids
# — or exact coordinates, which win over zip when both are present:
# lat = 35.9527
# lon = -87.3085
```

Everything else has defaults:

```toml
[radar]
site = "auto"        # nearest WSR-88D, or force one: "KOHX"
frames = 12          # observed history frames kept for playback
refresh_secs = 60    # radar poll cadence

[alerts]
poll_interval_secs = 5      # api.weather.gov advertises max-age=4
stale_after_secs = 300      # feed-dead watchdog threshold
extra_events = ["Special Weather Statement"]  # non-VTEC products to accept

[alerts.tiers]              # P-VTEC phenomenon.significance codes
lethal = ["TO.W", "EW.W", "FF.W"]   # tornado, extreme wind, flash flood warnings
severe = ["SV.W", "SQ.W", "DS.W"]   # severe thunderstorm, squall, dust storm
watch  = ["TO.A", "SV.A"]           # tornado / severe thunderstorm watches

[alerts.notify]             # notify-send urgency per tier:
lethal = "critical"         #   none / low / normal / critical
severe = "critical"         #   "none" silences the desktop daemon for that
watch  = "normal"           #   tier (e.g. to run only a script instead)

[alerts.scripts]            # absolute path of an executable to run when an
# lethal = "/home/you/bin/siren.sh"      # alert of that tier fires, in
# severe = "/home/you/bin/log-alert.sh"  # addition to the notification (or
# watch  = "/home/you/bin/log-alert.sh"  # instead, with notify = "none")

[render]
colormap = "threat"  # "threat" (high-contrast), "nws" (classic), "mono"
cold_below_f = 32.0  # temperatures at/below render blue in the HUD
hot_above_f = 95.0   # temperatures at/above render red
```

## Run

```sh
weatui        # interactive radar + alerting TUI
weatui -d     # headless daemon: notifications only, no UI
```

## Keys

![help overlay](assets/help.png)

*The `?` overlay, here on top of a forecast frame — the storm core HRRR
predicts is the yellow patch at the top.*

| | |
|---|---|
| `h` `j` `k` `l` | pan west / south / north / east |
| `C-d` `C-u` | pan half a screen |
| `zi` `zo` | zoom in / out |
| `gh` | recentre on home |
| `[` `]` | previous / next frame |
| `gg` / `G` | oldest frame / newest (follow live) |
| `space` | play / pause the loop |
| `<` `>` | slower / faster playback |
| `1` `2` `3` | base layer: reflectivity / echo top / VIL |
| `4` `5` `6` `7` | toggle augmentation: velocity / debris (CC) / ZDR / spectrum width |
| `f` | forecast horizon: +2 h (15-min steps) / +6 h / +18 h |
| `?` | help overlay |
| `q` `Esc` `ZZ` `C-c` | quit |

### Layers

**Base layers** (`1`–`3`) exist on both the observed and forecast halves of
the timeline, so scrubbing past "now" never blanks the map: reflectivity,
echo top, and vertically integrated liquid each have a NEXRAD-derived
observed form and an HRRR-forecast form of the same quantity.

**Augmentations** (`4`–`7`) are the radar-only moments — the ones no model
can forecast because they are properties of the radar pulse itself. They
paint *over* the base wherever the reading is diagnostic: strong inbound or
outbound velocity (rotation), a correlation-coefficient collapse inside
strong echo (lofted debris — the radar-confirmed-tornado signature),
anomalous differential reflectivity (hail), broad spectrum width
(turbulence). Velocity and debris detection are **on by default**; they only
draw inside ≥ 30 dBZ echo, so a clear night's insect layer cannot paint
false rotation. On forecast frames they simply have nothing to add and the
base keeps rendering.

## Notifications

Alerting works in both modes (`weatui` and `weatui -d`) and shells out to
`notify-send`, so it plugs into whatever notification daemon you already
run and styles with the urgency levels you have already configured (for
example mako's `[urgency=critical]` section):

- New warnings fire once, at the urgency configured for their tier —
  summary `[LETHAL] Tornado Warning`, body with the NWS headline, the area,
  and an estimated arrival time computed from the alert's storm-motion
  vector against your location.
- Notification text is plain ASCII on purpose: your notification daemon's
  font may not carry Nerd Font glyphs, and tofu in a tornado warning is
  unacceptable.
- Each tier can also run a **custom script** (`[alerts.scripts]`): the
  executable is spawned with the tier and event as arguments and the full
  alert in its environment — `WEATUI_TIER`, `WEATUI_EVENT`,
  `WEATUI_HEADLINE`, `WEATUI_AREA`, `WEATUI_ETA_MINUTES`. Scripts run in
  addition to the desktop notification, or instead of it when the tier's
  notify level is `"none"`. Script and notification fail independently —
  a broken script never silences a warning.
- If no poll of api.weather.gov has succeeded for `stale_after_secs`, a
  critical **ALERT FEED STALE** notification tells you that you are *not*
  currently protected — a dead poller must never be indistinguishable from
  calm weather.

By design, only life-safety products are alerted: tornado, extreme wind,
flash flood, severe thunderstorm, squall and dust storm warnings, tornado
and severe thunderstorm watches, and Special Weather Statements (which
carry storm motion, gust and hail data despite having no VTEC). Air
quality, heat, marine and surf products are deliberately out of scope.

## Data sources

| What | Where |
|---|---|
| Live radar | NEXRAD Level II realtime chunks (AWS Open Data) |
| Radar history | NEXRAD Level II archive |
| Forecast radar | HRRR `wrfsubhf` sub-hourly GRIB2, byte-ranged via `.idx` (AWS Open Data) |
| Alerts | api.weather.gov CAP/GeoJSON |
| Conditions | api.weather.gov station observations |
| Geocoding | embedded Census ZCTA centroids (offline) |
| Timezone | api.weather.gov points — times display in the *watched* location's zone, not the machine's |

All US-government sources; no API keys required.
