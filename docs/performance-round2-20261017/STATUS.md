# MacEvery Round 2 Status

Date: 2026-10-17 (Asia/Shanghai).

## Authorization and scope

The user explicitly authorized continued optimization, replacement of the running MacEvery App, local commits and pushes. Preserve first-round evidence and an App rollback. Do not rebuild the live index or change roots/excludes, authentication, global shortcut, other apps or system services. Local development installation, no tagged public release; keep 0.1.3 and record source SHA/build time.

## Baseline

- Round 1: ce375fe, frozen at .build/performance-round2-20261017/baseline-src.
- Accepted 7,226,486-row snapshot: .build/performance-round1-20261017/snapshot.sqlite.
- Accepted baseline binary/library: .build/performance-round1-20261017/candidate/release/.
- Original App: .build/MacEvery.app. Prior PIDs: GUI 50086, serve 50092, watch 50107; recheck before replacement.

## Plan

1. Freeze round 1; verify origin and authorized push target.
2. Packed records and bounded string arenas, retaining full memory-backend results.
3. Files content revision separate from checkpoints; snapshot-consistent low-peak reload and failure recovery.
4. Result/update/reload regressions and sequential real-snapshot A/B.
5. Commit verified source, push after remote gate is resolved, build a separate app, preserve rollback, stop only verified App processes, replace same path and restart.
6. Native GUI search, watcher create/rename/delete in a named fixture, cleanup and bounded idle CPU/RSS.

## Ownership

- compact_index.rs: search agent.
- db.rs/sqlite.rs: watch agent. Root initially added DbRecordRef.path_lower.
- server.rs: server agent.
- GUI/build/runtime/tools/reports: root.

## Push gate

Initial auto-review rejection was resolved through read-only verification: public repository almondyoung/macevery, default main, SSH identity almondyoung. Normal fast-forward push succeeded; remote main read back ce375fe0bf8ed72f6ad25794a7a70bc68c9d19b0. No force push or release tag. No second-round installation yet.

## Runtime preflight

Original PIDs no longer exist and no MacEvery process is currently running. User preferences preserve onboarding and result columns; no performance mode override is stored, so memory remains the default. Live index backup completed before migration: 7,292,429 rows in .build/performance-round2-20261017/live-index-before.sqlite.

## Pre-install verification

- Debug/Release 75/75, strict clippy all-targets, Swift GUI regressions PASS.
- Real 7,226,486-row two-block A/B/A: complete result digests match; load RSS -50.8%, search process peak -29.0%; query group CPU +2.5% (not an all-query speedup).
- Real memory-service metadata/content/explicit reload suite PASS: metadata search CPU 5.640s -> .039s, lifecycle peak 11.95 -> 2.64 GiB. Process groups and ports cleaned.
- Real FSEvents 300k A/B/A lifecycle/state checks PASS; HTTP40 burst 2 correct200/38 explicit503 and recovery PASS.
- Reports and sanitized summaries saved; original App backup verified, live index backup retained.
- Next: commit/push exact source, clean build to separate App, verify identity, replace same path, native window and live watcher smoke, idle process sampling, final acceptance evidence commit/push.
