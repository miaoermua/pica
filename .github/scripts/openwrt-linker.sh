#!/usr/bin/env bash
#
# Linker shim for the MIPS target, which is built against OpenWrt's toolchain
# instead of rust-std. See "OpenWrt Rust Binaries" in ../workflows.
#
# rustc hands the linker the crt objects by bare name: crt1.o, crti.o,
# crtbegin.o, crtend.o, crtn.o. That is all `crt-objects-fallback: musl` can
# produce for a target that ships no prebuilt std, and GNU ld resolves such
# names only through the SEARCH_DIR it was configured with. OpenWrt configures
# that to its own build directory and ld does not relocate it:
#
#   SEARCH_DIR("=/builder/shared-workdir/build/staging_dir/toolchain-…/mipsel-openwrt-linux-musl/lib")
#
# so on any other machine every one of them comes up as "cannot find crt1.o"
# even though the files sit in the toolchain. -lc is unaffected, because -l
# is resolved through the -L directories the driver passes on.
#
# This shim rewrites those bare names to their real paths inside the unpacked
# toolchain and hands the rest of the command line, untouched, to OpenWrt's gcc.
set -euo pipefail

: "${PICA_TOOLCHAIN:?PICA_TOOLCHAIN must point at the unpacked OpenWrt toolchain}"
: "${PICA_CC:?PICA_CC must name the gcc driver of the toolchain}"

# crt1.o, crti.o, crtn.o and libc.a live in the toolchain lib directory. It is
# also reachable through the target-name symlink that gcc itself reports for
# -print-file-name, which is what the workflow's verification step checks.
lib="$PICA_TOOLCHAIN/mipsel-openwrt-linux-musl/lib"
# crtbegin*/crtend* live with the compiler, one directory version deep.
gcc_lib=$(dirname "$(find "$PICA_TOOLCHAIN/lib/gcc" -name crtbegin.o -print -quit)")

argv=()
for arg in "$@"; do
  case $arg in
    crt1.o | Scrt1.o | rcrt1.o | crti.o | crtn.o) argv+=("$lib/$arg") ;;
    crtbegin.o | crtbeginS.o | crtbeginT.o | crtend.o | crtendS.o) argv+=("$gcc_lib/$arg") ;;
    *) argv+=("$arg") ;;
  esac
done

exec "$PICA_CC" "${argv[@]}"
