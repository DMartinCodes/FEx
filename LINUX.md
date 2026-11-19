# FEx on Linux

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
./target/release/fex [folder]            # the file explorer (X11)
sudo ./target/release/fex profile --save # measure the machine (install step)
sudo ./target/release/fex daemon         # the index daemon (see below)
./target/release/fex query <text>        # search through the daemon
./target/release/fex status              # daemon state
```

Runtime libraries (X11; winit loads them at start, and FEx reports any that
are missing):

| Ubuntu / Debian | Rocky / RHEL |
|---|---|
| `libx11-6 libxcursor1 libxrandr2 libxi6 libxkbcommon-x11-0 libgl1` | `libX11 libXcursor libXrandr libXi libxkbcommon-x11 mesa-libGL` |

Under a Wayland session FEx runs through XWayland.

## What works

The Linux UI has the Windows UI's features and layout (`src/linux/app.rs`,
`app_ui.rs` and `netview.rs` follow `src/app.rs` and `src/netview.rs`); the
platform parts are Linux-native:

| Windows | Linux |
|---|---|
| Copy/move engine (`ops.rs`) | Same design (`linux/ops.rs`): reflink (`FICLONE`), then `copy_file_range` (server-side on NFS 4.2/SMB3), then read/write; `renameat2(RENAME_NOREPLACE)`; mode and times kept; links copied as links, never followed |
| Recycle Bin | freedesktop Trash (`linux/trash.rs`): `~/.local/share/Trash`, or the volume's `.Trash-$uid`; shared with every file manager. Undo restores from it |
| Explorer clipboard (CF_HDROP) | X11 CLIPBOARD (`linux/clip.rs`) in `x-special/gnome-copied-files`, `text/uri-list` (+ KDE's cut flag) and plain text: copy/cut/paste works with Nautilus, Nemo, Caja, Thunar, Dolphin |
| OLE drag out / drop in | XDND (`linux/xdnd.rs`) to drag files out to any program; winit's drop target for files dragged in, landing on the folder under the pointer |
| ShellNew templates | `~/Templates` (XDG) plus an empty document |
| Mapped drives, Network page | GVFS through `gio` (`linux/net.rs`): connect `smb://`, `sftp://`, `ftp://`, `dav://` shares (and `\\server\share`), FEx's own credentials dialog, browse a server's shares, disconnect |
| Safely eject | udisks (`udisksctl unmount`, then power-off, or detach for disk images), polkit asks if needed; `gio mount -e` as fallback |
| Removable drives without a usable letter (`volumes.rs`) | `linux/drives.rs`: removable filesystems plugged in but not mounted (no automounter, or unmounted earlier) are listed under Drives, found from sysfs and the udev database without opening the device; click to mount and open (udisks, `gio mount -d` as fallback), or power the drive off. EFI, recovery, swap, MS-reserved and udev-ignored partitions are left out. A removable volume another mount hides is flagged instead of listed. Plug-ins show up at once (inotify on `/run/udev/data`), else within 5 s |
| Run as administrator | Run as root through `pkexec` (executables only) |
| Recent, session, tabs, windows | Same; files under `~/.config/fex` and `~/.local/share/fex` |

Everything else is the Windows behaviour: tabs (drag to reorder or tear
off into a window, spring-loaded drop targets), undo/redo of every file
operation, conflict choices (replace, update, skip, keep both), inline
rename, rubber-band selection, cut marks, ZIP browsing and safe extraction,
search below the folder or filter the folder, F1 shortcuts, session restore,
Properties (with POSIX permissions and ACLs), drives with capacity bars
(network ones on request) that follow mounts as they come and go.

Robustness: the folder watcher and clipboard reads run off the UI thread
(an unreachable network mount can't freeze the window), network commands
(`gio`, `umount`) are cut off after a time limit, and a panic while drawing
a frame is caught, logged to `~/.local/state/fex/crash.log`, reported in
the status bar, and the view reset instead of closing the window.

Desktop integration: on first start FEx installs a desktop entry and its
icon under `~/.local/share` (unless one is installed), so menus, the panel
and Alt+Tab show its name and logo; its window class is `fex`. Icon glyphs
come from the system's symbol fonts through fontconfig (DejaVu Sans, Noto
Sans Symbols 2), as Windows uses Segoe UI Symbol.

