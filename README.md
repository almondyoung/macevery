# MacEvery

MacEvery is an Everything-like local filename and path search app for macOS. It
builds its own local index and searches that index instead of scanning the disk
on every query.

## Features

- Native macOS GUI built with SwiftUI/AppKit.
- Rust CLI and reusable search core.
- Recursive indexing of files, directories, symlinks, and `.app` bundles.
- Local SQLite database at `~/.local/share/macevery/index.sqlite`.
- FSEvents-based `watch` command for incremental index updates.
- Local search service used by the GUI with Fastest, Balanced, Automatic, and
  Low Memory backends.
- Global `Ctrl+Space` hotkey to bring the search window forward.
- Open, Reveal in Finder, Quick Look, Copy Path, Copy Name, Copy Parent Folder,
  and Copy File actions.
- Case-insensitive strict search by default, glob wildcards, and explicit fuzzy
  search.
- Inline filters such as `ext:pdf|docx`, `kind:dir`, `path:Downloads`,
  `part:Downloads`, `!path:Library`, and `mtime:7d`.
- Sortable result columns, draggable result rows, and a Full Disk Access status
  shortcut in the GUI.
- Structured index root and exclude editors in Settings.

## Quick Start

1. Download `MacEvery-macos.zip` from GitHub Releases.
2. Unzip it and move `MacEvery.app` to `/Applications`.
3. Open the app. If macOS blocks an unsigned build, right-click the app and
   choose Open, or allow it in System Settings.
4. Grant Full Disk Access if you want protected folders such as Mail, Messages,
   Photos, and parts of `~/Library` to be indexed.
5. Review index roots in Settings. The default roots are your home folder and
   `/Applications`.
6. Click Rebuild Index.
7. Search from the main window, or press `Ctrl+Space` to bring MacEvery forward.

MacEvery searches its local index. If a file is not indexed yet, rebuild or wait
for the FSEvents watcher to refresh it.

## Build

Requirements:

- macOS 13 or later
- Rust toolchain
- Apple Command Line Tools or Xcode

Build the CLI and app:

```bash
make release
make app
```

Create a distributable unsigned app zip:

```bash
make dist
```

The packaged app is written to:

```text
.build/MacEvery.app
```

The zip artifact is written to:

```text
.build/release/MacEvery-macos.zip
```

Run the GUI:

```bash
open .build/MacEvery.app
```

## CLI

Create or rebuild an index:

```bash
./target/release/macevery index --rebuild ~ /Applications
```

Search:

```bash
./target/release/macevery search "invoice pdf"
./target/release/macevery search "report" --limit 50
./target/release/macevery search "*gpt*pdf"
./target/release/macevery search "~leetcode"
./target/release/macevery search "leetcode" --fuzzy
./target/release/macevery search "pdf" --ext pdf
./target/release/macevery search "node" --kind dir
./target/release/macevery search "Downloads dmg" --path
./target/release/macevery search "ext:pdf path:Downloads mtime:7d invoice"
./target/release/macevery search "ext:pdf|docx !path:Library invoice"
./target/release/macevery search "kind:dir code"
./target/release/macevery search "part:Downloads \"quarterly report\""
./target/release/macevery search "screenshot" --json
```

Search is case-insensitive by default. Plain terms use strict basename/path
contains matching, so `leetcode` does not fuzzy-match unrelated names such as
`sqlite_result_code.h`. `*` and `?` work as filename/path wildcards, so
`*gpt*pdf` matches names such as `ChatGPT notes.PDF`. Fuzzy search is explicit:
prefix the query with `~` or pass `--fuzzy`.

Inline filters can be typed directly into the GUI search box or CLI query:

- `ext:pdf`, `extension:pdf`, or `ext:pdf|docx` limits results by extension.
- `kind:file`, `kind:dir`, `kind:folder`, `kind:app`, `kind:symlink`,
  `kind:other`, or `kind:file|dir` limits the file kind.
- `path:Downloads` requires the path to contain `Downloads`.
- `part:Downloads` or `segment:Downloads` requires an exact path component.
- Prefix a filter with `!` or `-` to exclude it, such as `!path:Library`,
  `!ext:tmp`, or `!kind:dir`.
- `mtime:7d`, `mtime:24h`, `mtime:2w`, `mtime:3mo`, or `mtime:1y` limits
  results to recently modified items.
- Quoted values are supported, such as `path:"Application Support"`.

Open or reveal the top result:

```bash
./target/release/macevery open "invoice pdf"
./target/release/macevery reveal "invoice pdf"
```

Inspect or clean the database:

```bash
./target/release/macevery status
./target/release/macevery status --json
./target/release/macevery clean
```

Start incremental indexing for the already indexed roots:

```bash
./target/release/macevery watch
```

Start the local in-memory search service:

