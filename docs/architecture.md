# MacEvery Architecture

MacEvery is split into a Rust search core and a native macOS GUI.

```text
MacEvery.app
  Contents/MacOS/MacEveryApp   SwiftUI/AppKit GUI
  Contents/MacOS/macevery      Rust CLI/search/watch/service core

~/.local/share/macevery/index.sqlite
```

## Core Principles

- Search reads the local index, never live-scans the disk.
- Full rebuilds are explicit; incremental updates are handled by the macOS
  FSEvents watcher.
- The GUI is a presentation layer and process supervisor, not the search engine.
- SQLite is used for correctness and durability first; in-memory posting lists
  can be layered on top later without changing the user-facing commands.

## Search Flow

1. The GUI starts `macevery serve` on `127.0.0.1:17649`.
2. The service loads indexed file records from SQLite into memory.
3. GUI search requests call `/search?q=...&limit=...`.
4. The service reloads its in-memory records if the SQLite/WAL marker changes.
5. Rust ranks candidates using deterministic rules:
   basename exact, basename prefix, basename substring, case-insensitive glob,
   and path substring. Fuzzy matching is opt-in with `~query` or `--fuzzy`.
6. Top results are returned as JSON.

The CLI `macevery search` path remains available and is used as a GUI fallback.

## Watch Flow

1. `macevery watch` reads indexed roots and excludes from SQLite.
2. A CoreServices `FSEventStream` watches those roots with file-level events.
3. Events are batched for a short interval and de-duplicated by path.
4. Created, modified, and renamed paths are rescanned into SQLite.
5. Removed paths delete the exact path and child path prefix.
6. Dropped, must-scan, or root-changed events trigger a roots rescan.
7. The GUI starts the watcher automatically once status reports indexed roots.

## Future Hardening

- FSEvents event checkpoints and cross-launch replay.
- In-memory trigram/posting-list index loaded on launch.
- Full Disk Access onboarding and permission diagnostics.
- External volume and network volume policy.
- Local database encryption or file protection.
- Signed and notarized release packaging.
