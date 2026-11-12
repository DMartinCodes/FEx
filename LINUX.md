# FEx on Linux (prototype)

The Linux port lives in `src/linux/` and is compiled only on Linux. The
Windows build is unchanged: its modules are gated with `#[cfg(windows)]` in
`main.rs`, `windows-sys` is a Windows-only dependency, and the Linux-only
dependencies (`libc`, eframe's X11 backend, egui's bundled symbol fonts) are
declared under `[target.'cfg(target_os = "linux")'.dependencies]`.

Shared with Windows: `pool.rs` (thread pool) and `typeahead.rs` (type-to-find
ranking), both platform-neutral.

## Build and run

```bash
cargo build --release
./target/release/fex [folder]
./target/release/fex --bench /usr        # engine benchmark, no window
```

Runtime libraries (X11; winit loads them at start, and FEx reports any that
are missing):

| Ubuntu / Debian | Rocky / RHEL |
|---|---|
| `libx11-6 libxcursor1 libxrandr2 libxi6 libxkbcommon-x11-0 libgl1` | `libX11 libXcursor libXrandr libXi libxkbcommon-x11 mesa-libGL` |

Under a Wayland session FEx runs through XWayland.

## What works

- Browsing with sizes and dates, sort by name / date / type / size, folders first
- Virtualized table (only visible rows are drawn), all I/O off the UI thread
- Live updates of the open folder (inotify, coalesced per name)
- Indexed search: keywords, `*`/`?` globs, `type:ext`, regex mode; exact
  match count, results capped at 250,000 rows
- Type-to-find (same ranking as on Windows), keyboard navigation and
  selection, path bar with breadcrumbs (Ctrl+L), hidden files (Ctrl+H)
- Sidebar: XDG user folders and mounted volumes (from `/proc/self/mountinfo`)
- Open with the default application (`xdg-open`), copy path

Not yet: file operations (copy/move/delete/rename, trash, undo), clipboard
file transfer, drag and drop, tabs, properties, network browsing, saved
indexes, live index freshness.

## Design decisions (from the discovery review)

- **Names are bytes.** Linux filenames are any bytes except `/` and NUL, so
  names and paths stay `[u8]` and are decoded only for display. Files with
  invalid UTF-8 or newlines in their names can still be opened.
- **Exact path identity.** Linux filesystems are case-sensitive; `Foo/` and
  `foo/` are different folders. Search matching stays case-insensitive.
- **Names-only index.** `getdents64` returns only name and type; size and
  date cost a `statx` per entry. The index records names and types, and
  results get metadata after the query, which touches only what is shown.
- **NUL-separated arena.** The one byte no filename contains separates names,
  so storage is lossless and regexes use NUL as their line terminator. A bare
  `type:ext` becomes a SIMD search for `.ext\0`.
- **`statx` relative to the open folder** avoids resolving the path again
  for every entry. Large folders are statted on the thread pool.
- **Pseudo filesystems are skipped** (`/proc`, `/sys`, cgroups, ...) so
  crawling `/` indexes only user files.

## Measured (4 threads, warm cache, release build)

| | `/usr` (97.6K entries) | `/` (235K entries) |
|---|---|---|
| crawl, names only | 13.5 ms (7.2 M entries/s) | 45 ms (5.2 M entries/s) |
| crawl, names + `statx` | 34.9 ms (2.8 M entries/s) | 107 ms (2.2 M entries/s) |
| query `conf` | 0.3 ms | 0.9 ms |
| query `type:h` | — | 3.0 ms (was 23.7 ms before the `.ext\0` literal) |
| metadata for results | 0.6–1.8 ms | 0.9–4.2 ms |

Recording metadata during the crawl costs 2.4–2.6x; filling it in for
results costs milliseconds. Reproduce with `fex --bench <folder>`.

## Next steps

1. **Index freshness:** inotify per folder up to a watch budget, then a sweep
   that re-reads folders whose mtime changed (statting folders only).
2. **File operations:** `renameat2` (`RENAME_NOREPLACE`), `copy_file_range`
   (reflinks on Btrfs/XFS, server-side copy on NFS 4.2/SMB3) in the existing
   parallel, largest-first transfer design; freedesktop Trash for delete/undo.
3. **Clipboard and drag and drop:** `text/uri-list` and
   `x-special/gnome-copied-files` for copy/paste with Nautilus and others;
   XDND for dragging files out (winit has no drag-out on any platform).
4. Convergence: move the shared index core to byte names so Windows and
   Linux use one implementation (no cost on Windows, whose names are already
   stored as UTF-8).
