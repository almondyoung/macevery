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
- Local in-memory search service used by the GUI for lower-latency queries.
- Global `Ctrl+Space` hotkey to bring the search window forward.
- Open, Reveal in Finder, Quick Look, Copy Path, Copy Name, Copy Parent Folder,
  and Copy File actions.
- Case-insensitive strict search by default, glob wildcards, and explicit fuzzy
  search.
- Inline filters such as `ext:pdf`, `kind:dir`, `path:Downloads`, and
  `mtime:7d`.
- Sortable result columns, draggable result rows, and a Full Disk Access status
  shortcut in the GUI.

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
./target/release/macevery search "kind:dir code"
./target/release/macevery search "screenshot" --json
```

Search is case-insensitive by default. Plain terms use strict basename/path
contains matching, so `leetcode` does not fuzzy-match unrelated names such as
`sqlite_result_code.h`. `*` and `?` work as filename/path wildcards, so
`*gpt*pdf` matches names such as `ChatGPT notes.PDF`. Fuzzy search is explicit:
prefix the query with `~` or pass `--fuzzy`.

Inline filters can be typed directly into the GUI search box or CLI query:

- `ext:pdf` or `extension:pdf` limits results to an extension.
- `kind:file`, `kind:dir`, `kind:folder`, `kind:app`, `kind:symlink`, or
  `kind:other` limits the file kind.
- `path:Downloads` requires the path to contain `Downloads`.
- `mtime:7d`, `mtime:24h`, `mtime:2w`, `mtime:3mo`, or `mtime:1y` limits
  results to recently modified items.

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
./target/release/macevery serve
curl 'http://127.0.0.1:17649/search?q=invoice%20pdf&limit=20'
```

## GUI

The GUI does not live-scan the filesystem for search. It starts the bundled
`macevery serve` process for low-latency in-memory search and `macevery watch`
so FSEvents can keep the index fresh. If the service is unavailable, the GUI
falls back to the CLI search path.

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

## Privacy

The index contains local file paths. It is stored locally and is not uploaded.
The current storage is not encrypted. Future hardening should add Full Disk
Access onboarding, database protection options, signed/notarized distribution,
and explicit retention controls.

## License

MIT