Runtime tools used when present: `xdg-open` (opening files), `gio`
(network shares; `libglib2.0-bin` and `gvfs-backends`), `udisksctl`
(mounting and ejecting), `pkexec` (run as root), `fc-match` (fonts).

Not on Linux: dragging a tab into *another* FEx window (tearing off works);
virtual files dropped from other programs (only real files arrive through
XDND); clipboard contents beyond one X request (about 256 KB of paths) need
INCR, which FEx doesn't serve yet.

## Index daemon (`fex daemon`)

A system service that keeps the whole machine's name index ready, so the
first search is instant and nothing is crawled when FEx opens.

```bash
sudo install -m755 target/release/fex /usr/local/bin/fex
sudo install -m644 packaging/linux/fex-indexd.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now fex-indexd
```

Updates: the app offers new releases in its status bar (pkexec asks for
the password), or run `sudo fex update` (`fex update check` only looks).
Releases are signed and verified before they install. The binary is
swapped atomically, and the daemon restarts with its saved index, rolling
back if it doesn't answer. Only a root-owned, root-only-writable install
(as above) installs updates. Release binaries need glibc 2.28 or later
(Rocky/RHEL 8+, Ubuntu 20.04+). See DEPLOY.md.

**Lifecycle.** On start it memory-maps its last snapshot
(`/var/lib/fex/index.snap`, mode 0600) and answers at once, then verifies
it: one `statx` per folder, comparing ctimes, re-reading only folders that
changed while it was down. It then follows changes with fanotify
filesystem marks (create, delete, rename; one mark per filesystem), with a
full verification every 30 minutes as a safety net, and for filesystems
fanotify can't watch. Network filesystems (NFS, CIFS, Lustre, ...) are not
crawled unless `--include-network` is given: crawling them loads a shared
server, and they report no changes made by other clients.

**Snapshots.** The index arrays hold no pointers, so a snapshot is the
arrays written out as they are; loading maps the file (no parsing). Every
offset is validated on load, and files are replaced by atomic rename.
Updates go to an in-memory overlay; saving compacts.

