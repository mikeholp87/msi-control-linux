# Manual desktop / hardware checklist

Run on the real machine before a release. CI cannot cover these.

1. **Launch twice.** Start `nuc-gui`, then run it again. Expect the existing
   window to be presented, not a second window.
2. **Power profile.** Pick a different profile and press Apply. Expect a polkit
   prompt, the selector to stay on the choice until Apply, and the active
   profile to match afterwards (`nuc-control --power`). Switch back.
3. **Recording.** Enable Record, wait about 30 s, close the window, reopen.
   History should contain the session. Check `~/.local/share/nuc-control` is
   mode 0700 and `history.db` is 0600.
4. **GPU.** Enable GPU, change the sampling interval, confirm utilization or the
   CAP_PERFMON note appears. Close the app and confirm no `intel_gpu_top`
   process remains (`pgrep intel_gpu_top`).
5. **Suspend/resume.** With the GUI open, suspend and resume. Charts must show a
   gap, not a bridged line, and readings must recover within a few intervals.
6. **Package.** `scripts/build-deb.sh`, install the `.deb` on a clean system,
   confirm the launcher appears and `nuc-control --diagnostics` runs.
