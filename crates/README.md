# pica-rust

Rust workspace for pica migration.

## Binaries

- `pica` (device-side CLI)
- `pica-pack` (build-side packer)

## Goals

- Minimize dependencies for OpenWrt footprint.
- Improve maintainability, error handling, and testability.

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

- `build <pkgdir> [--outdir DIR] [--pkgmgr opkg|apk|none|opkg,apk]`
- Build targets from the manifest `targets` field, overridable with `--pkgmgr`
- One output directory per backend: `<outdir>/opkg/`, `<outdir>/apk/`, `<outdir>/none/`
- Backend-suffixed filenames (`-apk` / `-none`), `platform` segment omitted when `platform = all`
- Payload layout: `scripts/` (lifecycle), `root/` (path-mirrored), `pkgs/<backend>/[<platform>/<arch>/]`
- Matrix build per backend for `pkgs/<backend>/<platform>/<arch>`
- Build manifest rewrite (`builddate/size/platform/arch/pkgver/pkgrel/pkgmgr` injection)
- Per-backend `repo.json` (schema 2) and `SHA256SUMS`
- Archive creation via system `tar`

### `pica`

- `-S [selector]` (no selector: sync repo metadata; with selector: install auto)
- `-Q`, `-Qi`, `-Ql` (query installed db)
- `-So`, `-Si`, `-Sp`
- `-U`, `-R`, `-Su`, `-Syu`
- Install precheck/report flow (`install-report.json`)
- Backends: `opkg`, `apk`, and Pica-native `none` (`--pkgmgr` > manifest > config > auto-detect)
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
