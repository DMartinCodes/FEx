# Releases and updates

FEx ships as GitHub releases of this repository. Installed copies check for
a newer release, and with an administrator's approval they install it and
restart the index service. The service keeps its index across the update.

## Security model

The index service runs as SYSTEM (Windows) or root (Linux), so a malicious
update would own every machine that installs it. Updates are therefore
trusted only if they carry a valid signature. Coming from GitHub or HTTPS
is not enough.

- **Signatures.** Every release file is signed with
  [minisign](https://jedisct1.github.io/minisign/) (Ed25519 over a BLAKE2b
  hash). Builds contain the public keys in `packaging/release-keys.pub` and
  accept no others. The private key lives only in your offline backup and
  in the repository's protected `release` environment. A compromised
  GitHub account, release page or download path can't produce a file that
  installs.
- **What a signature covers.** Its trusted comment, `fex 0.2.0
  fex-linux-x64`, is covered by the signature too. The updater checks the
  file name, so one platform's binary can't stand in for another. It also
  requires a version newer than the installed one, so an older, validly
  signed release can't be replayed as a downgrade.
- **Who verifies.** The app downloads as the user and verifies (to fail
  early). Installing runs the *installed* copy as administrator: Windows
  asks through UAC, Linux through pkexec or sudo. That copy sits in a
  folder only administrators can write (`%ProgramFiles%\FEx`, a root-owned
  `/usr/local/bin`). It reads the download into memory, verifies it again
  with its own built-in keys, and writes exactly those bytes. Nothing the
  user could change between the check and the install is trusted. A Linux
  copy that isn't root-owned and root-only-writable refuses to install
  updates.
- **The release pipeline.** Signing runs only for `v*` tags, in a job that
  uses the `release` environment (you approve every run). Before
  publishing, the job checks that the binary it just built accepts the
  signatures with its built-in keys. A release can't go out unsigned or
  signed by a key the shipped updater doesn't trust. Third-party actions
  are pinned to commits, and checkouts don't keep the job token.
- **Transport.** Downloads use HTTPS only, including redirects: WinHTTP on
  Windows (system proxy and certificate store), curl on Linux. TLS
  protects privacy here. Integrity comes from the signatures.
- **Policy.** Update checks can be turned off per machine:
  `HKLM\SOFTWARE\Policies\FEx` `DisableUpdateCheck` = 1 (DWORD) on
  Windows, the file `/etc/fex/no-update-check` on Linux, or
  `FEX_NO_UPDATE_CHECK=1` for one user. Checks fetch one small signature
  file at start and daily after that, and send nothing about the machine.

## The index across updates

The index lives outside the binary: `%ProgramData%\FEx\Service` on Windows,
`/var/lib/fex` on Linux. An update stops the service, which saves the index
and the journal position. It then swaps the binary and starts the service
again, which loads the saved index in milliseconds and catches up: the NTFS
change journal on Windows, a timestamp sweep on Linux. Nothing is crawled
again.

These protect that:
- **Format fixtures** (`testdata/formats`). Every build must load the index
  formats released versions write. A format change has to keep reading the
  old format.
- **Protocol byte.** Every request starts with `wire::PROTOCOL`. An app and
  a service of different versions say "restart FEx" instead of misreading
  each other, and status reports the service's version.
- **First crawl.** While the service is building a first index (nothing
  saved yet), the updater refuses unless forced. In the app, "Update
  anyway" forces it.
- **Rollback.** If the new service doesn't answer within 60 s, the previous
  binary is restored and the service restarted on it. On Windows, a
  service registered from another folder (an older install) is moved to
  the Program Files copy.

## One-time setup (repository owner)

1. **Make the signing key** on a machine you trust, ideally offline, with
   a strong password:
   ```
   minisign -G -p fex-release.pub -s fex-release.key
   ```
   minisign is available through `winget install jedisct1.minisign`,
   `apt install minisign`, or `dnf install minisign` (EPEL).
2. **Back up** `fex-release.key` and its password, separately, offline: an
   encrypted USB drive and your password manager. If the key is lost,
   installed copies can't verify future releases and must be reinstalled
   by hand. If it leaks, rotate it (see `packaging/release-keys.pub`).
3. **Create the `release` environment** in the repository's Settings →
   Environments:
   - Required reviewers: yourself.
   - Deployment branches and tags: Selected, tag pattern `v*`.
   - Environment secrets: `MINISIGN_SECRET_KEY` (the whole contents of
     `fex-release.key`, both lines) and `MINISIGN_PASSWORD`.
4. **Protect tags and `main`** in Settings → Rules → Rulesets:
   - A tag ruleset for `v*` that restricts creation, update and deletion,
     with only repository admins allowed to bypass.
   - A branch ruleset for `main` that requires a pull request and the
     `build` checks.
5. **Account and Actions settings.** Use two-factor authentication with a
   passkey or hardware key. Set Settings → Actions → General → Workflow
   permissions to read-only. Review personal access tokens and deploy
   keys.
6. **Publish the public key.** Add the second line of `fex-release.pub`
   (it starts with `RW`) to `packaging/release-keys.pub`. It's public and
   safe to share.

## Making a release

1. Set `version` in `Cargo.toml` (e.g. `0.2.0`) and merge to `main`.
2. Tag and push the tag:
   ```
   git tag v0.2.0
   git push origin v0.2.0
   ```
3. Approve the `release` deployment in Actions. The workflow builds and
   tests both platforms, signs, checks the signatures with the shipped
   updater, and publishes:
   - `fex-windows-x64.exe` and `fex-linux-x64`: what the updater downloads.
   - `fex-X.Y.Z-windows-x64.zip` and `fex-X.Y.Z-linux-x64.tar.gz`: for
     people, with the docs and the systemd unit.
   - `SHA256SUMS`, and a `.minisig` for every file.

Changing `.github/workflows/release.yml` on a `claude/**` branch (or
starting it by hand once it is on `main`) does a dry run. It does
everything except publish, signing with a throwaway key.

Anyone can check a download:
```
minisign -Vm fex-linux-x64 -P <key from packaging/release-keys.pub>
```

The first signed release has to be installed by hand. Earlier builds have
no release key, so they refuse to update themselves.
