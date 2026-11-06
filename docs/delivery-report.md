# NUC Control delivery report

Version: 0.1.0. Updated: 2026-11-05.

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

## Verification

Local checks for this continuation:

- `cargo test --workspace --offline`: 75 tests passed, including new regression
  coverage for partial configuration, recording preference round trips and the
  shipped configuration example.
- The subsequent [full audit](audit-report.md) adds coverage and fixes for
  history retention/aggregation/close, GUI and GPU lifecycle, device identity,
  alerts, diagnostics, configuration saves and CLI contracts.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: passed.
- The provider benchmark example now uses a mutable borrow for the thermal
  monitor, matching its cached discovery API and restoring workspace test builds.
- The release build and Debian archive were checked locally. Packaging requires
  the delivery report and configuration example, and normalizes archive file
  ownership to root without running the build as root.
- ShellCheck and shfmt checks passed for the packaging script. Gitleaks found no
  secrets in the source, documentation or scripts.

## Limits and follow-up

This continuation does not install the package or change power profiles on the
host. Interactive GUI behavior, live hardware readings and distribution
compatibility beyond the recorded machine require a desktop/hardware test.
Background recording as a user service is not implemented. Alert preference
changes take effect after restart; changing the sample interval applies live.
The package uses the libraries on the build host, so an older Debian release is
not an established compatibility target.

The app does not write fan controls, RAPL limits or thermal protection settings.
Unavailable capabilities must remain explicit rather than being replaced with
fabricated values.
