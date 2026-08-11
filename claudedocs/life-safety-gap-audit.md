# weatui life-safety gap audit

**Scope:** gaps in alerting, analysis, tracking, and introspection that could cause a user to miss, receive late, or misread a life-threatening hazard.

**Tree state:** HEAD `fb029c2` plus uncommitted in-flight cell-tracking work in `src/radar/cells.rs`, `src/render/hud.rs`, `src/render/labels.rs`, `src/render/overlay.rs`, `src/tui.rs`. Those five files were being edited during the audit, so their line numbers will drift. Anchor on symbol names, not line numbers, in those files. Everything else is stable.

**Method:** three parallel evidence lanes (alert ingestion/delivery, radar analysis/tracking, introspection/failure-visibility) plus independent verification of every load-bearing claim. Two findings were settled against the live `api.weather.gov` feed (248 active alerts). Nothing here is build-verified; `cargo` was not on PATH in the audit shells.

**Severity key**

| Tier | Meaning |
|---|---|
| FATAL | User misses a life-threatening hazard, or is actively misled in the reassuring direction |
| SEVERE | Hazard delivered late, degraded, or ambiguous |
| MODERATE | Correctness or discoverability defect with a plausible path to harm |
| MINOR | Real but low-consequence or low-likelihood |

Counts: 12 FATAL, 15 SEVERE, 23 MODERATE, 9 MINOR.

---

## FATAL

### F1. The VTEC identity key drops significance, and it suppresses tornado warnings

`vtec.rs::event_key` returns `(office, phenomenon, etn)`; `state.rs::key_of` formats it as `OFFICE.PHENOM.ETN`. Significance (`W` warning vs `A` watch vs `Y` advisory) is discarded. NWS sequences ETNs independently per office per phenomenon **per significance**, so two different products routinely share a key.

`state.rs::ingest` notifies only when `!self.notified.contains(&key)`, and `retain` holds the key while either product is in the feed. First key wins, permanently, for the life of the pair. Three distinct failure modes, all live:

- **Watch first:** the tornado warning is never notified. Silent.
- **Warning first:** the toast fires, then the unconditional `active.insert` lets the *watch* overwrite the warning. The HUD renders Tornado Watch at Watch tier, yellow not red, with no `instruction`, no `tornadoDetection`, no `damageThreat`. The display contradicts the toast while a tornado warning is live. `/alerts/active` returns newest-first and the warning is issued inside the watch, so this is the likelier production ordering.
- **Cross-deletion:** when the watch expires while the warning is live and is processed second, `terminates_event` runs `active.remove(&key)` on the shared key and **deletes the live tornado warning** from the HUD, then `notified.remove` causes a spurious re-notify next poll.

Collateral: `daemon.rs::eta_minutes` looks up `active()` by the same key, so it can read motion off the wrong product and ship a notification with no ETA.

**Settled against live data.** The test suite assumes watches carry office `KWNS` (`state.rs` severe-watch test, `filter.rs` watch test). That assumption is false. In a 248-alert live snapshot, **0 of 30 watch VTECs carried KWNS**; every one carried the local WFO (KJKL, KREV, KIND, KIWX, KTWC, KCLE, KRLX, KILN, KPBZ, KSTO, KDVN, KLWX, KILX, KLMK, KCTP, KRNK, KSGF). Because every watch fixture uses an office that can never match a local warning, no test could ever have caught this.

Live near-misses in that same snapshot:

| Office | Phenomenon | Watch ETN | Warning ETN | Gap |
|---|---|---|---|---|
| KSGF | XH (extreme heat) | 2 | 3 | **1** |
| KLWX | FA (areal flood) | 14 | 16 | 2 |
| KRLX | FA | 13 | 56 | 43 |
| KIND | FA | 12 | 127 (Y) | - |

For locally numbered pairs (FF.A/FF.W, FA.A/FA.W, WS.A/WS.W, BZ.A/BZ.W, IS.A/IS.W, HU.A/HU.W, TR.A/TR.W, CF.*, FW.A/FW.W, XH.A/XH.W) both sequences restart at 1 each January and stay in the same band all year, so collision is the expected state, not a coincidence. For SPC-numbered convective watches (TO.A, SV.A) against local warnings, both counters are monotonic in the same hundreds band and necessarily cross during the year; for TO specifically the crossing lands at low watch numbers against an office's first ~60 tornado warnings, which is January through April in Dixie Alley.

**Fix:** include `significance` in `event_key`. One line.

### F2. Radar hazard detection has no path to notification, and daemon mode has no radar at all

`cells.rs` computes tornado debris signatures, rotation, hail, and 26 m/s wind hazards. Every notification in the program originates from `state.rs::ingest`, fed only by NWS alerts. The only `notify::` call sites are two in `tui.rs` and two in `daemon.rs`. A debris ball 5 km from home produces zero notification.

Worse: `daemon::run` constructs only an `AlertEngine`. `weatui -d`, the unattended mode, has **no radar analysis whatsoever**. The best independent detector in the program only works when someone is already looking at the screen, which is the case where they least need it.

### F3. The staleness watchdog lives inside the task it watches

