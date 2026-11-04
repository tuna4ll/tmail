#!/bin/sh
# Installs termail from the GitHub releases on Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/tuna4ll/termail/main/install.sh | sh
#
# TERMAIL_VERSION      a release such as v0.2.0 (default: the latest)
# TERMAIL_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
set -eu

repo="tuna4ll/termail"
version="${TERMAIL_VERSION:-latest}"
dir="${TERMAIL_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "termail: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Linux) os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *) fail "no build for $(uname -s). On Windows, use install.ps1." ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *) fail "no build for $(uname -m) processors." ;;
esac
# A shell running under Rosetta reports x86_64 on Apple silicon.
if [ "$os" = "apple-darwin" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
    arch="aarch64"
fi
if [ "$os" = "unknown-linux-gnu" ] && ldd --version 2>&1 | grep -qi musl; then
    fail "the Linux build needs glibc. On musl systems such as Alpine, use: cargo install --git https://github.com/$repo"
fi

asset="termail-$arch-$os.tar.gz"
if [ "$version" = "latest" ]; then
    base="https://github.com/$repo/releases/latest/download"
else
    base="https://github.com/$repo/releases/download/$version"
fi

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        fail "curl or wget is needed to download."
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset ($version)"
download "$base/$asset" "$tmp/$asset" || fail "unable to download $base/$asset"
download "$base/$asset.sha256" "$tmp/$asset.sha256" || fail "unable to download the checksum"
expected="$(cut -d ' ' -f 1 < "$tmp/$asset.sha256")"
[ "$(sha256 "$tmp/$asset")" = "$expected" ] || fail "the download does not match its checksum."

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/termail" "$dir/termail"
echo "Installed $("$dir/termail" --version) to $dir/termail"

case ":$PATH:" in
    *":$dir:"*) echo "Run termail to start." ;;
    *) echo "Add $dir to your PATH, then run termail." ;;
esac