```bash
./target/release/macevery serve --backend memory
curl 'http://127.0.0.1:17649/search?q=invoice%20pdf&limit=20'
```

Search service backends:

- `memory`: fastest. Loads full records into memory.
- `compact`: balanced. Keeps an in-memory compressed index.
- `sqlite`: low memory. Queries SQLite directly.
- `auto`: chooses a backend from index size and memory budget.

The GUI defaults to `memory` because MacEvery is optimized for fast local use.
Switch to Balanced or Low Memory in Settings if background memory matters more.
For `auto`, set `MACEVERY_MEMORY_BUDGET_MB` to override the default budget.

Benchmark the current index:

```bash
./target/release/macevery bench
./target/release/macevery bench --json
./target/release/macevery bench --queries queries.txt
./target/release/macevery bench --rebuild ~/Documents /Applications
```

The benchmark reports index size, database size, query p50/p95/p99, and the
slowest queries. It is intended for comparing backend and query changes over
time.

## GUI

The GUI does not live-scan the filesystem for search. It starts the bundled
`macevery serve` process for low-latency search and `macevery watch` so FSEvents
can keep the index fresh. If the service is unavailable, the GUI falls back to
the CLI search path.

On first launch, MacEvery shows a compact setup panel for Full Disk Access,
index roots, and index rebuild. You can reopen those controls from the sidebar
and Settings.

Keyboard and actions:

- Type to search live.
- Press `Ctrl+Space` globally to bring MacEvery forward.
- Enter opens the selected result.
- Cmd+Enter reveals it in Finder.
- Cmd+C copies the selected path.
- Shift+Cmd+C copies the selected filename.
- Space opens Quick Look through `qlmanage`.
- Right-click a result for Open, Reveal, Quick Look, Copy Path, Copy Name, Copy
  Parent Folder, and Copy File.
- Click column headers to sort by name, path, kind, size, or modified time.
- Drag a result row into Finder or another app to pass the file URL.
- Rebuild Index starts a full reindex of configured roots.
- The sidebar shows a best-effort Full Disk Access status and opens the macOS
  privacy settings page.
- Settings provides structured index root and exclude rule editors.

## Known Limitations

- Release artifacts may be unsigned unless signing secrets are configured.
  Unsigned builds trigger macOS Gatekeeper warnings.
- Full Disk Access is required for protected folders. Without it, those folders
  may be skipped during indexing.
- The index stores local file paths and metadata in SQLite and is not encrypted.
- iCloud Drive, network volumes, and removable disks depend on macOS reporting
  paths and FSEvents consistently.
- FSEvents checkpointing is best-effort. Dropped event flags trigger root
  rescans, but offline volume semantics are not yet modeled.
- Unicode normalization is not fully normalized across NFC/NFD spellings yet.

## Troubleshooting

If search returns nothing:

```bash
./target/release/macevery status
./target/release/macevery index --rebuild ~ /Applications
```

If a newly created or deleted file is stale, use Refresh in the GUI or restart
the watcher:

```bash
./target/release/macevery watch
```

If the GUI cannot connect to the service, it should fall back to CLI search. You
can also verify the service manually:

```bash
./target/release/macevery serve --backend memory
curl 'http://127.0.0.1:17649/status'
```

If the database looks corrupted or you want a clean rebuild:

```bash
./target/release/macevery clean
./target/release/macevery index --rebuild ~ /Applications
```

If protected folders are missing, grant Full Disk Access to MacEvery and rebuild
the index.

## Homebrew

A Homebrew cask is not published yet. The intended future install path is:

```bash
brew install --cask almondyoung/tap/macevery
```

Until then, use the release zip or build from source with `make dist`.

## Release Signing

`make app` ad-hoc signs local builds by default. To create Developer ID signed
and notarized builds, provide signing and Apple notarization credentials:

```bash
export MACEVERY_CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export APPLE_ID="name@example.com"
export APPLE_TEAM_ID="TEAMID"
export APPLE_APP_SPECIFIC_PASSWORD="xxxx-xxxx-xxxx-xxxx"
make notarize
```

GitHub Releases use the same path when these repository secrets are configured:

- `MACEVERY_CODESIGN_CERTIFICATE_P12`: base64-encoded Developer ID certificate
  `.p12`.
- `MACEVERY_CODESIGN_CERTIFICATE_PASSWORD`: password for the `.p12`.
- `MACEVERY_CODESIGN_IDENTITY`: Developer ID Application identity name.
- `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_SPECIFIC_PASSWORD`: notarization
  credentials.

Without those secrets, the release workflow still publishes an unsigned app zip.

## Privacy

The index contains local file paths. It is stored locally and is not uploaded.
The current storage is not encrypted. Future hardening should add Full Disk
Access onboarding, database protection options, signed/notarized distribution,
and explicit retention controls.

## License

MIT
