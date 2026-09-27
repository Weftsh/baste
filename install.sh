#!/bin/sh
# Install the latest Baste release into ~/.local/bin (or $BASTE_INSTALL_DIR).
#   curl -fsSL https://raw.githubusercontent.com/weftsh/baste/main/install.sh | sh
set -eu

repo="weftsh/baste"
dir="${BASTE_INSTALL_DIR:-$HOME/.local/bin}"
version="${BASTE_VERSION:-}"

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  Darwin/x86_64)
    # An arm64 Mac running this shell under Rosetta still reports x86_64.
    if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
      target="aarch64-apple-darwin"
    else
      echo "baste: Intel Macs are not supported (the Tart backend needs Apple Silicon)." >&2
      exit 1
    fi
    ;;
  Linux/x86_64) target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64 | Linux/arm64) target="aarch64-unknown-linux-musl" ;;
  *)
    echo "baste: $os/$arch is not supported. On Windows, install inside WSL2." >&2
    exit 1
    ;;
esac

fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1"; else wget -qO- "$1"; fi
}

if [ -z "$version" ]; then
  version=$(fetch "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
fi
[ -n "$version" ] || { echo "baste: couldn't find the latest release" >&2; exit 1; }

name="baste-$version-$target.tar.gz"
base="https://github.com/$repo/releases/download/$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $name"
fetch "$base/$name" > "$tmp/$name"
fetch "$base/SHA256SUMS" > "$tmp/SHA256SUMS"
expected=$(grep " $name\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$name" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/$name" | cut -d' ' -f1)
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "baste: checksum mismatch for $name" >&2; exit 1; }

tar -xzf "$tmp/$name" -C "$tmp"
mkdir -p "$dir"
for f in baste baste-linux-aarch64 baste-linux-x86_64; do
  if [ -f "$tmp/$f" ]; then
    install -m 0755 "$tmp/$f" "$dir/$f"
  fi
done
echo "Installed baste $version to $dir/baste"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH, then run: baste init" ;;
esac
