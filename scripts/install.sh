#!/usr/bin/env sh
set -eu

repo="pedro-canedo/smith"
bin="smith"
install_dir="${SMITH_INSTALL_DIR:-/usr/local/bin}"

target="${SMITH_TARGET:-}"
if [ -z "$target" ]; then
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os:$arch" in
    Linux:x86_64|Linux:amd64) target="x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-gnu" ;;
    Darwin:x86_64|Darwin:amd64) target="x86_64-apple-darwin" ;;
    Darwin:arm64|Darwin:aarch64) target="aarch64-apple-darwin" ;;
    *) echo "unsupported platform: $os $arch" >&2; exit 2 ;;
  esac
fi

version="${SMITH_VERSION:-}"
if [ -z "$version" ]; then
  version="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | head -n 1)"
fi
if [ -z "$version" ]; then
  echo "could not resolve latest smith release" >&2
  exit 2
fi

plain_version="${version#v}"
name="$bin-$plain_version-$target"
archive="$name.tar.gz"
tmp="${TMPDIR:-/tmp}/smith-install-$$"

cleanup() {
  rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

mkdir -p "$tmp"
cd "$tmp"

base="https://github.com/$repo/releases/download/$version"
curl -fsSLO "$base/$archive"
curl -fsSLO "$base/$archive.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "$archive.sha256"
elif command -v shasum >/dev/null 2>&1; then
  shasum -a 256 -c "$archive.sha256"
else
  echo "warning: no sha256 checker found; installing without checksum verification" >&2
fi

tar xzf "$archive"
mkdir -p "$install_dir"
cp "$name/$bin" "$install_dir/$bin"
chmod 0755 "$install_dir/$bin"

echo "installed $bin $version to $install_dir/$bin"