`app.stale` / `app.stale_secs` are written in exactly one place, from an `AlertSnapshot` produced by the spawned alert task. Verified negatives: no independent timer in the event loop (its only time branch is the playback advance), no `set_hook` anywhere in `src/`, no liveness check, and all four `JoinHandle`s dropped at their spawn sites. There is no `JoinSet`, no abort-on-panic, no supervisor.

The alert task is a bare unsupervised `tokio::spawn` sending into a single bounded `mpsc::channel::<Update>(16)` shared with the radar, forecast, conditions, and counties tasks, and the forecast task bulk-sends. If the alert task panics **or blocks on `send().await` because the channel is full**, the UI paints the last known alert list and `stale: false` forever. The red `FEED STALE - YOU ARE NOT BEING WARNED` banner can only be drawn by the task whose death it exists to report.

### F4. A laptop resume silently erases the entire blind window

`AlertEngine::tick` calls `mark_poll_success` **before** the `is_stale` check, in the same tick. Two clocks also disagree by design: `tokio::time::sleep` is `Instant`-driven and does not advance during S3 suspend, while `now_epoch` is `SystemTime` and advances by the full wall interval.

- **Case A, network still down on resume:** staleness fires correctly, but the notification says "No successful poll for 180 min" when the machine was merely asleep and the user was never at risk. Indistinguishable from a real dead feed. Every laptop resume trains the user to dismiss the single most important notification the program can send.
- **Case B, network already up on resume:** the poll succeeds, `mark_poll_success(now)` runs, and the `is_stale` check four lines later is false. **The entire multi-hour gap is erased with no banner, no notification, and no record.**

Compounding: the point query returns only currently active alerts. A tornado warning issued and expired during the gap is simply absent, was never in `notified`, and fires nothing. Combined with F14 (no persistence), the fact that a tornado warning covered the user's house for 40 minutes while the laptop slept is **unrecoverable by any means available to this program**.

No `caffeinate`, no `systemd-inhibit`, no idle inhibitor anywhere. A laptop running `weatui -d` with default sleep settings is not a monitor overnight.

### F5. The only channel that can wake a sleeping user is silent, unverified, and has no fallback

`notify.rs::build_args` passes only `-u`, `-a`, summary, body. Verified by grep: **no `-h string:sound-name:`, no `-h string:sound-file:`, no terminal bell, no audio of any kind anywhere in `src/`.** A silent visual notification on a locked, dark screen does not wake anyone.

Every link is unverified:
- No startup preflight that `notify-send` exists or that a daemon is listening.
- `config.rs::validate` checks script paths are absolute but never that they exist or are executable.
- Tier-script exit status is discarded, so a siren that cannot open the audio device reports success.
- Dispatch failure in daemon mode goes to `eprintln`, which is `/dev/null` for any backgrounded or supervised process.
- `send_stale_warning` routes through the **same** `run()` -> `notify-send`. If notify-send is what is broken, the backstop designed to catch it fails identically.
- `flake.nix` declares `x86_64-darwin` and `aarch64-darwin` as supported systems. `notify-send` does not exist on macOS. On a supported platform the program has no notification channel at all.
- `flake.nix` ships `devShells` only: no package output, no NixOS or nix-darwin module, no systemd unit, no launchd agent, despite the README calling this a daemon. If the process dies at 3am, nothing restarts it.

### F6. The default allowlist is convective-only, and cannot be extended for any VTEC product

`config.rs` defaults: lethal `TO.W EW.W FF.W`, severe `SV.W SQ.W DS.W`, watch `TO.A SV.A`. `filter.rs::classify` returns `None` from inside the VTEC branch, so the `extra_events` escape hatch is **unreachable for anything carrying a VTEC**.

Silently dropped and not fixable by config: `TS.W` tsunami, `SS.W` storm surge, `HU.W`/`TR.W` tropical, `XH.W` extreme heat, `BZ.W`/`IS.W`/`WS.W` winter, `FA.W`/`FL.W` areal and river flood, `CF.W` coastal flood, `AV.W` avalanche, `HW.W` high wind, and the non-weather emergency set `EVI` evacuation-immediate, `CDW` civil danger, `HMW` hazmat, `RHW` radiological, `NUW` nuclear power plant.

`FA.W` and `FL.W` were **active in the live feed during this audit**. They are drowning-class products, `FF.W` is already lethal-tier, and `FA.W` is in no tier at all.

Two related defects:
- The heat rejection is *deliberate*: a test asserts Extreme Heat Warning should be rejected. Heat is the leading US weather killer in most years. That is a design decision worth revisiting, not an oversight.
- That test pins the **retired** code `EH.W`. NWS migrated to `XH.W`, and the live feed carried 7 `XH.W` plus 1 `XH.A`. `XH.W` appears nowhere in the tree.

### F7. A volume with no parseable time range is stamped with the current wall clock

`fetch.rs:309` and `fetch.rs:350` both end in `.unwrap_or_else(Utc::now)`. When a decoded NEXRAD volume has no readable time range, the frame is stamped **now** instead of the observation time.

This is strictly worse than having no age display. It fabricates freshness for data of unknown age, it defeats any staleness indicator added later, and it corrupts `ring.push` dedup and ordering, which match on `captured_at`.

### F8. Escalation to a tornado emergency produces no notification at all

When NWS upgrades a tornado warning to a tornado emergency it issues an SVS carrying the **same ETN**. The key is unchanged, `notified` already contains it, `fresh` stays empty. `state.rs` does replace the stored alert so the HUD's red bar appears, but only for someone watching the screen.

