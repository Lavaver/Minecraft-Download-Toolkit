# Changelog

## [0.6.0] | Registry & Security Update

- **Resilient downloads** — a single asset timing out or dropping the connection no longer aborts
  the batch. Each file is retried up to 3 times with exponential backoff + jitter, then soft-failed
  and reported via `MdtError::PartialDownloadFailed`, so the rest of the queue still finishes.
- **Cargo-registry flavor asset system** — a *global* registry rooted at `$HOME/.mdt/registry` (override
  `MDT_HOME`), shared across `.minecraft` directories, following the standard layout:
  `src/mojang/<version>/main/` for vanilla assets (content-addressed by SHA-1),
  `index/mojang/.cache/<sha1>` for the `assetIndex` JSON, and `<Modloader>/<version>/` for loader
  libraries and Method A installer JARs. The index is loaded first; every file is downloaded into
  the registry (temp + checksum verified + moved) and then *copied* — never hardlinked — into the
  local `assets/objects/` or `libraries/` dir. Modloader artifacts (libraries and installer JARs)
  are checksum-verified via their Maven `.sha1` sidecar (falling back to `.sha256` then `.md5`),
  and the hash is persisted next to the cached file so warm cache hits stay verifiable offline.

## [0.5.3] | Feature Update: We made a minor tweak to the terminal experience

- **Cargo-style progress** — installs and downloads now print friendly status lines to stderr:
  `Updating <source> index`, `Downloaded <file> by <Minecraft/Loader> <version>`, and a throttled
  `Downloading x/y Component(s) ...` tracker that stays pinned to the bottom of the terminal
  (rewritten in place with `\r` on a TTY). `stdout` stays clean for downstream launchers.

## [0.5.2] | hotfix patch 2: don't kill slow downloads (lol)

0.5.1 was yanked: the global 30s *total* request timeout aborted large-but-healthy downloads
(client.jar, assets) mid-stream. The client now uses a 15s connect timeout plus a 30s *read*
timeout (which resets as bytes arrive), so slow downloads survive while a stalled mirror still
times out. Small metadata fetches (manifests, loader profiles) get a strict 15s per-request
timeout.

## [0.5.1] | hotfix patch 1: We're happy to tell you that we've written a bug :D

0.5.0 was yanked: Fix Modloader Installation Deadlock & Add Resilient Network Fallbacks

## [0.5.0] | Modloader (Un)Install Mechanism Update

- **Uninstall** — `MdtClient::uninstall_modloader` reverts a loader back to pure vanilla using a
  SHA-1 manifest comparison. It pre-loads the manifest table into memory (no N+1 network queries),
  scans on disk with a multithreaded bucket-sorted scanner, and applies MCR markers
  (`[Hash].pinned` modified, `[Hash].pinned.add` created, `[Filename].pinned.remove` removed).
  The `mods/` folder is preserved. Re-running it is safe.
- **1 Pub Function(s) Breaking** — `MdtClient::install_modloader` now takes a vanilla version directory (`&Path`)
  plus a `copy_vanilla: bool` instead of a `&str` version. The Minecraft version is read from the
  directory's `version.json` `id` field and fails fast with `MdtError::InvalidManifest` if that
  JSON is missing or malformed.
- **Copy-on-write** — with `copy_vanilla == true` the vanilla directory is copied into the
  isolated `versions/<mc>+<Loader><ver>` (or the official installer's `-forge-`/`-neoforge-`)
  directory first; with `false` the loader is installed in place, mutating the vanilla directory.
- **New error** — `MdtError::UninstallFailed`.
- **Resilient downloads** — every loader download now follows a mirror-first, official-as-fallback
  policy, and the shared HTTP client carries connect/request timeouts (15s/30s), so a stalled or
  403ing mirror falls back to the official source instead of hanging.
- **Macro Name & Mechanism Breaking** — the `mdt_install!` macro is renamed to `full_install!`, and it now
  installs vanilla and the loader in one shot (with a five-argument form for the copy-on-write
  flag). It expands to the new `MdtClient::full_install`.

## [0.4.0] | custom mirror providers

Mirror endpoints and URL-rewrite rules moved out of the code into `providers.toml`. They're
compiled into static lookup tables at build time by the `mdt-macros::providers!` proc macro
(MDT's take on `rust-i18n`'s `i18n!`), so nothing is parsed at runtime.

- **Custom mirrors** — `Mirror::Custom("name")` picks a `[name]` section from `providers.toml`,
  with its own endpoints and ordered `mirror` / `maven` rewrite rules (plus a `maven_exclude` list).
- **Fallible lookup** — `ManifestUrls::new` split into `try_new` (returns `MdtError::UnknownProvider`)
  and `new` (panics).
- **New error** — `MdtError::UnknownProvider`.
- **Breaking** — `Mirror` is now `Clone` (not `Copy`), and mirrors are passed by reference (`&Mirror`).
- **Docs** — `docs/mirror_toml.md` and `docs/mirror_uri.md`.
- **Workspace** — `mdt-macros` is now a separate member crate.

## [0.3.1] | Dependencies Update

Added the README badge and bumped Tokio.

## [0.3.0] | First Released

First release of MDT as a library.
