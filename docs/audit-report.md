# Workspace audit — 2026-11-05

What this repo does: NUC Control reads Linux hardware telemetry, stores opt-in
history and offers authorized power-profile changes through D-Bus. It ships a
GTK desktop app and a CLI. This audit assumes one desktop user, repeated app
activation/closure and sustained monitoring sessions.

Scope: all seven crates, their source and tests, examples, manifests and lockfile,
packaging scripts and documentation. Source was compared with a local snapshot
because Git metadata is unavailable in this workspace. An independent reviewer
read the frontend and diagnostics flows, then reviewed the fixes and their
shared callers. No new dependencies were added; four unused direct dependency
declarations were removed.

## Must fix — resolved

1. **Closing the app lost buffered history** (`nuc-history/src/lib.rs`, `nuc-gui/src/pages.rs`).
   - What this is: history batches samples before writing to SQLite.
   - Problem: closing before the periodic flush discarded the current batch.
   - Fix: flush on close and store drop; report failures. A reopen test now omits an explicit flush.
   - If skipped: short recording sessions disappear.

2. **Minute aggregation skipped boundary samples** (`nuc-history/src/lib.rs`).
   - What this is: a cursor marks the start of the next unfinished minute.
   - Problem: an exclusive lower bound discarded a sample exactly at that cursor.
   - Fix: use an inclusive lower bound and test a later minute with a sample on its exact boundary.
   - If skipped: minute coverage and maxima can be wrong.

3. **Retention could erase data without shrinking the database** (`nuc-history/src/lib.rs`).
   - What this is: retention applies age and size budgets.
   - Problem: allocated free pages still counted against the budget; the old deletion query also did nothing below 5,000 rows.
   - Fix: count live pages, prune bounded oldest batches, include aggregates/events when necessary, and compact. Tests cover a small oversized store and event-only growth.
   - If skipped: the size ceiling fails or recent data is needlessly deleted.

4. **History inspection modified storage and accepted future schemas** (`nuc-history/src/lib.rs`, `nuc-diagnostics/src/lib.rs`).
   - What this is: diagnostics should inspect existing history safely.
   - Problem: the old helper created or initialized databases; normal opening overwrote the schema version even when it was newer.
   - Fix: provide read-only inspection, reject newer schema versions before initialization and test both cases.
   - If skipped: diagnostics creates files and an older app can relabel a newer database.

5. **GUI power-profile switching was unusable** (`nuc-gui/src/main.rs`, `pages.rs`).
   - What this is: available profiles and current state come from power-profiles-daemon.
   - Problem: GUI snapshots never contained profile state; once populated, periodic refresh would overwrite pending selection.
   - Fix: poll profiles independently, copy results into snapshots, guard programmatic selection updates and disable overlapping switches.
   - If skipped: the selector is empty or resets before Apply.

6. **Window lifecycle leaked work and could orphan the GPU tool** (`nuc-gui/src/main.rs`, `pages.rs`, `nuc-monitor/src/engine.rs`).
   - What this is: windows own timers and a sampler which can own a child process.
   - Problem: activation created additional windows/samplers; timers outlived closed windows, and exit could precede child cleanup.
   - Fix: present the existing window, remove timers on close, stop the sampler and wait until its engine has dropped. Tests cover channel-disconnect shutdown and child reaping.
   - If skipped: duplicate notifications and orphaned collectors can remain.

7. **GPU collection could block or never parse readings** (`nuc-monitor/src/gpu.rs`).
   - What this is: intel_gpu_top provides a streaming JSON array of samples.
   - Problem: reading stderr before stdout blocked successful collection, while parsing each line as a complete JSON document rejected multiline samples. The engine never started the collector.
   - Fix: start collection when enabled, drain both pipes independently, parse complete objects, select the matching DRM device and reject stale/out-of-range readings. Restart collection when cadence changes.
   - If skipped: utilization stays unavailable despite authorization, or describes the wrong/stale device interval.

8. **Sampling settings and read failures produced incorrect deltas** (`nuc-monitor/src/engine.rs`, `cpu.rs`, `power.rs`).
   - What this is: utilization and watts require two valid measurements.
   - Problem: interval changes left gap detection using the old interval; Linux monotonic time excludes suspend, and failed reads could preserve or invent baselines.
   - Fix: propagate intervals to engine and GPU, apply GPU enable changes live, detect gaps with wall time, and reset missing CPU/RAPL/throttle baselines. Tests cover gaps, interval changes and read recovery.
   - If skipped: readings disappear at long cadences or falsely bridge missing data.

9. **Alert debounce was not consecutive** (`nuc-monitor/src/alerts.rs`).
   - What this is: temperature alerts require consecutive threshold breaches.
   - Problem: readings just below the threshold did not reset the count, and only the first matching sensor was checked.
   - Fix: reset the debounce count independently of active-alert hysteresis and inspect the hottest matching CPU/NVMe reading. Regression tests cover both cases.
   - If skipped: alerts can fire early or miss a hotter device.