A sleeping user with the daemon running is notified once, for the routine warning, and never again as the event escalates to the most severe product NWS issues.

Compounding: even the *first* notification cannot convey severity. `notify.rs::body_for` uses only headline, area, and ETA. It omits `damageThreat` (CATASTROPHIC = tornado emergency, CONSIDERABLE = PDS), `tornadoDetection` (OBSERVED vs RADAR INDICATED), and `instruction`, which `alert/mod.rs` itself documents as "the most important string in the payload." All three reach the HUD. None reach the toast. `ThreatTier` also tops out at `Lethal` for any `TO.W`, so a tornado emergency cannot outrank a routine warning in urgency either.

### F9. The rotation detector is biased toward "no rotation" through three independent mechanisms

- **Aliasing.** `ROTATION_SPAN_MS = 40.0`, and `cells.rs` states outright that velocity is not dealiased. WSR-88D Doppler-cut Nyquist is roughly 25-35 m/s depending on VCP and PRF, which the app never inspects. A violently rotating couplet exceeding Nyquist folds and reads as a *small* span, so it is classified Strong and gets no `T` letter: **the stronger the tornado, the more likely it is missed.** Conversely a benign fold boundary produces an artificial ~2x Nyquist span (50-64 m/s) and a false Rotation. The 40 m/s bar is essentially only reachable by an aliasing artifact or an unaliased extreme.
- **Lattice.** `STEP_DEG` is ~2.2 km and `LOCAL_RADIUS_CELLS` is +/-6 km. Shear is the max difference between two ~2 km-spaced point samples; the operational quantity is gate-to-gate at ~250 m. This under-resolves the couplet peak, always downward.
- **Classification gap.** `classify()` requires rotation >= 25 **and** CC < 0.85 for Debris, but >= 40 for Rotation. A cell rotating 25-39 m/s without a debris signature is classified Intense or Strong, gets no `T` hazard letter, sorts lower, and can be evicted by the 8-cell cap. The code trusts 25 m/s as corroboration for debris but not as rotation on its own.

These are one compound defect, not three. Fixing dealiasing changes the calculus for the other two.

### F10. Forecast frames are pixel-identical to observed radar in the map area

`tui.rs` rasterizes `ring.current()` with no styling difference for `frame.projected`. The map is identical for observed NEXRAD and an 18-hour HRRR forecast (`MAX_LEAD_MINUTES = 1080`). The only cues are one row: the ` FCST` text and dashed rune in the timeline.

`ring.rs::advance_playback` walks observed and projected frames indiscriminately, so a running loop cycles the map between measurement and model with no visual change. `hud.peak_dbz` is computed from the forecast grid and printed unqualified.

### F11. Interpolated frames scale dBZ linearly, fading arriving storm cores into drizzle

`interp.rs`:

```rust
(Some(x), None) => Some(x * (1.0 - self.alpha)),
(None, Some(y)) => Some(y * self.alpha),
```

dBZ is a logarithmic unit; scaling it linearly is not a meaningful operation. At the **leading edge of an advancing storm** only `after` has echo, so a 55 dBZ core at alpha 0.25 renders as **13.75 dBZ**, light blue drizzle on the Threat ramp where the model says a severe core is arriving. Fails in the reassuring direction, on the arriving edge, on three of every four forecast frames at 15-minute steps.

### F12. A stalled consumer freezes all five producers into a calm-looking screen

The channel is `mpsc::channel::<Update>(16)` with five producers (conditions, alerts, radar, forecast, and counties riding the conditions sender). Every send is a blocking `.await`; there is no `try_send` anywhere. The consumer is `while let Ok(update) = rx.try_recv()`, which runs **after** `term.draw(|f| app.draw(f))?`.

If the consumer stops for any reason (`term.draw` hanging on a wedged tty, SIGSTOP, a terminal that stops reading, an ssh session frozen rather than closed), all five producers block within at most 16 messages. The alert loop never reaches `engine.tick()` again. **Polling stops, notifications stop, and staleness evaluation stops together**, and the screen is frozen on the last good frame showing a plausible radar picture and "no active warnings". No timeout on any send, no `try_send` fallback, no watchdog, and no send that prefers dropping a forecast frame over blocking an alert.

This reaches the same circularity as F3 without requiring a panic.

Two things to state accurately rather than inflate:

- **Backpressure cannot suppress a notification.** In the alert task the ordering is `tick()`, then `dispatch`, then `send_stale_warning`, then the snapshot is built, and only *then* `alert_tx.send().await`. Every notification has already fired before the task can block.
- **Head-of-line blocking is bounded and is not a hazard.** The forecast task can enqueue up to 72 frames in a tight loop and will fill 16 slots, but `try_recv` drains everything available in one pass, so an alert snapshot queued behind forecast frames still lands in the same event-loop iteration. Cost is one draw, roughly 40ms.

Minor corollary: `stale_secs` is computed before the send, so if the send blocks the HUD renders a stale age understated by the block duration.

---

## SEVERE

### S1. The ETA is never aged, in two independent code paths

