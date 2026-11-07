# NUC Control delivery report

Version: 0.1.0. Updated: 2026-11-06.

## Delivered components

The Rust workspace contains the `nuc-control` CLI, the GTK4/libadwaita `nuc-gui`
desktop app, and shared crates for models/configuration, monitoring, power policy,
SQLite history and read-only diagnostics. The GUI has dashboard, sensors, power,
history, diagnostics and settings pages. The Debian packaging script stages both
binaries, a desktop entry, configuration example and documentation.

History recording is opt-in through `monitoring.record`. The GUI loads this
preference, saves changes from Settings and uses it to gate sample recording.
Partial configuration sections preserve the same defaults as a missing config,
including alert thresholds and history retention. The configuration example is
checked against these defaults by a unit test.

## Hardware compatibility matrix

The observations below come from the existing
[hardware capability report](hardware-capabilities.md), recorded on an MSI PRO
RPL-U Cubi NUC 1M (MS-B0B1), Intel Core 7 150U, Debian forky/sid, kernel 7.2.12.
They were not remeasured during this continuation. Other machines have not been
verified; available values depend on their exposed kernel interfaces and daemons.

| Capability | Recorded status | Interface / limitation |
|---|---|---|
| CPU topology, utilization, frequency, EPP, turbo | Available | procfs and CPU sysfs; utilization needs two samples |
| CPU, NVMe and other temperatures | Available | hwmon and thermal zones; missing readings stay unavailable |
| Fan readout | Uncertain units | `acpi_fan` may expose control state rather than verified RPM |
| Manual fan control | Unsupported | No verified PWM interface; no control is offered |
| Package/core/uncore energy and power | Available | RAPL counters; power requires a valid interval |
| RAPL power-limit editing | Not offered | Restricted kernel interface; telemetry is read-only |
| GPU frequency | Available | i915 DRM sysfs |
| GPU utilization | Authorization required | `intel_gpu_top` needs CAP_PERFMON |
| GPU temperature, power and dedicated memory | Unavailable | No documented interface exposed on this machine |
| Memory usage | Available | `/proc/meminfo`; module details need additional authorization |
| NVMe SMART health | Available | UDisks2 D-Bus; direct device access is restricted |
| Power profile switching | Available | power-profiles-daemon with polkit and post-switch verification |
| Firmware identity | Available | DMI sysfs; no firmware update operation is offered |

## Firmware-backed sensor caching (perf, 2026-11-06)

Direct measurement on the target machine showed that a full monitoring sample
cost ~410 ms because individual firmware-backed channels block: `acpi_fan`
reads run a real ACPI method inside the kernel (~203 ms each) and the INT340x
firmware thermal zones SEN1/SEN2 cost ~103 ms each, while every other sensor
reads in 1.5–10 ms. Both monitors (`nuc-monitor` thermal and fan) now measure
each channel's read time and treat channels at or above 50 ms as firmware-backed:
those are re-read only every 30 s, and intervening samples serve the last real
reading annotated `cached Ns ago (this sensor reads ~N ms per access — re-read
every 30 s)`. Fast channels stay real-time, so CPU/NVMe temperature alerts are
unaffected and alert latency is unchanged.

Provider benchmark (release build, same session, warm loop):

| Operation | Before cache | After cache |
|---|---|---|
| `thermal.sample` (warm) | ~103 ms (one INT340x zone inside) | served from cache |
| `fan.sample` (warm) | ~203 ms | served from cache |
| `engine.sample` warm (full snapshot) | ~410 ms | **~12 ms (~34×)** |

The first sample per process still performs every read once, so the cost is paid
at most once per 30 s per slow channel, transparently annotated. Regression
tests in both modules pin the behavior: a cached channel serves the stored
reading with the note while fresh, and re-reads after the window expires.

## Verification

Local checks for this continuation:

- `cargo test --workspace --offline`: 78 tests passed, including the three new
  slow-channel-cache regressions (fan cached/expiry, thermal zone cached/expiry)
  on top of the previous configuration, recording and audit coverage.
- `cargo clippy --workspace` and `cargo fmt`: clean, no remaining warnings.
- Live run on the target machine: `nuc-control --sensors --json` shows the
  annotations as designed (`firmware-backed driver (read took 202 ms) — reading
  cached between 30 s refreshes; cached 1s ago …`) on `acpi_fan`, SEN1 and SEN2,
  while TCPU, TCPU_PCI, INT3400 and x86_pkg_temp remain live reads. CLI wall
  time `--sensors` 0.94 s (startup/D-Bus dominated; the first in-process read
  still includes the unavoidable ~400 ms of ACPI calls once per 30 s window).
- GUI launches with empty stderr; an already-running installed instance makes a
  second invocation exit quietly through normal GTK single-instance forwarding
  (see Limits).
- The subsequent [full audit](audit-report.md) covered history
  retention/aggregation/close, GUI and GPU lifecycle, device identity, alerts,
  diagnostics, configuration saves and CLI contracts.
- The release build and Debian archive were checked locally. Packaging requires
  the delivery report and configuration example, and normalizes archive file
  ownership to root without running the build as root. ShellCheck, shfmt and
  gitleaks checks passed. The final package rebuild on 2026-11-06 contains the
  caching-enabled binaries (verified via embedded annotations and
  `dpkg-deb --contents`: both binaries, desktop entry, documentation and the
  configuration example).

## Limits and follow-up

This continuation does not install the package or change power profiles on the
host. Interactive GUI behavior, live hardware readings and distribution
compatibility beyond the recorded machine require a desktop/hardware test.
Background recording as a user service is not implemented. Alert preference
changes take effect after restart; changing the sample interval applies live.
The package uses the libraries on the build host, so an older Debian release is
not an established compatibility target.

At delivery time the GUI installed from the 2026-11-05 package build was still
running on the desktop session; it predates the firmware-backed sensor caching
described above and must be restarted (or upgraded via the new .deb) to pick up
the fix. A second GUI invocation exits quietly when a primary instance is
already registered — expected GTK single-instance behavior, not a failure.

The app does not write fan controls, RAPL limits or thermal protection settings.
Unavailable capabilities must remain explicit rather than being replaced with
fabricated values.
