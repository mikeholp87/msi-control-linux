# NUC Control — Hardware Capability Report

**Machine inspected:** 2026-11-05, by direct inspection of the running system (user `mikeh`, unprivileged).
Verification method: direct reads of documented kernel interfaces plus cross-checks
against installed system utilities (`lscpu`, `nvme`, `powerprofilesctl`, `busctl`,
`free`, `lsblk`, `intel_gpu_top`). Kernel paths are read verbatim from this machine;
nothing below is inferred from product marketing data.

## System identity

| Item | Value | Source |
|---|---|---|
| Vendor | Micro-Star International Co., Ltd. | `/sys/class/dmi/id/sys_vendor` |
| Product | PRO RPL-U Cubi NUC 1M (MS-B0B1) | `/sys/class/dmi/id/product_name` |
| Board | MS-B0B11 (MSI) | `/sys/class/dmi/id/board_name` |
| BIOS | AMI 8.40, 2025-07-22 | `/sys/class/dmi/id/bios_*` |
| OS | Debian forky/sid, kernel 7.2.12-amd64 | `/etc/os-release`, `/proc/sys/kernel/osrelease` |
| Desktop | KDE Plasma / GNOME supported targets; Wayland & X11 | session/environment inspection at runtime |

## CPU

`lscpu`: Intel Core 7 150U (family 6, model 186, stepping 3), 10 cores / 12 threads
(hybrid: 2 performance cores with HT + 8 efficient cores), 12 MiB L3. HWP + HFI
supported (`hwp`, `hfi` flags).

| Interface | Path | State | Notes |
|---|---|---|---|
| CPUFreq driver | `/sys/devices/system/cpu/cpu*/cpufreq/scaling_driver` | **readable**: `intel_pstate` (active mode) | per-CPU policies `policy0`…`policy11` |
| Governor | `scaling_governor` / `scaling_available_governors` | **readable** | `powersave` active; `performance` available |
| Frequency range | `scaling_min_freq` / `scaling_max_freq` / `scaling_cur_freq` | **readable** | 400–5400 MHz (boost), current ~3.9 GHz |
| EPP | `energy_performance_preference`, `energy_performance_available_preferences` | **readable** | `default performance balance_performance balance_power power` |
| intel_pstate | `/sys/devices/system/cpu/intel_pstate/{status,no_turbo}` | **readable** (`active`, `no_turbo=0`) | `/sys/devices/system/cpu/cpufreq/boost` available as well |
| Turbo state | above interfaces | readable | changes would need root; changeable through power-profiles-daemon instead |
| Core topology | `topology/{core_id,core_cpus_list,thread_siblings_list}` | **readable** | usable to identify P-cores (sibling threads present) vs E-cores (single thread, higher `core_id`s 4..15) |
| CPU capacity | `cpu_capacity` | readable | cross-check for hybrid classification |
| Thermal throttle counters | `/sys/devices/system/cpu/cpu0/thermal_throttle/{core,package}_{throttle_count,total_time_ms,max_time_ms}` | **readable** | counts accumulate since boot; duplicates across CPUs handled by reading cpu0 |
| Utilization | `/proc/stat` | readable | standard jiffy deltas |

## Temperatures

| Sensor (hwmon) | Chip / device | Attributes | Status |
|---|---|---|---|
| hwmon0 | `acpitz` (thermal_zone0) | temp1_input | readable |
| hwmon1 | `acpi_fan` (PCI INTC10A2) | `fan1_input` | readable — see Fans |
| hwmon2 | `nvme` (WD_BLACK SN850X) | temp1_input/max/min/crit_alarm/label(Composite) | readable |
| hwmon3 | `coretemp` (platform/coretemp.0) | temp1..temp17 input/crit/max/label | readable |
| hwmon4 | `iwlwifi_1` (Wi-Fi) | temp1_input | readable |
| x86_pkg_temp (thermal_zone7), INT3400, SEN1/2, TCPU, TCPU_PCI | `/sys/class/thermal/thermal_zone*` | type/temp | readable |

Also available:

| Interface | Path | State |
|---|---|---|
| Thermal zones & cooling devices | `/sys/class/thermal/thermal_zone*/type`,`temp`; `cooling_device*` | readable (19 cooling devices) |
| hwmon ABI limits | `tempX_crit`, `tempX_max` (`temp1_crit=100 °C` coretemp; NVMe crit 93.85 °C / max 89.85 °C) | readable |

### Sensor read cost and caching (measured 2026-11-06)

Individual sysfs read latency varies by orders of magnitude because some
firmware-backed channels execute ACPI methods inside the kernel:

| Channel | Read latency | Class |
|---|---|---|
| coretemp, NVMe, iwlwifi, acpitz hwmon temps | 1.5–10 ms | fast (real-time) |
| `thermal_zone2` / `thermal_zone5` (INT340x firmware zones: SEN1/SEN2) | ~101–104 ms | slow |
| `acpi_fan` `fan1_input` (ACPI fan query) | ~202–204 ms | slow |

NUC Control measures each channel's read time once; channels reading at or above
50 ms are re-read only every 30 s (config default) and intervening samples serve
the last real reading annotated `cached Ns ago …`. A full warm temperature+fan
sample therefore costs ~12 ms instead of ~410 ms. Alerts that matter for
monitoring (CPU package/cores, NVMe, fan alarm) are fast channels and stay
real-time. Cached values are always labeled; nothing is presented as live that
is not.

