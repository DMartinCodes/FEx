Index and state files in the formats released versions write. Every build
must load them (tests in `src/linux/index.rs`, `src/index.rs`,
`src/winsvc/usn.rs`): an update that couldn't read what the previous
version saved would re-index every machine it reaches.

Changing a format: keep reading the old one (convert on load), bump its
version, and add a fixture of the new format here. Never delete one.

- `linux-snapshot-v1.snap`: the Linux daemon's snapshot (`snapshot.rs`),
  written by `cargo test write_format_fixture -- --ignored`.
- `windows-index-FEXIDX02.idx`: the Windows service's saved index.
- `windows-usn-position.usn`: its change-journal position.
