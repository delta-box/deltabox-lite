#!/bin/sh
# Install the deltabox CLI from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/delta-box/deltabox-lite/main/scripts/install.sh | sh
#
# Picks the tarball for this machine, verifies its sha256 against the sidecar
# published next to it, and puts `deltabox` in $HOME/.local/bin. No sudo, no
# Node, no Rust. Override with environment variables:
#
#   DELTABOX_VERSION=0.2.0        install this version instead of the latest
#   DELTABOX_INSTALL_DIR=/usr/local/bin   install somewhere else
#   DELTABOX_BASE_URL=file:///path/to/dist   fetch from a local build (testing)
#
set -eu

repo="delta-box/deltabox-lite"

log() { printf 'deltabox-install: %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

# tmp is read by cleanup() under `set -u`, so keep it bound even if we are
# interrupted before main() ever assigns it a real mktemp path.
tmp=""
cleanup() {
  if [ -n "$tmp" ]; then
    rm -rf "$tmp"
  fi
}
trap cleanup EXIT
# Separate handlers so Ctrl-C/kill still clean up *and* exit with the
# conventional signal-based status, instead of falling through as if we'd
# finished normally. Disarm the other traps first so cleanup only runs once.
trap 'trap - EXIT INT TERM; cleanup; exit 130' INT
trap 'trap - EXIT INT TERM; cleanup; exit 143' TERM

# --- pick the tooling ----------------------------------------------------------
# https URLs get pinned to TLS + a locked-down redirect protocol; file:// and
# plain http:// (used by local/test servers) are left alone.
if command -v curl >/dev/null 2>&1; then
  fetch() {
    case "$1" in
      https://*)
        curl -fsSL --proto '=https' --tlsv1.2 \
          --connect-timeout 15 --max-time 300 --retry 3 --retry-delay 1 \
          "$1" -o "$2"
        ;;
      *)
        curl -fsSL --connect-timeout 15 --max-time 300 --retry 3 --retry-delay 1 "$1" -o "$2" ;;
    esac
  }
  fetch_text() {
    case "$1" in
      https://*)
        curl -fsSL --proto '=https' --tlsv1.2 \
          --connect-timeout 15 --max-time 300 --retry 3 --retry-delay 1 "$1"
        ;;
      *)
        curl -fsSL --connect-timeout 15 --max-time 300 --retry 3 --retry-delay 1 "$1" ;;
    esac
  }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q --timeout=15 --tries=3 "$1" -O "$2"; }
  fetch_text() { wget -q --timeout=15 --tries=3 "$1" -O -; }
else
  die "curl or wget is required"
fi

if command -v shasum >/dev/null 2>&1; then
  verify() { (cd "$1" && shasum -a 256 -c "$2" >/dev/null); }
elif command -v sha256sum >/dev/null 2>&1; then
  verify() { (cd "$1" && sha256sum -c "$2" >/dev/null); }
else
  die "shasum or sha256sum is required to verify the download"
fi

main() {
  # --- resolve the install directory --------------------------------------------
  if [ -n "${DELTABOX_INSTALL_DIR:-}" ]; then
    install_dir="$DELTABOX_INSTALL_DIR"
  elif [ -n "${HOME:-}" ]; then
    install_dir="$HOME/.local/bin"
  else
    die '$HOME is not set, so the default install dir is unknown; set DELTABOX_INSTALL_DIR explicitly'
  fi

  # --- pick the target triple ---------------------------------------------------
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *) die "unsupported operating system: $os (macOS and Linux are supported)" ;;
  esac
  case "$arch" in
    arm64 | aarch64) arch_part="aarch64" ;;
    x86_64 | amd64) arch_part="x86_64" ;;
    *) die "unsupported architecture: $arch (arm64 and x86_64 are supported)" ;;
  esac
  if [ "$os" = "Darwin" ] && [ "$arch_part" = "x86_64" ]; then
    # An x86_64 shell on Apple Silicon means we're running under Rosetta;
    # install the native aarch64 build instead.
    if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
      log "detected Rosetta on Apple Silicon; installing the aarch64 build instead of x86_64"
      arch_part="aarch64"
    fi
  fi
  target="${arch_part}-${os_part}"

  # --- resolve the version and download location --------------------------------
  version="${DELTABOX_VERSION:-}"
  if [ -z "$version" ]; then
    # The latest release's tag, without calling jq: the API answer contains a
    # single "tag_name" field.
    version=$(fetch_text "https://api.github.com/repos/${repo}/releases/latest" \
      | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$version" ] || die "could not determine the latest release: the GitHub API call failed or returned no tag (this is often the unauthenticated rate limit of 60 requests/hour); set DELTABOX_VERSION explicitly"
  fi
  version="${version#v}"
  base_url="${DELTABOX_BASE_URL:-https://github.com/${repo}/releases/download/v${version}}"
  tarball="deltabox-${version}-${target}.tar.gz"

  # --- download, verify, extract ---------------------------------------------------
  tmp=$(mktemp -d)

  log "downloading deltabox ${version} for ${target}"
  fetch "${base_url}/${tarball}" "${tmp}/${tarball}" \
    || die "download failed: ${base_url}/${tarball}"
  fetch "${base_url}/${tarball}.sha256" "${tmp}/${tarball}.sha256" \
    || die "download failed: ${base_url}/${tarball}.sha256"
  verify "$tmp" "${tarball}.sha256" || die "checksum mismatch for ${tarball}; refusing to install"
  tar -xzf "${tmp}/${tarball}" -C "$tmp" deltabox

  # --- install ---------------------------------------------------------------------
  mkdir -p "$install_dir"
  # Write to a temporary name first so a running deltabox is replaced atomically.
  cp "${tmp}/deltabox" "${install_dir}/.deltabox.new"
  chmod 755 "${install_dir}/.deltabox.new"
  mv -f "${install_dir}/.deltabox.new" "${install_dir}/deltabox"
  log "installed ${install_dir}/deltabox"

  case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
      log "${install_dir} is not on your PATH; add this to your shell profile:"
      log "  export PATH=\"${install_dir}:\$PATH\""
      ;;
  esac

  "${install_dir}/deltabox" --version </dev/null \
    || die "installed ${install_dir}/deltabox but it could not run; your system may be too old for this build (Linux binaries are built against glibc 2.39) -- try building deltabox from source instead"
}

main "$@"