`motion.rs::eta_to` never references `self.observed_at`. The error equals the vector age exactly, independent of distance and speed: `reported = true + (now - observed_at)`. At 30 kt a 15-minute-old vector hides 13.9 km of travel, so "arrival in 20 min" means 5. There is no floor, so a vector older than the true ETA returns a positive number for a storm already overhead.

`hud.rs` does display `(vector Nm old)` beside it, but `notify.rs::body_for` emits the bare number with no age and no correction, and `script_env` exports `WEATUI_ETA_MINUTES` with no age field, so no user script can correct it either.

The same arithmetic error exists independently on the radar side: `Approach` is computed from the centroid at volume time and rendered verbatim, where volume age is 4-6 min of scan plus assembly plus `refresh_secs`. Two un-aged ETAs from two code paths, on two different HUD rows, from two different time origins, never reconciled. If the radar feed dies, `app.cells` is never cleared and that number freezes without counting down.

### S2. Nothing on screen says the beam is above the tornado

`fetch.rs::beam_height_km` already implements the 4/3-effective-earth model. It is called only from `echo_top_km` and `vil`. **Nothing displays it.** At 0.5 degrees:

| Range | Beam center AGL |
|---|---|
| 30 km | 0.31 km |
| 60 km | 0.74 km |
| 75 km | 0.99 km |
| 100 km | 1.46 km |
| 150 km | 2.63 km |

Tornadic circulations are diagnosed below ~1 km AGL, so past roughly 60-75 km the lowest cut is already above the layer that matters. `MAX_CELL_DISTANCE_KM` is 75 km **from home**, while home can sit 100+ km from its site. The app computes exactly the number that would tell a user "you are past useful range for low-level features" and never shows it.

### S3. "Not rotating" and "cannot tell" render identically

Rotation is silently suppressed beyond `ROTATION_MAX_RANGE_KM = 150` from the site by skipping velocity samples. When suppressed, `rotation_ms` is `None`, and the HUD simply omits the `dv` row. Absence of evidence renders as evidence of absence.

### S4. The dual-pol clutter filter can erase the debris ball it exists to reveal

`DEFAULT_MIN_CORRELATION = 0.90`, enforced fail-closed: a reflectivity gate whose co-located CC is below 0.90, or which has no valid CC, is dropped. Tornado debris runs CC 0.3-0.8 **by definition**. Large hail runs 0.85-0.95. The melting-layer bright band also falls below it. 0.90 is not configurable; `from_scan_with` is reachable only from tests.

The map draws a **hole** where the debris ball is, and `peak_dbz` understates hail. Partial mitigation the author did get right: `value_at(Reflectivity)` is a column max, so a tilt above the debris layer with good CC can still contribute. That saves the common case and fails where debris fills multiple low tilts, which is a strong tornado.

Worse: the CC overlay, the one product that is supposed to *show* debris, is gated on surviving reflectivity. `AUG_MIN_REFLECTIVITY_DBZ = 30.0` is tested against the CC-masked composite before any overlay pixel is painted. Where debris masks out low-level reflectivity, the CC overlay cannot paint there. The velocity overlay has the identical gate, so a couplet in a rain-free base or a weak-echo notch is not drawn either. Both overlays are on by default.

### S5. Warnings become visible only once they already cover you

`poll.rs::alerts_url` queries `?point=lat,lon`. No radius, no adjacent-county awareness, no second location for family, work, or travel. A tornado warning one county upstream and inbound is invisible until the polygon reaches you.

### S6. No radar or conditions staleness anywhere

`stale`/`stale_secs` exist only for the alert feed. `captured_at` is **never** compared to `Utc::now()` in non-test code; the only two age computations in the program are for surface observations and the motion vector. If the radar task dies the ring keeps animating old frames indefinitely, with timestamps, with no banner. Compounded by F7, which can stamp unknown-age data as current.

### S7. A single malformed feature discards the entire alert batch

`poll.rs` does one `resp.json::<AlertCollection>()`. `Properties.event` is a required `String`, and `pub type Ring = Vec<[f64; 2]>` rejects any GeoJSON position carrying an elevation third element. One bad feature loses every alert in the response, including a tornado warning. Partly mitigated: `mark_poll_success` is not called on the error path, so staleness eventually fires, after 300s, which is most of a tornado warning's life.

### S8. A 200 with a changed body shape zeroes everything and counts as success

`AlertCollection.features` is `#[serde(default)]`. A 200 whose body lacks `features` (schema change, error envelope, captive portal) parses to zero alerts, clears all active state, calls `mark_poll_success`, and renders a green "no active warnings". **This is the one failure mode the staleness backstop is structurally incapable of catching, because the poll succeeded.**

### S9. A malformed VTEC on a tornado warning silently discards it

`vtec.rs::parse_all` uses `filter_map(...ok())`. If every VTEC line fails to parse, `alert.vtec` is empty, `primary_vtec()` is `None`, and `filter.rs::classify` falls through to the `extra_events` check, which by default contains only "Special Weather Statement". The tornado warning is discarded and never notified. No log, no counter, and per F14 no way to discover it happened.

### S10. Zone-based warnings render as though they are somewhere else

`alert/mod.rs::contains` returns `false` when geometry is null. Of 43 live alerts in products weatui watches, **11 (26%) had null geometry, including all 9 active Severe Thunderstorm Watches**. Probing the exact URL form `poll.rs` builds against a point inside one of those zones returned `Severe Thunderstorm Watch geometry=NULL` and `Flood Watch geometry=NULL`.

