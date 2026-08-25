#!/bin/sh
# gage installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/gageml/gage/main/scripts/install.sh | sh
#
# Environment overrides:
#   GAGE_VERSION       install a specific tag (default: latest release, prereleases included)
#   GAGE_INSTALL_DIR   install directory   (default: $XDG_BIN_HOME, else ~/.local/bin)
#
# Installs the `gage` binary for the current platform as a non-root user.

set -eu

REPO="gageml/gage"
BIN="gage"

need() {
    for cmd in "$@"; do
        command -v "$cmd" >/dev/null 2>&1 || err "required command not found: $cmd"
    done
}

err() {
    printf 'error: %s\n' "$*" >&2; exit 1;
}

info() {
    printf '%s\n' "$*" >&2;
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    # Under Rosetta 2 `uname -m` reports x86_64 on Apple Silicon; sysctl
    # reports the actual hardware. Borrowed from cargo-dist's installer.
    if [ "$os" = Darwin ] && [ "$arch" = x86_64 ]; then
        if [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
            arch="arm64"
        fi
    fi
    case "${os}/${arch}" in
        Linux/x86_64 | Linux/amd64) echo "x86_64-unknown-linux-gnu" ;;
        Darwin/arm64)               echo "aarch64-apple-darwin" ;;
        *) err "unsupported platform: ${os} ${arch}" ;;
    esac
}

# Raw HTTPS GET to stdout, for metadata and the checksums file itself.
get() {
    curl -fsSL "$1"
}

latest_version() {
    get "https://api.github.com/repos/${REPO}/releases" \
        | grep -m1 '"tag_name"' \
        | sed -E 's/.*"tag_name" *: *"([^"]+)".*/\1/'
}

# Echo the browser_download_url of the release ${version}'s asset whose name
# ends in -${target}.tar.gz. Resolving the asset from the release itself keeps
# install independent of how the tag is named.
asset_url() {
    version="$1"
    target="$2"
    get "https://api.github.com/repos/${REPO}/releases/tags/${version}" \
        | grep '"browser_download_url"' \
        | sed -E 's/.*"browser_download_url" *: *"([^"]+)".*/\1/' \
        | grep -m1 -- "-${target}\.tar\.gz$"
}

checksums_url() {
    version="$1"
    get "https://api.github.com/repos/${REPO}/releases/tags/${version}" \
        | grep '"browser_download_url"' \
        | sed -E 's/.*"browser_download_url" *: *"([^"]+)".*/\1/' \
        | grep -m1 -- '/SHA256SUMS$'
}

# Download the archive at $archive_url and its checksums at $sums_url into
# $tmp, then verify the archive against SHA256SUMS. Returns only when
# ${tmp}/${archive} is a checksum-verified copy; otherwise aborts. $archive is
# the asset's basename, which must match its entry in SHA256SUMS.
get_archive() {
    archive_url="$1"
    sums_url="$2"
    archive="$3"
    tmp="$4"

    info "Downloading $archive"
    get "$archive_url" > "${tmp}/${archive}"
    get "$sums_url" > "${tmp}/SHA256SUMS"

    info "Verifying archive"
    expected="$(awk -v f="$archive" '$2 == f || $2 == "*" f {print $1}' "${tmp}/SHA256SUMS")"
    [ -n "$expected" ] || err "no checksum for ${archive} in SHA256SUMS"
    [ "$(sha256 "${tmp}/${archive}")" = "$expected" ] \
        || err "checksum mismatch for ${archive}"
}

# Print the SHA-256 of $1. sha256sum is coreutils; macOS ships shasum.
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

check_path() {
    dir="$1"
    case ":${PATH}:" in
        *":${dir}:"*) ;;
        *) info "note: ${dir} is not on your PATH; add it to use ${BIN} directly" ;;
    esac
}

main() {
    need curl tar
    command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 \
        || err "required command not found: sha256sum or shasum"

    target="$(detect_target)"
    version="${GAGE_VERSION:-$(latest_version)}"
    [ -n "$version" ] || err "could not determine release version"
    install_dir="${GAGE_INSTALL_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"

    info "Installing ${BIN} ${version} (${target}) to ${install_dir}"

    archive_url="$(asset_url "$version" "$target")"
    [ -n "$archive_url" ] || err "no ${target} asset in release ${version}"
    sums_url="$(checksums_url "$version")"
    [ -n "$sums_url" ] || err "no SHA256SUMS in release ${version}"
    archive="${archive_url##*/}"

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    get_archive "$archive_url" "$sums_url" "$archive" "$tmp"
    tar -C "$tmp" -xzf "${tmp}/${archive}"

    info "Installing $BIN to ${install_dir}"
    mkdir -p "$install_dir"
    install -m 0755 "${tmp}/${BIN}" "${install_dir}/${BIN}"
    check_path "$install_dir"
}

main "$@"