10. **Charts and sensor rows misrepresented telemetry** (`nuc-gui/src/main.rs`, `pages.rs`, `chart.rs`, `nuc-monitor/src/thermal.rs`, `fan.rs`).
    - What this is: the GUI consumes snapshots and displays identified sensor rows and bounded series.
    - Problem: ticks duplicated old samples, charts bridged missing intervals and compressed retained data; duplicate chip names collided and departed sensors kept stale values.
    - Fix: consume each timestamp once, retain explicit gaps, normalize against retained time, use device-based identities, reconcile removed rows and periodically rediscover sensors. Display warning/critical limits in the selected unit.
    - If skipped: old or invented continuity can look like current measurements.

11. **ACPI fan state was labelled and stored as RPM** (`nuc-core/src/model.rs`, both frontends, diagnostics and history).
    - What this is: acpi_fan exposes a value whose tachometer units are unverified on this board.
    - Problem: source notes warned about uncertainty, but speed labels and the history RPM column still asserted RPM.
    - Fix: share truthful speed formatting and leave history RPM unavailable for this source; tests cover the label and stored row. Existing history is preserved unchanged.
    - If skipped: a control-state number is mistaken for measured fan speed.

12. **Storage discovery confused disks and temperatures** (`nuc-monitor/src/storage.rs`).
    - What this is: disks, partitions and hwmon sources are discovered through sysfs.
    - Problem: prefix matching hid disks such as nvme0n10 beside nvme0n1; every NVMe disk received the first NVMe temperature.
    - Fix: use the kernel partition attribute and match hwmon to each disk's canonical controller device. Mount lookup now honors fixture paths. A two-device test verifies identity and temperature pairing.
    - If skipped: an entire disk can be hidden or receive another drive's temperature.

13. **Sparse CPU numbering corrupted capacity classification** (`nuc-monitor/src/topologyparse.rs`).
    - What this is: capacity belongs to a logical CPU ID.
    - Problem: a dense vector was indexed by sparse CPU numbers, and missing capacity was treated as a low tier.
    - Fix: key capacities by CPU ID and retain Unknown for absent evidence; test sparse IDs and missing data.
    - If skipped: a performance core may be labelled efficient or lack its actual capacity.

14. **Configuration and fallback storage had unsafe failure behavior** (`nuc-core/src/config.rs`, `nuc-gui/src/main.rs`).
    - What this is: configuration saves replace a TOML file; history may fail to open.
    - Problem: invalid intervals/nonfinite thresholds were accepted, unreadable config was silently ignored, saves followed a predictable temporary filename and history fell back to a shared /tmp path.
    - Fix: validate loads/saves, warn on read errors, create exclusive unique temporary files with private permissions and synced contents, and use visibly session-only in-memory history. Tests cover invalid input, preserved saves and the old temporary-symlink case.
    - If skipped: invalid settings can break monitoring, and fallback files can be redirected or exposed locally.

15. **Diagnostics and CLI output claimed more than they checked** (`nuc-diagnostics/src/lib.rs`, `nuc-cli/src/main.rs`).
    - What this is: diagnostic verdicts and CLI output guide user decisions.
    - Problem: live diagnostics never fetched NVMe SMART or checked history; unavailable evidence could receive Ok/all-passed. CPU frequencies had the wrong unit, export ignored JSON, and exclusive modes silently ignored other flags.
    - Fix: fetch SMART with explicit backend failures, inspect history read-only, classify unavailable checks honestly, correct frequency units, return JSON export metadata and reject conflicting modes before D-Bus writes. Added coverage for SMART faults, missing history, unavailable verdicts and argument combinations. The client reads controller critical-warning labels and rejects never-updated SMART properties rather than treating default zeros as health evidence.
    - If skipped: failing storage can escape checks and automation receives misleading output.

16. **The package omitted its actual ABI requirements** (`scripts/build-deb.sh`).
    - What this is: native binaries depend on minimum shared-library versions.
    - Problem: symbol inspection found a glibc 2.39 requirement absent from the hand-written metadata, allowing installation on an incompatible system.
    - Fix: generate runtime library dependencies with dpkg-shlibdeps, retain explicit GTK/libadwaita policy minima and reject unsupported native architectures.
    - If skipped: installation can succeed while the binaries cannot start.

## Should fix — improvement priorities