Bounded correctly: this is display-only, not suppression. `contains` has one non-test caller and `classify` never looks at geometry, so these alerts still notify. The harm is that the app teaches the user "real threat = polygon on the map + `[YOU]` tag", then denies both to every tornado and severe watch.

### S11. The nearest-cell rescue can evict a tornadic cell

The `MAX_CELLS = 8` truncation is otherwise safe: the sort is descending by threat, so a 9th cell cannot evict a lethal one. But the nearest-cell rescue does `truncate(MAX_CELLS - 1)` then `push(keep)`. When the nearest cell ranks 9th or worse, the **8th-ranked cell is dropped**. In an outbreak with eight Debris or Rotation cells inside 75 km, the weakest-but-nearest cell evicts a tornadic one. `MAX_CELLS` is not configurable.

### S12. Effective cell-tracking lead time is shorter than 75 km implies

`CellTracker` needs two fixes; velocity is `None` when `prev` is `None`. The first volume in which a storm crosses 75 km yields `motion=None` and `approach=None`. Usable arrival time does not exist until the **second** volume, so the real horizon is 75 km minus one VCP of travel: about 35 min at 60 kt, not 48. Internally inconsistent with `MAX_ETA_MINUTES = 120`, a horizon the 75 km gate makes unreachable for any closing storm.

### S13. Panic leaves a broken terminal and no message

