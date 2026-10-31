# pica-rust

Rust workspace for pica migration.

## Binaries

- `pica` (device-side CLI)
- `pica-pack` (build-side packer)

## Goals

- Minimize dependencies for OpenWrt footprint.
- Improve maintainability, error handling, and testability.

## Non-goals

- Writing into `opkg`/`apk`'s installed database, or solving dependencies for them.
- Parsing backend container formats (`.ipk`/`.apk`); they are handed to the backend that installs them. The one exception is `pica-pack` measuring a container's payload for `size`.

## Workspace layout

```text
./
  crates/
    pica-pkg-core/  # shared domain/runtime helpers
    pica-pkg-cli/   # device-side CLI
    pica-pkg-pack/  # build-side packer
```

## Implemented

### `pica-pack`

- `build <pkgdir> [--outdir DIR] [--pkgmgr opkg|apk|pica|opkg,apk]`
- Build targets from the manifest `targets` field, overridable with `--pkgmgr`
- One output directory per backend: `<outdir>/opkg/`, `<outdir>/apk/`, `<outdir>/pica/`
- Backend-suffixed filenames (`-apk` / `-pica`), `platform` segment omitted when `platform = all`
- Payload layout: `scripts/` (lifecycle), `root/` (path-mirrored), `pkgs/<backend>/[<platform>/<arch>/]` for `opkg`/`apk` only
- A `pica` target accepts `scripts/` and `root/` and refuses `pkgs/pica/`: expanding a container into `root/` would take file ownership away from the backend's database and drop its dependency solving
- Matrix build per backend for `pkgs/<backend>/<platform>/<arch>`
- Build manifest rewrite (`builddate/size/platform/arch/pkgver/pkgrel/pkgmgr` injection)
- `size` counted from what a device installs: `root/` plus each container's unpacked payload for `opkg`/`apk`; a container that cannot be unpacked (apk-tools 3.x ADB) is reported as unmeasured rather than counted as zero
- Per-backend `index.json` (schema 1) and `SHA256SUMS`, plus an entry `index.json` at the output root
- Archive creation via system `tar`

### `pica`

- `-S [selector]` (no selector: sync repo metadata; with selector: install auto)
- `-Q`, `-Qi`, `-Ql` (query installed db)
- `-So`, `-Si`, `-Sp`
- `-U`, `-R`, `-Su`, `-Syu`
- Install precheck/report flow (`install-report.json`)
- Backends: `opkg`, `apk`, and the Pica-native `pica` (`--pkgmgr` > manifest > config > auto-detect)
- Lifecycle scripts and env persistence/removal
- `--json`, `--json-errors`, `--non-interactive`, `--feed-policy`, `--pkgmgr`
- File lock fallback (`db.lck.d`) with RAII release

## Build

```bash
cargo build --workspace --release
```

## Notes for OpenWrt

- Keep dependency set minimal (`serde`, `serde_json`, std-only argument parsing).
- Use system tools (`opkg`/`apk`, `uclient-fetch/wget/curl`, `tar`) instead of bundling heavy Rust crates.
- Prioritize deterministic, explicit errors over implicit fallbacks.

See `docs/developer.md` for the workspace layout and `docs/standard.md` for the package format.
