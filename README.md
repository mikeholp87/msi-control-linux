# NUC Control — MSI mini PC hardware & power monitor (Linux)

Native Rust/GTK4 desktop app + CLI that reports what the hardware actually exposes:
sensors, CPU/GPU state, RAPL power counters, NVMe SMART health, power-profile state,
history in SQLite, and read-only diagnostics — with honest `Unavailable` markers for
anything the platform does not provide. **Nothing is ever invented, and no fan or
thermal protection is ever modified.**

Verified against: MSI PRO RPL-U Cubi NUC 1M (MS-B0B1), Intel Core 7 150U,
Debian forky/sid, kernel 7.2.12, GNOME/KDE on Wayland. See
`docs/hardware-capabilities.md` for what this machine reports.

## Components

| binary        | what it does                                                            |
|---------------|-------------------------------------------------------------------------|
| `nuc-control` | CLI: `--cpu --gpu --sensors --temperatures --power --health --hardware --diagnostics --export-report --profile <name> --json` |
| `nuc-gui`     | libadwaita GUI: dashboard, sensors, power (profile switch), history chart, diagnostics, settings |

Crates: `nuc-core` (model/config), `nuc-monitor` (kernel providers), `nuc-power`
(D-Bus: power-profiles-daemon, UDisks2), `nuc-history` (SQLite store), `nuc-diagnostics`
(read-only checks + reports), plus the two frontends.

## Safety model

- Read-only by default. The only "write" is a platform-profile switch, which goes
  through the polkit-authorized `power-profiles-daemon` D-Bus API and is verified
  by re-reading the daemon's answer.
- Unavailable data is reported as `Unavailable` (GUI) / `null` (JSON), never as 0.
- GPU utilization needs CAP_PERFMON for `intel_gpu_top`; without it the app reports
  that it requires authorization rather than guessing a number.
- No fan control exists on this platform (ACPI fan interface only); the UI says so.
- No root daemon, no telemetry, no background services installed by default.

## Build

```sh
cargo build --release            # nuc-control + nuc-gui
cargo test --workspace           # fixture-based unit tests; no kernel writes
cargo fmt && cargo clippy        # clean
```

Runtime requirements: GTK4 ≥ 4.10 + libadwaita ≥ 1.4 (GUI), D-Bus system+session
buses, `power-profiles-daemon` for profile switching, UDisks2 for NVMe SMART.

## Install (Debian)

```sh
./scripts/build-deb.sh           # produces target/debian/nuc-control_<ver>_amd64.deb
sudo dpkg -i target/debian/nuc-control*.deb
```

The package script requires `dpkg-dev` and a native amd64 build host. Runtime
library dependencies are derived from the built binaries with `dpkg-shlibdeps`.

Installs `/usr/bin/nuc-control`, `/usr/bin/nuc-gui`,
`/usr/share/applications/org.nuccontrol.gui.desktop` (GUI shown as
"NUC Control — hardware & power monitor"), `/usr/share/doc/nuc-control/…`.

## Configuration

`~/.config/nuc-control/config.toml` (written by the GUI settings page on Save;
defaults shown, all fields optional under `#[serde(default)]`):

```toml
[general]
theme = "system"            # system | dark | light
temp_unit = "Celsius"
chart_refresh_ms = 1000
start_page = "dashboard"

[monitoring]
interval_ms = 2000          # sampler heartbeat (GUI applies live on Save)
gpu_enabled = true
record = false              # raw recording is opt-in (GUI Record switch)

[alerts]
cpu_temp_warm_c = 85.0
nvme_temp_warm_c = 70.0
fan_fault = true
throttle_warn = true
hysteresis_c = 3.0
above_samples = 3           # debounce: consecutive samples above threshold
cooldown_s = 300

[history]
raw_retention_h = 24        # raw per-sample rows
minute_agg_retention_days = 30
max_db_mib = 128            # oldest-first pruning under this ceiling
```

History DB lives in `~/.local/share/nuc-control/` (WAL-mode SQLite; per-minute
aggregates; automatic retention pruning).

## Docs

- `docs/hardware-capabilities.md` — verified capability matrix for this machine
- `docs/usage.md` — CLI/GUI walkthrough, diagnostics categories, alert behavior
- `docs/delivery-report.md` — final engineering summary + compatibility matrix
- `docs/audit-report.md` — resolved bugs, verification and improvement priorities
