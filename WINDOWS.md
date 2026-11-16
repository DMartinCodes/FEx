# FEx index service (Windows)

`fex service` keeps every fixed drive's search index ready, so the app
never crawls on launch and the first search is instant. It is the Windows
counterpart of the Linux index daemon (see LINUX.md) and shares its CPU
reserve policy (`src/reserve.rs`) and message framing (`src/wire.rs`).

**Status: early testing.** CI builds it on Windows and runs the Windows
tests, including real-system tests of the permission checks and the change
journal; the service as a whole has had one round of hands-on testing. The
app is unaffected when the service isn't installed: it checks for the
service's pipe and otherwise searches exactly as before.

## Install

From an elevated prompt:

```bat
fex service install      & rem install as FExIndex (LocalSystem, delayed auto start) and start
fex service status       & rem drives, entries, activity, CPU reserve, memory
fex service query report & rem search through the service from the current folder
fex service profile      & rem the machine profile and CPU reserve
fex service log          & rem the end of the service's log
fex service uninstall    & rem stop and remove
fex service debug        & rem run in the console instead (Ctrl+C ends it unsaved)
```

State lives in `%ProgramData%\FEx\Service` (readable by SYSTEM and elevated
Administrators only): one saved index and journal position per drive, and
the profile. The log is `%ProgramData%\FEx\service.log`, readable by
everyone (timings and drive roots, no file names); `fex service log` shows
its end. Set `FEX_RESERVE` (percent) on the service to override the CPU
reserve, and `FEX_MEMORY_FLOOR` (percent, default 10) to change when it
starts giving memory back.

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
- **Memory:** the same deal as CPU: use memory freely, give it back before
  other programs have to. The service runs at below-normal memory priority,
  so the memory manager trims its pages before normal programs'. Twice a
  second it reads available memory (the smaller of physical memory and
  commit charge left) and the OS low-memory notification, and sets a level
  (shared with Linux, `src/memory.rs`). *Tight* (below the floor, or the OS
  signals low memory): background work pauses, the heap is compacted and
  the working set emptied; the pages stay on the standby list, so the next
  query takes soft faults unless something else needed them. *Critical*
  (below half the floor): the folder permission cache is dropped too,
  queries read what they need, and it's read again in the background once
  memory is normal. Re-applied every 30 s while the level stays up.
  `fex service status` shows the working set, private bytes, cached folder
  permissions and the level.
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
- Each folder with matches is checked with `AccessCheck` against that
  folder's security descriptor for `FILE_LIST_DIRECTORY`, with the client's
  token: Windows' own evaluation (deny entries, groups, inherited ACLs).
  This needs no file opens, because listing a folder depends only on its
  own ACL when the token has "bypass traverse checking" (granted to
  everyone by default). Descriptors are read once by the service and cached
  across queries, with duplicates merged; the cache follows the change
  journal every second (a permission change anywhere clears it; a folder
  created, deleted or renamed drops its entry).
- Tokens without the traverse bypass, and drives without a change journal,
  get the exact check instead: workers take on the token and open each
  folder for listing.
- Hidden matches are left out of the count. Anything that can't be checked
  is hidden. SYSTEM clients see everything.
- Not covered: mandatory integrity labels (in the SACL); the default policy
  only restricts writing up, not listing.
- A newer query from the same process cancels its older ones (search as you
  type). A client gets 5 s to send its request; at most 32 are served at once.

## To test on Windows

1. Install and run `fex service log`: load or crawl times per drive, then
   "read permissions of N folders (M distinct)".
2. Restart the service after changing files: the log should show a journal
   catch-up with the number of folders re-read, and no crawl.
3. As a standard user, search for names in another user's profile: they
   must not appear, and the match count must not include them.
4. Run a CPU-bound job on every core during a first crawl: the job should
   lose about the reserve, and the crawl should still progress.
5. Search in the app: the status bar shows "Index service" on indexed drives.
6. Set `FEX_MEMORY_FLOOR=99` on the service and restart: `fex service
   status` should show "paused: memory tight" and the log "memory tight
   (...): working set A -> B MB"; searches must still answer.
