#!/bin/sh
# Install the pica binary on an OpenWrt device.
#
#   sh installer.sh                          latest release, /usr/bin
#   sh installer.sh --version v0.4.7         a specific tag
#   sh installer.sh --mirror https://gh-proxy.org/
#   sh installer.sh --dir /tmp/pica --dry-run
#   sh installer.sh --uninstall
#
# The script downloads one static binary, `pica.<arch>`, from the release and
# puts it in place. It does not write /etc/pica: that file is what `pica setup`
# produces, and an unattended installer would bake in choices the user never
# made. It also does not need root to run — only to write the target directory.

set -eu

REPO="${PICA_REPO:-miaoermua/pica}"
VERSION="latest"
MIRROR="${PICA_MIRROR:-}"
DIR="/usr/bin"
DRY_RUN=0
UNINSTALL=0

die() {
  printf 'pica installer: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Install pica on an OpenWrt device.

Usage: sh installer.sh [options]

  --version <tag>   Release tag to install (default: latest)
  --mirror <url>    Prefix the download URL, e.g. https://gh-proxy.org/
  --dir <path>      Install directory (default: /usr/bin)
  --dry-run         Print what would happen and change nothing
  --uninstall       Remove the binary from --dir
  -h, --help        This text

Environment: PICA_REPO (default miaoermua/pica), PICA_MIRROR (default none).
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      [ $# -ge 2 ] || die "--version needs a tag"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      shift
      ;;
    --mirror)
      [ $# -ge 2 ] || die "--mirror needs a URL"
      MIRROR="$2"
      shift 2
      ;;
    --mirror=*)
      MIRROR="${1#*=}"
      shift
      ;;
    --dir)
      [ $# -ge 2 ] || die "--dir needs a path"
      DIR="$2"
      shift 2
      ;;
    --dir=*)
      DIR="${1#*=}"
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --uninstall)
      UNINSTALL=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1 (try --help)"
      ;;
  esac
done

# The release ships `pica.amd64`, `pica.arm64` and `pica.mipsel`: the name says
# what the binary runs on, not what the feed calls the architecture.
#
# `opkg print-architecture` and `apk --print-arch` answer with the feed's own
# names (`mipsel_24kc`, `aarch64_cortex-a53`), which are finer than what the
# release needs — the first match decides. `uname -m` is the fallback for a host
# with neither, and it cannot tell mips from mipsel: every device pica publishes
# for is little-endian, so that is the reading it takes.
detect_arch() {
  raw=""
  if command -v opkg >/dev/null 2>&1; then
    raw=$(opkg print-architecture 2>/dev/null |
      awk '$1 == "arch" && $2 != "all" && $2 != "noarch" { print $2; exit }')
  fi
  if [ -z "$raw" ] && command -v apk >/dev/null 2>&1; then
    raw=$(apk --print-arch 2>/dev/null | head -n 1)
  fi
  if [ -z "$raw" ]; then
    raw=$(uname -m)
  fi

  case "$raw" in
    x86_64 | x86-64 | amd64) echo amd64 ;;
    aarch64* | arm64 | armv8*) echo arm64 ;;
    mipsel*) echo mipsel ;;
    mips | mips_*)
      # `uname -m` says `mips` on ramips too, and those are little-endian. A
      # big-endian router has no artifact here; saying so is better than
      # installing one that cannot exec.
      echo mipsel
      ;;
    *) return 1 ;;
  esac
}

# `-O` (capital) is the output file. BusyBox wget's `-o` is the *log* file, and
# a binary installed that way runs a progress report as a shell script.
fetch() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    # Silent: the progress bar redraws over a serial console, and -S keeps the
    # error text the silence would otherwise hide.
    curl -fsSL --connect-timeout 20 --retry 3 --retry-delay 2 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url"
  elif command -v uclient-fetch >/dev/null 2>&1; then
    uclient-fetch -q -O "$out" "$url"
  else
    die "no downloader: install curl, wget or uclient-fetch"
  fi
}

# A 200 from a captive portal or a proxy error page is still a file; only the
# ELF magic says the download is the binary that was asked for.
#
# Read with `head` and matched by the shell, not hex-dumped: BusyBox builds drop
# `od` as readily as they keep it, and an installer that needs a tool the device
# may not have fails after the download instead of before it. `?ELF` is exactly
# four characters — the first is 0x7f, which glob matches as `?`, and `head -c`
# hands the bytes through untouched.
is_elf() {
  magic=$(head -c 4 "$1" 2>/dev/null)
  case "$magic" in
    ?ELF) return 0 ;;
    *) return 1 ;;
  esac
}

if [ "$UNINSTALL" = 1 ]; then
  if [ "$DRY_RUN" = 1 ]; then
    printf 'would remove %s/pica\n' "$DIR"
    exit 0
  fi
  rm -f "$DIR/pica"
  printf 'removed %s/pica\n' "$DIR"
  printf 'kept: /etc/pica/pica.json and /var/lib/pica (the configuration and the\n'
  printf 'package ledger). Remove them by hand if this was a full uninstall.\n'
  exit 0
fi

if ! ARCH=$(detect_arch); then
  die "no pica build for '$(uname -m)'; opkg and apk did not name an architecture"
fi
case "$VERSION" in
  latest | "") URL="https://github.com/$REPO/releases/latest/download/pica.$ARCH" ;;
  *) URL="https://github.com/$REPO/releases/download/$VERSION/pica.$ARCH" ;;
esac
if [ -n "$MIRROR" ]; then
  URL="$MIRROR$URL"
fi

if [ "$DRY_RUN" = 1 ]; then
  printf 'arch:    %s\n' "$ARCH"
  printf 'from:    %s\n' "$URL"
  printf 'to:      %s/pica\n' "$DIR"
  exit 0
fi

[ -d "$DIR" ] || mkdir -p "$DIR" || die "cannot create $DIR"
[ -w "$DIR" ] || die "$DIR is not writable; run as root or pass --dir"

# One temporary file in the target directory, so the final move is a rename on
# the same filesystem — a device losing power mid-install keeps the old binary.
STAGE="$DIR/.pica.$$"
trap 'rm -f "$STAGE"' EXIT INT TERM

printf 'Downloading pica for %s...\n' "$ARCH"
fetch "$URL" "$STAGE" || die "download failed: $URL"

if ! is_elf "$STAGE"; then
  printf 'not an ELF binary — first bytes:\n' >&2
  head -c 120 "$STAGE" >&2
  printf '\n' >&2
  die "the download is not a pica binary (mirror returned an error page?)"
fi

chmod 0755 "$STAGE"
mv -f "$STAGE" "$DIR/pica"
trap - EXIT INT TERM

printf 'Installed %s/pica\n' "$DIR"
installed=$("$DIR/pica" -V 2>/dev/null | head -n 1) || installed=""
if [ -n "$installed" ]; then
  printf '  %s\n' "$installed"
else
  printf '  (the binary did not run here — wrong architecture?)\n'
fi

if ! command -v tar >/dev/null 2>&1; then
  printf 'Note: tar is missing. pica needs it to unpack artifacts.\n'
fi

printf '\nNext: pica setup\n'
