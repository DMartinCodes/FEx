# FEx index service (Windows)

`fex service` keeps every fixed drive's search index ready, so the app
never crawls on launch and the first search is instant. It is the Windows
counterpart of the Linux index daemon (see LINUX.md) and shares its CPU
reserve policy (`src/reserve.rs`) and message framing (`src/wire.rs`).

**Status: compile-verified only.** It builds for `x86_64-pc-windows-msvc`
with no warnings and adds no Clippy findings, but it hasn't run on Windows
yet. The app is unaffected when the service isn't installed: it checks for
the service's pipe and otherwise searches exactly as before.

## Install

From an elevated prompt:

```bat
fex service install      & rem install as FExIndex (LocalSystem, delayed auto start) and start
fex service status       & rem drives, entries, activity, CPU reserve
fex service query report & rem search through the service from the current folder
fex service profile      & rem the machine profile and CPU reserve
fex service uninstall    & rem stop and remove
fex service debug        & rem run in the console instead (Ctrl+C ends it unsaved)
```

State lives in `%ProgramData%\FEx\Service` (created readable by SYSTEM and
Administrators only): one saved index and journal position per drive, the
profile, and `service.log`. Set `FEX_RESERVE` (percent) on the service to
override the CPU reserve.

## How it works

- **Start:** each drive's saved index loads at once. The NTFS change
  journal (USN) replays what changed while the service was stopped: the
  folders involved are re-read, nothing is crawled or verified. If the
  journal was recreated or no longer reaches back, the drive is crawled in
  the background while the saved copy answers.
- **Running:** the index's own recursive watcher (`ReadDirectoryChangesW`,
  as in the app) applies changes as they happen. Indexes are saved every
  ten minutes when changed, and on stop, with the journal position taken
  just before (replaying a few changes twice is harmless; missing one isn't).
- **Priorities:** crawler threads run in background mode, which lowers CPU,
  I/O and memory priority together (Windows honours the I/O part on every
  storage stack). The CPU reserve controller raises them through lowest,
  below-normal and normal priority while they get less than the reserve,
  so a machine that is always busy still finishes indexing. Background work
  pauses on battery. Queries and index updates stay at normal priority.
- **Profile:** cores, memory, battery, and files per NTFS volume (from the
  size of its file table, no crawl); the first crawl replaces the estimate.

## Security

- The pipe `\\.\pipe\FEx.Index` is created first-instance-only (no other
  process can own the name), refuses remote clients, and admits
  authenticated users. The app checks that the pipe belongs to SYSTEM or
  Administrators before sending a query, so another user can't stand in for
  the service.
- Identity comes from the pipe, not the request: the service impersonates
  the client just long enough to copy its token (aborting if it can't revert).
- Query workers take on that token and open each folder with matches for
  listing (`FILE_LIST_DIRECTORY`): Windows' own access check, inherited ACLs
  and all. Decisions are cached per query; hidden matches are left out of
  the count. A token that can't be taken on makes every check fail. SYSTEM
  clients see everything.
- A client gets 5 s to send its request; at most 32 are served at once.

## To test on Windows

1. Install and check `service.log`: load or crawl times per drive.
2. Restart the service after changing files: the log should show a journal
   catch-up with the number of folders re-read, and no crawl.
3. As a standard user, search for names in another user's profile: they
   must not appear, and the match count must not include them.
4. Run a CPU-bound job on every core during a first crawl: the job should
   lose about the reserve, and the crawl should still progress.
5. Search in the app: the status bar shows "Index service" on indexed drives.
