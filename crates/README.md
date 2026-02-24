# pica-rs

Rust workspace for pica migration.

## Binaries

- `pica-rs` (CLI replacement for `legacy/pica-cli/pica`)
- `pica-pack-rs` (packer replacement for `legacy/pica-pack/pica-pack`)

## Goals

- Keep behavior aligned with existing Bash implementation first.
- Minimize dependencies for OpenWrt footprint.
- Improve maintainability, error handling, and testability.

## Workspace layout

```text
./
  crates/
    pica-core/      # shared domain/runtime helpers
    pica-cli-rs/    # device-side CLI
    pica-pack-rs/   # build-side packer
```

## Implemented

### `pica-pack-rs`

- `build <staging_dir> [--outdir DIR]`
- Manifest compatibility handling (`pkgver/pkgrel` and legacy fallback)
- Matrix build for `binary/<platform>/<arch>` and `depend/<platform>/<arch>`
- Strict package filename composition (same rule as bash)
- Build manifest rewrite (`builddate/size/platform/arch/pkgver/pkgrel` injection)
- Archive creation via system `tar`

### `pica-rs`

- `-S` (sync repo metadata, strict `repo.json` validation)
- `-Q`, `-Qi`, `-Ql` (query installed db)
- `-So`, `-Si`, `-Sp`
- `-U`, `-R`, `-Su`, `-Syu`
- Install precheck/report flow (`install-report.json`)
- Lifecycle hooks and cmd/env persistence/removal
- `--json`, `--json-errors`, `--non-interactive`, `--feed-policy`
- File lock fallback (`db.lck.d`) with RAII release

## Build

```bash
cargo build --workspace --release
```

## Notes for OpenWrt

- Keep dependency set minimal (`serde`, `serde_json`, std-only argument parsing).
- Use system tools (`opkg`, `uclient-fetch/wget/curl`, `tar`) instead of bundling heavy Rust crates.
- Prioritize deterministic, explicit errors over implicit bash-style fallbacks.

See `docs/architecture.md` for migration rationale and staged rollout.