No `impl Drop` anywhere in `src/`, no panic hook. `restore()` is a plain function called at exactly one place, on the normal and `Err` return paths of `event_loop`. A panic unwinds straight past it: the terminal is left in raw mode inside the alternate screen, and the panic message is printed **into** the alternate screen, which the terminal then tears down, so the user frequently sees nothing at all explaining the disappearance. No signal handling anywhere (tokio's `signal` feature is enabled in `Cargo.toml` but never used), so SIGTERM/SIGHUP behave the same.

### S14. A valid-but-wrong location is undetectable

A typo'd ZIP that happens to be valid (73018 for 73019, or an adjacent town) resolves cleanly to the wrong centroid and polls healthily and greenly forever. **The resolved location is never displayed in either mode.** `home: Coords` reaches the HUD but is used only for `alert.contains`; the startup status line naming site and distance is overwritten within 5 seconds. In daemon mode the location echo is dead code (see M1). A 40 km error is undetectable, and 40 km is the difference between inside and outside a warning polygon.

### S15. The stale warning bypasses scripts, so the documented "script only" setup gets no staleness signal at all

`dispatch` fans out to both `notify-send` and the tier script, and they fail independently. `send_stale_warning` calls `run()` directly and **never consults `Scripts`**.

The README documents exactly the configuration this breaks: `[alerts.notify]` says `"none"` silences the desktop daemon for a tier "e.g. to run only a script instead", and `[alerts.scripts]` is presented as the alternative. A user following that documented pattern receives every alert through their script and receives **zero** staleness signal, because the one message meaning "this system is broken" is routed exclusively through the channel they have deliberately turned off, which is also the channel most likely to be broken.

---

## MODERATE

### M1. Daemon mode is mute about itself
`main.rs` is the only caller of `daemon::run` and passes `echo_to_stdout=false`, making **four** diagnostics dead code: the location echo, the fired-alert echo, "alert feed stale for Ns", and the poll-failure report. `weatui -d` never states what it is watching and never reports a poll error.

### M2. `[alerts.tiers]` replaces rather than merges
Each field is independently `#[serde(default)]`. A user writing `lethal = ["TS.W"]` to add tsunami warnings **silently deletes `TO.W`** and stops receiving tornado warnings. Nothing validates that a tier is non-empty. This is worse than the gap it is meant to fix, because it is user-induced and silent.

### M3. `notify.lethal = "none"` with no lethal script is a silent total disable
`notify.rs::send` returns `Ok(())` early at `Urgency::None`, and validation never checks that a silenced tier has a script replacement. Parses, validates, runs, and lethal alerts vanish.

### M4. No `deny_unknown_fields` on any config struct
`stale_after_sec`, `[alerts.notifiy]`, `poll_interval` all silently revert to defaults. A typo'd `[alerts.scripts]` header means the absolute-path validation never runs, so the script silently never fires.

### M5. Blocking process spawn inside the async alert loop, no timeout
`Command::new("notify-send").status()` is a blocking wait, called from async with no `spawn_blocking`. With an unreachable session bus, notify-send can hang for the D-Bus timeout (~25s) per call. N fresh alerts in an outbreak serialize into N x 25s with polling stopped and no snapshot emitted, compounding F3.

### M6. JSON-parse failures never trigger backoff
`poll.rs` sets `consecutive_failures = 0` on a 2xx **before** the body is read. A persistently unparseable response polls at the 5s base interval forever, and `next_delay()` reports a healthy cadence for a poller that has parsed nothing.

### M7. No local expiry enforcement
`properties.expires` is parsed for display only. `AlertState` never reads it; removal depends entirely on the alert leaving `/alerts/active`.

### M8. 429 handling ignores `Retry-After`
Treated as a generic failure with blind exponential backoff.

### M9. Timezone silently degrades to UTC
`Err(_) => chrono_tz::UTC` with no message. Every "until HH:MM" in the alert block then reads UTC. `%Z` prints "UTC" so it is detectable, but "until 03:15" during a 22:15 local tornado warning reads as five hours of margin instead of one.

### M10. No re-notification on in-place upgrade
Beyond the tornado-emergency case in F8: a CON carrying an extended expiry or a redrawn polygon produces no new notification.

### M11. No flash-flood or rainfall-accumulation capability
No radar QPE, no storm-total precipitation, no rate product. The only precipitation figure in the program is `precipitationLastHour` from a single METAR station. Flash flooding is the leading US weather killer in most years and the radar side is blind to it.

### M12. Lightning and hail are proxies, not measurements
`LIGHTNING_ECHO_TOP_KM = 9.0` stands in for charge separation; there is no GLM or ENTLN feed, so the `L` letter is an inference. Hail fires on `VIL >= 45` **or** `echo top >= 14 km`: no MESH, and no VIL-density normalization against the freezing level, which is the standard discriminator. Raw VIL over-flags in warm airmasses and under-flags cool-season and low-topped hailers; a 14 km top alone is routine in Plains summer convection, and the `OR` means either weak criterion alone fires.

### M13. No storm-relative velocity
Only raw radial velocity is carried. Motion **is** measured by `CellTracker` but is never subtracted from the velocity field, so the operator cannot see the mesocyclone the way SRV would show it. `cells.rs` documents the consequence ("a mesocyclone in strong flow can be all-inbound") and works around it with sign-free shear rather than fixing it.

### M14. Cone of silence and beam blockage are unmodelled and unannounced
Max VCP elevation is 19.5 degrees, so there is no data above 3 km AGL within 8.5 km of the site, and none above 10 km within 28 km. Echo top and VIL therefore under-report for a user living near their radar, and both feed the hail classifier. No terrain mask anywhere.

### M15. No RDA outage handling, and the status line hides it
One site is chosen and one site string is accepted. No secondary, no compositing, no failover. On failure `app.status` is set once and the next alert tick unconditionally overwrites it with a healthy-looking "N active | radar KXXX | M frames". The empty-map explainer is gated on `ring.current()` being `Some`, so a never-populated ring draws nothing on the map at all. VCP number is never inspected anywhere, which also feeds F9.

### M16. Display resolution is coarser than the hazard, unannounced
Half-block pixels give `viewport.height = rows * 2`; at the 260 km default that is ~3 km per pixel on both axes. A tornado, a hook echo, and a debris ball are all sub-pixel. Nothing warns the user and there is no zoom-to-threat.

### M17. No hook echo, BWER, or shape analysis
Cell ID is a 4-connected flood fill on one dBZ threshold producing a centroid and a max. No shape descriptor exists, so hook echo, inflow notch, and bounded weak echo region are undetectable. CC and velocity carry **all** the tornado information in this program, and both are compromised per S4 and F9.

Also in this tier, from the swallowed-error sweep:

- **M18.** `conditions.rs` `parse_from_rfc3339(t).ok()`: a malformed observation timestamp yields `observed_at=None`, and the HUD then renders no age suffix at all, silently removing the conditions panel's only staleness cue.
- **M19.** `tui.rs` forecast task: `let _ = forecast_tx.send(...)` then `return`. If the HTTP client fails to build, the forecast task exits permanently and silently at startup.
- **M20.** `alert/mod.rs::motion` `StormMotion::parse(&s).ok()`: a malformed motion vector silently yields no ETA; the user cannot distinguish "no motion data" from "motion data present but unparseable" during a closing storm.
- **M21.** `state.rs::key_of` fallback when there is no VTEC and no id is `format!("{event}|{area}")`. Two distinct concurrent warnings with the same event and area collapse to one key; the second is deduped away.
- **M22.** Nonexistent radar site is caught in the TUI but **not** in daemon mode, which never constructs `App`.
- **M23.** `radar.refresh_secs` is silently clamped up to 30; `refresh_secs = 0` and `stale_after_secs = 0` both pass validation entirely.

---

## MINOR

- **N1.** `unreachable!` sited in the notification path (`config.rs`). Guarded today by the `Urgency::None` early return, but it is a panic in the alert dispatch path one refactor from killing the process mid-warning.
- **N2.** Flapping re-notify: an alert that momentarily drops out of one poll response is re-notified when it returns. Fail-loud, so low priority.
- **N3.** `daemon.rs::now_epoch` `.unwrap_or(0)`: a pre-1970 clock makes `now_epoch` 0, so `is_stale` computes `0.saturating_sub(t) = 0` and **staleness is silently disabled entirely**. Absurd precondition, but it is a total-watchdog-defeat path.
- **N4.** `tui.rs` backfill error path uses `let _ = radar_tx.send(...)` while every other send in that task uses `if ...is_err() { return; }`.
- **N5.** Hazard letters are drawn at `x.saturating_add(4)`, four **terminal columns** east of the cell: ~13 km at the 260 km default span, ~45 km at the 900 km max. The offset is always in one fixed compass direction rather than radially away from the echo. Hazard letters also render on forecast frames while cell markers correctly do not, so `T`/`H`/`W` paint at observed centroids on top of an 18-hour forecast raster.
- **N6.** QLCS and low-topped tornadic cells fall below the detector floor (`CELL_MIN_DBZ = 40.0`, `MIN_SAMPLES = 4`, ~16 km^2). A cool-season low-topped tornadic cell or a QLCS mesovortex in a broad stratiform shield may never form a distinct >=40 dBZ blob. Also, the cell scan box is centred on the **radar site** while the distance filter is relative to **home**, so coverage of the 75 km home radius is asymmetric at the box corners. And `cells::scan` is ~49k synchronous `value_at` calls inside an async task with no `spawn_blocking` and no timeout.
- **N7.** HRRR cycle age is never displayed. `latest_cycle` walks back up to **six hours** looking for a published `.idx` and returns whatever it finds with no age attached; the label is measured from cycle time, not staleness. A 5-hour-old run is presented with the same authority as a fresh one.
- **N8.** Interpolated frames are labelled distinctly at the data layer but there is no third UI state: the timeline cannot distinguish "HRRR model output" from "our own advection guess between two HRRR steps". Three of every four frames at 15-minute steps are synthetic.
- **N9.** Colormap dominance is correct (saturated red at 50 dBZ, magenta at 60, white at 70) but colorblind safety is asserted only in raw RGB space, with no CVD simulation in the suite. Under deuteranopia the 35 dBZ green and 40 dBZ yellow collapse toward each other, and that is exactly the boundary `CELL_MIN_DBZ = 40` keys on. A `Mono` ramp exists but nothing steers a colorblind user toward it.

---

## The unattended chain, end to end

Every link that can fail silently between a tornado warning landing in the poll response and the user waking up:

| Link | Failure | Visible? |
|---|---|---|
| L0 | Machine suspended; F4 Case B erases the evidence on resume | Silent |
| L1 | Alert task dead (F3) or parked on a full channel behind a stalled consumer (F12) | Silent |
| L2 | Network down | Caught, but only via L7 |
| L3 | Body fails to deserialize (S7); no backoff (M6) | Loud, eventually |
| L4 | VTEC fails to parse; warning discarded (S9) | Silent |
| L5 | `classify` finds no matching tier; fail-closed, no log (F6) | Silent |
| L6 | Dedup key collision drops the warning (F1, M21) | Silent |
| L7 | `notify-send`: no sound, no bell, no fallback, no verification, absent on macOS (F5) | Silent |
| L8 | Tier script exit status discarded | Silent |
| L9 | Dispatch failure to an unread stderr (M1) | Silent |
| L10 | Stale warning bypasses scripts, so a `notify="none"` + script setup gets no staleness signal (S15) | Silent |
| L11 | Process died; nothing restarts it (F5) | Silent |

**One sentence:** the program's only mechanism for waking a sleeping user is a silent visual desktop notification, on a machine that may itself be asleep, delivered by a task that may be dead, over a channel it never verifies, with no sound, no fallback, no retry, no restart, and no record that any of it happened.

---

## What is already good

Worth stating so effort does not go here.

**Alerting.** Conditional ETag polling with exponential backoff and a 300s ceiling. NWS-compliant User-Agent with contact details, enforced by test. The lethality allowlist is deliberately an allowlist over P-VTEC rather than trusting CAP `response`, with the reasoning documented from live-data observation. The parsers are careful and well tested; the weakness is what happens to an alert *after* it parses.

**Staleness display, where it fires.** The red banner states plainly that warnings are not arriving, and `no active warnings` is correctly suppressed while stale. `is_stale` correctly returns true for a never-polled state. The mechanism is right; F3 is about the liveness of the reporter.

**Storm motion.** The NWS FROM/TOWARD 180-degree inversion is correct, heavily documented, and regression-tested. Closest-point-of-approach is correct vector math and returns `None` for a receding cell.

**Cell tracking.** Greedy nearest-first association with a time-widening gate, exponential smoothing, a 20-minute gap bail-out that starts fresh tracks rather than inventing jumps, and a 1.5-minute guard against re-polled duplicate volumes. Backfill is fed chronologically so motion exists at startup.

**Radar reasoning.** Squall-line geometry correctly rejected as rotation via local rather than cluster-wide shear pairing, tested. Debris CC is couplet-local so a distant hail core cannot fake a TDS, tested. Column reduction is physically reasoned (composite max for reflectivity to match HRRR REFC, lowest cut for CC/velocity/ZDR) with a regression test. VIL caps Z at 56 dBZ so hail cannot inflate the integral. Echo top is reported MSL to match HRRR RETOP so the observed/forecast handoff does not jump. Derived quantities are labelled as floors ("VIL >=", "top >=").

**Projection.** Clean, and this was checked specifically for offset bugs. Everything on the map goes through one equirectangular `Viewport`: radar raster, warning polygons, county borders, home crosshair, cell markers, distance rings. Counties reuse the alert `Ring` type so there is one shared GeoJSON `[lon, lat]` convention with no transposition. The inverse is asserted exact to 1e-9, pixels are asserted square in ground distance to within 2%, and the floor-vs-round choice is documented. HRRR is projected separately in its own Lambert Conformal on the GRIB2 spherical earth, and the GRIB scanning mode is **validated rather than assumed**, with a comment noting that any other mode would decode cleanly and draw every echo in the wrong place. That is exactly the class of bug this audit went looking for, and it is guarded.

**Panic risk is genuinely low.** The complete non-test panic inventory is four sites, all guarded. `tui.rs` has zero non-test `unwrap`/`expect`. The exposure is not that panics are likely, it is that the consequence of one is unbounded and invisible (F3, S13).

**Frame ring.** Insert-by-validity-time and `drop_projected` are correct and well tested, including a late observation arriving after a forecast batch. `leads_every` guarantees `valid_at > now`, so no past frame is mislabelled as forecast.

**Config errors that do fail loud.** Missing config file (names the path and prints a usable stanza), malformed TOML with path context, no location or only one of lat/lon, a ZIP that is not five digits or has no Census ZCTA, and a nonexistent radar site in TUI mode. One hypothesis this audit specifically tested and **falsified**: a positive longitude in the US is *not* a silent failure. Config validation has no bound check of any kind, but `api.weather.gov` returns HTTP 400 `Parameter "point" is invalid: out of bounds`, so the poller fails permanently, backoff engages, and staleness fires. It errs loud.

---

## Critical unknowns

**U1. Do WSR-88D split cuts reach `tilts` with reflectivity?**
`fetch.rs` unconditionally skips any sweep with no reflectivity radial. If the low-elevation Doppler cut is a separate sweep without reflectivity, all near-ground velocity is discarded before rotation analysis ever runs, and F9 goes from "biased low" to "structurally blind". Comments elsewhere in `fetch.rs` imply the author believes Doppler cuts do reach `tilts`, which contradicts the skip unless modern super-res split cuts carry REF on both halves. Not resolvable from source.

**U2. Does multi-line VTEC appear on upgrade products in practice?**
An upgrade product carries both `/O.CAN.KTLX.SV.W.0087/` and `/O.NEW.KTLX.TO.W.0012/`. `parse_all` splits on `\n`, so the author has seen the multi-line form. `state.rs` tests `terminates_event()` on `primary_vtec()` = `vtec.first()`, so if the terminating line is first the alert is skipped entirely: never in `active`, never notified, never rendered, and it repeats every poll so it never self-heals. Total loss, not a missed update. **Zero of 248 live alerts carried multi-line VTEC**, but the feed had no tornado warnings at the time, so that is weak counter-evidence rather than refutation.

**U3. Which resume case dominates in practice?** F4 Case A versus Case B depends on network-stack resume timing that was not measured.

---

## Recommended probes, cheapest first

1. **F1, no network, one test.** In `state.rs` tests: ingest `/O.NEW.KOHX.TO.A.0012/`, then ingest both it and `/O.NEW.KOHX.TO.W.0012/`. Expect a `Lethal` notification on the second ingest. Current code returns an empty `Vec` and leaves one entry in `active()`.
2. **S1, no network, one test.** Call `eta_to` with `observed_at` set 15 minutes in the past and assert against wall-clock truth. Fails today by exactly 15 minutes.
3. **F8, no network, one test.** Build two `Alert`s differing only in `damageThreat` (`None` vs `CATASTROPHIC`), run both through `Filter::classify` and `notify::body_for`, assert the outputs differ. They will not.
4. **F9 classification gap, no network.** Sweep `classify()` over `rotation_ms` 20..45 in 1 m/s steps at `min_cc = 0.97` and assert the hazard letters a user would actually see. Documents the band the current tests skip.
5. **U1 and F9 together, one run.** An `#[ignore]` test against a site with active severe convection that prints, per tilt: elevation, `velocity.is_some()`, `correlation.is_some()`, reflectivity gate count, and the volume's VCP. Then over the cells `scan()` returns, print `rotation_ms`, `min_cc`, peak `|velocity|`, and range-from-site, and compare against the NWS warning text for that storm.

## Highest-leverage fixes

Ordered by consequence removed per line of code changed.

1. **Include `significance` in `event_key`.** One line, closes F1's three failure modes.
2. **Move staleness out of the watched task.** Compute it in the render loop from a locally held `last_snapshot_at: Instant`. Closes F3 and F12, and makes F4 Case B visible by giving the UI a clock that does not depend on the poller.
3. **Check `is_stale` before `mark_poll_success` in `tick`, and carry the gap in the notification.** Closes F4 Case B; distinguishing "asleep" from "feed dead" also fixes the Case A alarm fatigue.
4. **Add an audible path, and route the stale warning through `dispatch`.** `-h string:sound-name:alarm-clock-elapsed` on the lethal tier, plus a terminal bell in TUI mode, plus a startup preflight that `notify-send` exists. Having `send_stale_warning` go through `dispatch` rather than `run` closes S15 in the same change. Closes most of F5.
5. **Route radar-detected Debris and Rotation into `notify::dispatch`, and run the radar pipeline in daemon mode.** Closes F2.
6. **Subtract vector age in `eta_to`, floor at zero.** Closes S1's alert-side half; do the same for `Approach`.
7. **Add `damageThreat`, `tornadoDetection`, and `instruction` to `body_for`, and re-notify when `damageThreat` changes.** Closes F8.
8. **Style projected frames distinctly in the map raster, not just the timeline.** Closes F10.
9. **Interpolate in linear reflectivity (Z), not dBZ.** Closes F11.
10. **Echo the resolved location at startup in both modes and keep it in the HUD.** Closes S14, and un-deadens the diagnostics in M1.