## Fans

hwmon1 is `acpi_fan` exposing `fan1_input` (e.g. 61). **Caveat:** values reported by
`acpi_fan` may encode the ACPI fan control state rather than a true tachometer RPM.
NUC Control therefore labels this source explicitly and does not claim it is verified RPM.
No other fan hwmon is present. No PWM (`pwm1`, `pwm1_enable`) controls exist at any chip —
**manual fan control is unsupported** and must not be offered. (System identity confirms
MSI "Cubi NUC" firmware; no MSI EC fan driver is present in the kernel.)

## Power (RAPL)

`/sys/class/powercap` exposes:

| Domain | Energy counter | Constraints | Writable |
|---|---|---|---|
| `intel-rapl:0` package-0 | `energy_uj` readable (e.g. 37 041 906 637 µJ) with `max_energy_range_uj` | long_term 20 W, short_term 30 W, peak_power 70 W (`constraint_0..2_power_limit_uw`) | **no** for unprivileged user |
| `intel-rapl:0:0` core | energy readable | constraint_0 | no (user) |
| `intel-rapl:0:1` uncore | energy readable | constraint_0 | no (user) |
| `intel-rapl-mmio` | duplicate package-0 view | — | not used (overlapping measurement of package-0) |

Power limits are platform policy; the kernel refuses writes without root. NUC Control’s
v1 treats power limits as **read-only telemetry**; no power-limit editor is shipped.

## GPU

| Item | State |
|---|---|
| Device | Intel Raptor Lake-U integrated graphics, PCI 00:02.0 |
| Driver | **i915** (bound). `xe` is loaded but unbound on this VID/DID. |
| DRM nodes | `card0`, `renderD128` |
| GPU frequency | `/sys/class/drm/card0/gt_cur_freq_mhz` (els `gt_{act,min,max,boost}_freq_mhz`, `gt_RP0/1n_freq_mhz`) — **readable** |
| GPU utilization | requires `intel_gpu_top` which needs `CAP_PERFMON` → **permission-restricted** for the user; the app detects the failure instead of faking data |
| GPU temperature/power/memory | not exposed through any documented interface here — unavailable |

## Memory & storage

- `/proc/meminfo`: 32 GiB RAM + ~15.6 GiB `zram0` swap (current sample: ~18 GiB used, ~12 GiB available).
- DMI memory tables require root → module details marked authorization-required.
- Storage: NVMe `WD_BLACK SN850X 2000GB` (`/dev/nvme0n1`, FW 620381WD, 7 partitions) and USB `Seagate One Touch w/PW`.
- `/dev/nvme0` is **not readable by the user**; direct `nvme`/`smartctl` use is blocked.
- **UDisks2 D-Bus works unprivileged** for the active session:
  `org.freedesktop.UDisks2.NVMe.Controller.SmartGetAttributes` returned
  `percent_used=0`, `avail_spare=100`, `spare_thresh=10`, `total_data_written≈6.1 TB`,
  `unsafe_shutdowns=4`, `media_errors=0`, `wctemp=90 °C`, `cctemp=94 °C`.
  The app uses this interface and reports only attributes it actually received.

## Power profile management

- `power-profiles-daemon` **active** (D-Bus `org.freedesktop.UPower.PowerProfiles`).
  `powerprofilesctl` reports profiles: `performance` (active), `balanced`, `power-saver`;
  CpuDriver `intel_pstate`, Degraded `no`.
- `tuned`, `tuned-ppd`, `TLP`, `thermald` are **not installed/running** — no conflict.
- `fwupd` service is active (read-only firmware metadata available).

## Capability classification summary

| Capability | Classification |
|---|---|
| CPU topology / utilization / frequency / driver info | **Supported & available** (readable sysfs/procfs) |
| CPU temperatures | **Available** (coretemp) |
| Package/chipset/NVMe/Wi-Fi temperatures | **Available** (hwmon ×5, thermal zones) |
| Hybrid P-core/E-core split | **Available** (topology; classification basis is reported in-app) |
| Thermal throttling counters | **Available** (core/package count + duration since boot) |
| RAPL package/core/uncore energy → power | **Available** (counters readable) |
| Power profile selection | **Available** (power-profiles-daemon D-Bus; polkit-authorized) |
| RAPL power-limit editing | **Supported but restricted** (kernel interface exists, root-writable) — not offered in v1 |
| Turbo status | **Available** (read-only; changes via PPD) |
| GPU frequency | **Available** (i915 sysfs) |
| GPU utilization | **Requires authorization** (CAP_PERFMON for intel_gpu_top) |
| GPU temperature/power/memory | **Unsupported** by exposed driver interfaces |
| Fan speed readout | **Uncertain** (acpi_fan state-derived; labeled as such) |
| Manual fan control | **Unsupported** (no PWM interface; safe automatic control can’t be verified/bypassed) |
| NVMe SMART/NVMe attributes | **Available** via UDisks2 D-Bus |
| Direct nvme-cli/smartctl device access | **Requires authorization** (`/dev/nvme0` not user-readable) |
| Firmware (BIOS) info | **Available** (DMI) |
| fwupd update listing | **Available** (service running) |
| Memory module SPD/SMBIOS details | **Requires authorization** (DMI tables root-only) |
| EPP / governor manual control | **Requires authorization** (sysfs root) — PPD profile changes are the supported path |
