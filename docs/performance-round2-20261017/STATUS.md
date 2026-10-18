# MacEvery Round 2 Status

Updated: 2026-10-18 (Asia/Shanghai). **Local implementation, installation and bounded acceptance complete.**

## Scope

User authorized continued resource optimization, replacement of the local App, commits and pushes. No live index rebuild, roots/excludes/preference/shortcut changes, production deployment or release tag. Version remains 0.1.3 with source SHA/build time.

## Source and installed App

- Round 1 baseline: ce375fe0bf8ed72f6ad25794a7a70bc68c9d19b0, frozen under .build/performance-round2-20261017/baseline-src.
- Packed index and revision/reload implementation: 09fe2a99f28f61adfd19de19e698d8519ba167f7.
- Final source: a5af62c80759e0655e1f49ff05a6c746c93e8a5c (current-request cancellation busy-state fix). Both source commits pushed to origin/main and exact SHA read back.
- Installed at .build/MacEvery.app, BuildLabel 0.1.3-ga5af62c, built clean from remote main. Strict/deep ad-hoc signature verification PASS. Signed Rust payload matches the measured App byte for byte; unsigned payload compared after stripping signatures from disposable copies.
- Final observed processes: GUI 39843, service 39857, watcher 39858; exact original App executable paths. PIDs are an acceptance-time observation, recheck before future operations.

## Verification

- Rust Debug/Release each 75/75; strict clippy all-targets PASS.
- Full Swift GUI/model/subprocess regression PASS, including old/new status decode and current-generation transport cancellation. The latter has preserved before-fix failure and after-fix PASS logs.
- Real 7,226,486-row two-block A/B/A: complete ordered result digests match. Process RSS peak 3.72 -> 2.64 GiB (-29.0%); total query CPU 6.194 -> 6.350s (+2.5%; paired +4.17%). Not an all-query speedup.
- Real HTTP metadata/content/reload suite PASS: metadata-triggered search CPU 5.640 -> .039s; multi-reload lifecycle peak 11.95 -> 2.64 GiB. Owned groups and ports cleaned.
- FSEvents 300k A/B/A 9 lifecycle/state checks PASS. HTTP40 burst: 2 correct200/38 explicit503, auth/control/recovery/cleanup PASS.
- Final native App: creation 18 bytes, modification 35 bytes, rename shows only new beta name, deletion gives 0; independent live DB path checks PASS. Fixture directories and query cleared. Settings BuildLabel, mode and original roots/shortcut verified without changing them.
- Final 20.07s idle observation: total three-process CPU .11% of one core, RSS 2.67 GiB. Original App was exited before baseline idle observation, so this is not an idle A/B or long-run leak result.

## Evidence and rollback

- Report and sanitized summaries: docs/performance-round2-20261017/REPORT.md and evidence/.
- Raw measurements, frozen binaries, CPU-unit cross-check, source fingerprints and GUI failure/success logs: .build/performance-round2-20261017/ (ignored).
- Original App rollback: .build/MacEvery.rollback-20261017-before-round2.app.
- Pre-migration live index backup: .build/performance-round2-20261017/live-index-before.sqlite, 7,292,429 rows. Real index continued receiving normal file updates; final App startup showed 7,296,280 rows.
- Source snapshot, real index copies and full search results remain local and are not pushed.

## Boundaries

Remote CI, formal QA and long-running background acceptance not verified. First search after file changes still performs a full packed reload. Status waits for an in-progress write-lock reload; health does not. SQLite candidate caps and conservative auto selection remain. HTTP cancellation does not stop already-running Rust computation.

Initial push review rejection was resolved after verifying public almondyoung/macevery, default main and SSH identity almondyoung. Some later approval checks timed out; unaffected checks completed with existing permissions or authorized isolated retries. Mac lock blocked native automation until the user manually unlocked it; locked-state observations were not labeled a product failure. No unresolved approval gate remains.
