# Usage guide

## GUI (`nuc-gui`)

Launch from the applications menu ("NUC Control — hardware & power monitor") or
`nuc-gui` in a terminal. Runs unprivileged in your desktop session (Wayland/X11).

Six pages in the sidebar:

1. **Dashboard** — eight tiles: CPU usage, package power, hottest core, GPU
   frequencies, memory, fan, active power profile, session peak power. Delta-based
   tiles show `Unavailable (first interval)` until two samples exist — the app
   never pretends.
2. **Sensors** — one row per kernel temperature/fan sensor, keyed by sensor id,
   with warn/crit limits and alarm/fault flags as reported by hwmon.
3. **Power** — active platform profile from power-profiles-daemon; switch profiles
   via the combo + button (D-Bus → polkit; the switch is confirmed by re-reading
   the daemon). CPU governor/EPP/turbo and RAPL domain energies are shown read-only.
4. **History** — live cairo chart of the current session (CPU temp / package
   power / CPU utilization, each normalized to its own range) plus the SQLite
   database status (samples · minute rows · events) and retention housekeeping.
5. **Diagnostics** — Run checks / Export report. Checks re-sample the system and
   produce statuses `ok / warning / fail / info / unavailable`; export writes
   `diagnostics-report.{md,json}` into `~/.local/share/nuc-control/`.
6. **Settings** — interval, GPU monitoring, recording switch, alert thresholds.
   Save applies the interval live and writes the TOML config.

Alerts (CPU temp warm, NVMe temp warm, fan fault, throttle) surface as desktop
notifications with debouncing (above-samples) + hysteresis + cooldown.

## CLI (`nuc-control`)

One-shot tools; they never touch kernel state.

```sh
nuc-control --cpu                 # model, topology (P/E cores), usage, freqs, package W, EPP, turbo
nuc-control --gpu                 # i915 frequency state + utilization source status
nuc-control --sensors             # all temperatures + fans with limits/alarms
nuc-control --temperatures        # temperatures only
nuc-control --power               # profile daemon state + EPP/turbo + RAPL zone list
nuc-control --health              # per-device SMART via UDisks2 (honest refusal for non-NVMe)
nuc-control --hardware            # DMI/kernel/session identity
nuc-control --diagnostics         # full read-only checks; exit code 2 when a check fails
nuc-control --export-report       # writes diagnostics-report.{md,json}, prints paths
nuc-control --profile balanced    # switch profile; verified by re-read (human + --json forms)
nuc-control --json ...            # machine-readable output
```

Combine read-only section flags to render each section; `--json` emits a combined
object. `--profile`, `--diagnostics` and `--export-report` each run alone, with
optional `--json`. JSON report export returns the report paths and verdict.

## Diagnostics categories

System · Thermal · Cooling · CPU · GPU · Power · Storage · Memory ·
Power policy · Services · History.

Know-what/why on this machine (all documented in the capability report):
- `gpu-utilization` shows `unavailable: intel_gpu_top requires CAP_PERFMON` — by design.
- The `acpi_fan` fan value is annotated as state-uncertain (may encode a control
  state, not a tachometer reading).
- Sensors marked *firmware-backed / cached* are re-read on a 30 s cadence, not
  every tick. This is a deliberate performance trade-off: on this machine some
  ACPI firmware reads block the kernel for ~100–200 ms per access (measured), and
  re-querying them every sample would multiply into seconds per minute of UI
  hang. Fast sensors (CPU cores, NVMe) stay real-time, so thermal alerts are
  unaffected. Cached readings are always explicitly labeled (`cached Ns ago…`);
  no value is presented as live if it is not.
- Third-party power managers (TLP, auto-cpufreq, powerprofiles conflicts) are
  detected and flagged as warnings when active.

## Data locations

| path | content |
|---|---|
| `~/.config/nuc-control/config.toml` | configuration (GUI Save or hand-edited) |
| `~/.local/share/nuc-control/history.db` | WAL-mode SQLite history |
| `~/.local/share/nuc-control/diagnostics-report.{md,json}` | exported reports |

## Troubleshooting

- *GPU tile stuck at "Unavailable"*: `intel_gpu_top` needs privileges
  (`sudo setcap cap_perfmon+ep /usr/bin/intel_gpu_top` is one option); the app
  deliberately does not run as root itself.
- *Profile switch failed*: make sure `power-profiles-daemon` is running
  (`busctl --system` shows both `org.freedesktop.UPower.PowerProfiles` and the
  legacy `net.hadess.PowerProfiles` name; the client tries both).
- *History database errors*: close the app and back up `history.db` together with
  any `history.db-wal` and `history.db-shm` files before attempting recovery. Check
  available disk space and directory permissions. Recording pauses on write
  errors and retries buffered rows. If persistent storage cannot open, the GUI
  shows a warning and uses session-only memory storage.