**Freshness signal.** A folder's ctime changes whenever an entry is added,
removed or renamed (and can't be set back like mtime). The index stores
names only, so a folder's ctime is a complete signal for whether its part
of the index is stale. Empty folders are recorded too.

**Priorities.** Crawling and sweeping run on threads with `SCHED_IDLE` and
the idle I/O class, and pause between folders when other processes' I/O
stalls (PSI) exceed `--io-limit` (10%), memory is under pressure, or the
machine is on battery. Queries and index updates run at normal priority, so
a starved background thread never delays an answer.

**Per-user visibility.** Each client sees only what a crawl run as that
client could have found: names in folders it can list, reached through
folders it can list.
- Identity comes from the kernel (`SO_PEERCRED`, `SO_PEERGROUPS`), never
  from the request; if groups can't be read, only the primary group counts.
- A match is shown only if its folder, and every folder from the search
  scope down to it, can be listed by the client. Names under an
  execute-only (`0711`) folder stay private even where the folder holding
  them is readable.
- Permission work happens at indexing time: each folder's bit "everyone
  may list it" (read and search for owner, group and others, and no ACL)
  is recorded when it is crawled, re-read or swept, and chained down the
  tree. Folders whose whole chain is public need no check at query time.
  The bits are kept current by fanotify attribute events (chmod, chown,
  ACL changes), are cleared if the change queue overflows, and are used
  only while every indexed filesystem is watched; otherwise, and for
  every other folder, the check is live.
- Live checks: query workers take on the client's filesystem identity
  (fsuid, fsgid, groups: per thread on Linux) and `open()` the folder. That
  is the kernel's own check, so ACLs and search permission on every
  ancestor count exactly. Decisions are cached per query, and hidden
  matches are left out of the count too.
- Fail closed: a thread that couldn't take on the identity reports nothing.
- The crawl never follows a symbolic link anywhere in a path
  (`openat2(RESOLVE_NO_SYMLINKS)`, or a verified fallback on older
  kernels), so swapping a folder for a link can't get another folder's
  names indexed under it. A subfolder replaced by a different folder of the
  same name is noticed (its ctime no longer matches) and re-read.
- One user can hold at most 8 of the 32 connection slots.
- Not covered: SELinux/AppArmor is evaluated in the daemon's domain, not
  the client's.

Tested with: owner-only folder, group folder (with and without the group),
world-readable folder with an ACL denying the user, private folder with an
ACL granting the user, a world-readable folder inside a private one, and a
readable folder inside an execute-only one (unit test).

**Protocol.** Unix socket `/run/fex/indexd.sock`; see `proto.rs`. Results
carry names and types; clients stat what they show, as the user.

**Streaming.** Queries collect matches as compact (shard, entry) pairs. The
daemon picks the first screenful in the client's sort order by comparing
names in place in the index (a linear-time selection, no copying), sends it
with the exact match count, then builds and streams the rest after
releasing the index lock. Past the 250,000-row limit, the rows kept are the
first by name.

**Metadata backfill.** The GUI shows result names at once; a worker stats
the rows on the thread pool, rows on screen first, and sizes and dates
appear as they arrive. Sorting by size or date takes effect once every row
has them.

**CPU reserve under load.** `SCHED_IDLE` gets nothing on a machine that is
always fully loaded, so the daemon reserves a small share of total CPU for
background work (it still takes everything idle). `fex profile` (run at
install, or by the daemon on first start) measures cores, memory, battery,
disk type, files in use (inode counts, no crawl) and the CPU cost per
entry, and picks the reserve from files per core: 0.5% up to 10K per core,
5% from 1M per core, log-scaled between, a quarter more with a battery
(capped at 5%). The first crawl replaces the file estimate with the real
count. A controller measures background threads' own CPU time and steps
them from `SCHED_IDLE` through nice 19, 15, 10, 5, 0 until they get the
reserve, and back once they have it. Override with `--reserve PCT` or by
editing `/var/lib/fex/profile`. The policy is shared with Windows
(`src/reserve.rs`).

**Memory.** The same deal as CPU: the daemon uses memory freely but gives
it back before other programs have to. Each second it reads available
memory (`/proc/meminfo`) and how long other processes stalled on memory
(system memory PSI minus its own cgroup's), and sets a level (shared with
Windows, `src/memory.rs`):
- *Tight*: available below `--memory-floor` (10%), or others stalled over
  10% of the time. Background work pauses; freed heap goes back to the OS
  (`malloc_trim`) and the mapped index is marked cold (`MADV_COLD`), so the
  kernel reclaims it before other programs' pages.
- *Critical*: below half the floor, or stalls over 25%. Also drops the
  folder map (rebuilt when an update needs it), folds changes held in heap
  into the snapshot if they exceed 4 MB, and pages the mapped index out now
  (`MADV_PAGEOUT`).

Levels ease only once readings are clearly better (1.5x the floor, stalls
under 5%), so the daemon doesn't flap. Queries and live (fanotify) updates
keep running at every level; a query after a shed pages the index back in.
Shedding runs on its own thread, re-applied every 30 s while the level stays
up; `kill -USR1` sheds at once. `fex status` shows resident memory (heap and
file-backed) and the level. The unit adds backstops: `MemoryHigh=25%`
(reclaim from the daemon first above that) and `OOMScoreAdjust=500` (if
the OOM killer must pick, the daemon goes before interactive programs; it
restarts from its snapshot in milliseconds).

Measured (237K entries, `kill -USR1`): resident 37.3 -> 22.1 MB (heap
20.0 -> 16.3, file-backed 17.3 -> 5.8), queries unaffected. With the floor
forced to 99%, background work paused at once and a live change was still
searchable within 6 ms; the daemon stops within about 1.5 s while paused.

**Reliability.** Maintenance runs under a supervisor: if it ever panics,
the last snapshot is reloaded and maintenance starts over (logged), so the
daemon never keeps answering from an index nobody updates. Verification
sweeps and their follow-up crawls run on a background thread and take the
index lock one batch at a time, so a sweep paused for battery or pressure
holds no lock and live changes keep being applied. Malloc arenas are
capped and freed heap is returned once a minute after queries.

Clients give the daemon 3 s for its first reply. One that doesn't answer
is skipped for a minute and the search runs in-process, with a note in the
status bar.

## Appearance and layout

☰ → *Appearance and layout…* (saved in `~/.config/fex/appearance.txt`,
shared by all windows; changes show at once):
- Theme: dark (default), light, or frosted glass with a tint colour,
  opacity and blur. Glass needs a see-through window, which is chosen at
  start, so switching to or from it offers a restart. With a compositor
  the desktop shows through; KWin (and picom builds that honour
  `_KDE_NET_WM_BLUR_BEHIND_REGION`) blur it. Without a compositor the
  tint is painted solid. Only glass asks for a transparent window, so
  every other theme costs older compositors nothing.
- Accent colour, interface font (any fontconfig family) and size.
- What the window shows: back/forward/up, the New button, path bar,
  search box, status bar (it still appears to report a problem), the
  sidebar and each of its sections, and the Date, Type, Size and Folder
  columns. Presets: Everything, Compact, Search only.

Nothing here is evaluated per frame: settings are applied when they
change, and an idle window uses no CPU in any theme.

## Design decisions (from the discovery review)

- **Names are bytes.** Linux filenames are any bytes except `/` and NUL, so
  names and paths stay `[u8]` and are decoded only for display.
- **Exact path identity.** Linux filesystems are case-sensitive; `Foo/` and
  `foo/` are different folders. Search matching stays case-insensitive.
- **Names-only index.** `getdents64` returns only name and type; size and
  date cost a `statx` per entry, so results get metadata after the query.
- **NUL-separated arena**, so storage is lossless and regexes use NUL as
  their line terminator. A bare `type:ext` becomes a SIMD search for `.ext\0`.
- **`statx` relative to the open folder**; large folders statted on the pool.
- **Pseudo filesystems are skipped** (`/proc`, `/sys`, cgroups, ...).

## Measured

4 threads, release build, virtual disk; "cold" means after dropping the
page, dentry and inode caches. Reproduce with `fex --bench`,
`fex --bench-snapshot`, `fex --bench-interference`.

Time to a searchable index:

| Tree | Cache | Crawl | Map snapshot |
|---|---|---|---|
| `/usr` (98K entries, aged) | warm | 18.8 ms | 0.8 ms |
| `/usr` | cold | 196 ms | 4.8 ms |
| synthetic CFD tree (1.56M entries) | warm | 144 ms | 9.0 ms |
| synthetic CFD tree | cold | 1,501 ms | 24.4 ms |

Snapshot of 1.56M entries: 31.6 MB, written in 87 ms, mapped with no heap.

Verifying a snapshot (ctime sweep, 4 threads): 61K folders 17.6 ms warm,
290 ms cold.

Daemon, whole machine (1.8M entries, 92K folders):
- cold restart: first query answered 142 ms after launch, fully verified
  after 826 ms (a cold crawl of `/` takes several seconds)
- changes searchable via fanotify within 8–16 ms (create, rename, new
  subtree, deleted subtree)
- first screen of results as root / as a filtered user: 1.7 / 2.8 ms
  (`conf`, 3K matches); 52 / 83 ms (`p`, 650K matches); the remaining rows
  of a 250K-row result stream in behind it (~170 ms in total)
- GUI on a cold cache: first rows 60 ms after typing, with their dates
  already filled (visible rows are backfilled first)

Background crawling beside a CPU-bound job on every core:

| Phase | Job throughput | Crawl rate |
|---|---|---|
| job alone | 100% | — |
| job + crawler at normal priority | 53.6% | 5.1 M entries/s |
| job + crawler at idle priority (daemon) | 100.2% | ~0 |
| idle-priority crawler alone | — | 11.3 M entries/s |

With the reserve (same job, 10 s phases):

| Crawler | Crawler's share of CPU | Job throughput |
|---|---|---|
| idle priority, no reserve | 0.31% | 98.9% |
| idle priority, 0.5% reserve | 0.56% | 97.9% |
| idle priority, 5% reserve | 5.45% | 94.1% |

Earlier engine numbers: recording metadata during the crawl costs
2.4–2.6x; filling it in for results costs milliseconds.

## Known limits and open decisions

- **Cold metadata for results** still costs inode reads (a snapshot holds
  names, not the kernel's inode cache); backfill keeps that off the first
  screen, but sorting a big cold result by size waits for it.
- Not tested under systemd here (the test container has no systemd); the
  unit file is untested, and PSI self-throttling is only accurate inside a
  cgroup of its own, as the service runs.
- The daemon indexes one root (default `/`); fanotify needs kernel 5.1+
  (Rocky 8's 4.18 falls back to periodic sweeps).

## Next steps

1. The gaps listed under "Not on Linux" above.
2. Windows: see WINDOWS.md for the index service.