17. **Plan the zbus 6 migration** (`Cargo.toml`, `nuc-power/src/lib.rs`).
    - What this is: D-Bus serialization/name support is supplied transitively by zbus 5.
    - Problem: the cached RustSec database flags zvariant, zvariant_derive and zbus_names as unmaintained. These are maintenance advisories, not detected exploitable vulnerabilities.
    - Fix: migrate to zbus 6 using the upstream upgrade guidance and test actual PPD/UDisks2/polkit behavior before changing the dependency stack.
    - If skipped: the application remains on components that receive no further standalone releases. [Upstream migration context](https://github.com/z-galaxy/zbus/issues/2043).

18. **History housekeeping blocked the GTK thread — resolved** (`nuc-gui/src/history_worker.rs`).
    - What this is: flush, aggregation, retention and compaction currently run during the UI tick.
    - Problem: disk contention or compaction can block rendering until SQLite returns, even though errors now pause recording visibly.
    - Fix: a dedicated worker owns the store, receives samples through a bounded queue and returns status to the GUI; shutdown waits for queued samples to flush.
    - If skipped: slow storage can make the desktop app temporarily unresponsive.

19. **Finish desktop and hardware integration coverage** (GUI, power client and package).
    - What this is: unit tests cover parsers, storage and lifecycle logic, but not a real desktop session.
    - Problem: profile authorization dialogs, authorized GPU collection, suspend/resume and real GTK interactions still need runtime evidence.
    - Fix: add a desktop CI smoke job plus a repeatable hardware checklist for profile selection, recording close/reopen, reactivation and GPU enable/cadence changes.
    - If skipped: desktop/driver regressions can pass the unit suite.

## Nice to have

20. **Reduce inactive preferences and release placeholders** (config, manifests and desktop entry).
    - What this is: compact_mode, background_service and record_interval_ms expose unfinished/reserved behavior; package metadata remains local/example metadata.
    - Problem: unused knobs add maintenance burden, and example identity is unsuitable for public distribution.
    - Fix: remove or explicitly reserve inactive preferences; set real release metadata and align the installed desktop filename with the application ID when preparing a public release.
    - If skipped: users may expect inactive options to work, and desktop integration remains less polished.

## Verification and limits

- Baseline: 47 tests passed. Expanded suite: 75 tests passed.
- Strict workspace/all-target Clippy and Rust formatting checks passed.
- ShellCheck, shfmt, desktop-entry validation and Gitleaks source/config/docs scans passed.
- The rebuilt Debian archive passed contents, permissions, root ownership and
  generated ABI dependency checks. Extracted CLI smoke checks passed for combined
  JSON sections, human CPU units, diagnostics and JSON report export; read-only
  diagnostics did not create a history directory. The unsupported-architecture
  guard was checked in an isolated fixture.
- Tests include SQLite size/retention, missing/future history, close/reopen persistence,
  failed telemetry reads, sparse topology, distinct devices, alert debounce,
  chart gaps/window bounds, GPU JSON/cadence/child cleanup and CLI validation.
- Cargo audit used the local RustSec database dated 2026-11-03, with fetching and
  yanked-version lookup disabled. It found no known vulnerabilities and the three
  unmaintained warnings above. This is not a fresh registry-wide certification.
- NVMe controller properties were checked against the [UDisks2 API](https://storaged.org/doc/udisks2-api/latest/gdbus-org.freedesktop.UDisks2.NVMe.Controller.html).
- GPU format/device selection was checked against the
  [upstream tool manual](https://manpages.debian.org/unstable/intel-gpu-tools/intel_gpu_top.1.en.html).
- No fan-control, RAPL-limit, thermal-protection or real profile writes were performed.

Verdict: the confirmed correctness and data-loss findings above are fixed and
covered by local checks. The highest remaining priorities are dependency
maintenance and desktop/hardware tests.

Lean: four unused direct dependency declarations removed; their transitive
packages can still be required by other crates. No speculative framework or new
dependency was introduced.

Not checked: interactive GTK/polkit, authorized GPU hardware collection,
suspend/resume on physical hardware, package installation, older distributions,
fresh registry/yanked advisories or unreadable Git history. Jev scoring was not
used after its earlier call reported a missing API key; local tests and
independent source review provide the verification here.

## Follow-up — 2026-11-07

Resolved after the second audit pass:

- **zbus 6 migration (item 17).** `zbus` 5 to 6.2.1; `zvariant*` and
  `zbus_names` unmaintained advisories are gone and `cargo audit` is clean.
  zbus 6 is async-only, so `nuc-power` wraps each call in `zbus::block_on`
  behind its unchanged synchronous API. Live read-only checks passed against
  power-profiles-daemon, UDisks2 NVMe SMART and systemd unit queries. Profile
  *writes* were not exercised. MSRV is now 1.89 (a zbus requirement).
- **Item 19.** CI (`.github/workflows/ci.yml`) runs fmt, clippy `-D warnings`,
  tests, shellcheck and `cargo audit`. Desktop and hardware steps that CI
  cannot cover are in `hardware-checklist.md`; they have not been run.
- **Item 20.** Inactive config keys removed (old files containing them still
  load); desktop entry renamed to the application ID; maintainer and homepage
  point at the GitHub repository; `LICENSE` added; internal reports no longer
  ship in the package.
- **Robustness.** Poisoned mutexes no longer cascade into the GUI
  (`MutexExt::lock_or_recover`); `intel_gpu_top` is launched by absolute path,
  its stderr parse is capped and a CAP_PERFMON diagnosis is not overwritten;
  history directory is 0700 and database 0600; lossy `as` casts replaced with
  checked conversions and enforced by workspace lints; release binaries are
  stripped (5.1/5.5 MB to 4.2/4.5 MB); package directories are 0755.
