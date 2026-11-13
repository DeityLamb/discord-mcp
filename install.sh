#!/bin/sh
# Installs the latest discord-mcp release for this machine (Linux or macOS).
#   curl -fsSL https://raw.githubusercontent.com/DeityLamb/discord-mcp/main/install.sh | sh
# Set DISCORD_MCP_VERSION=v0.1.0 for a given release, DISCORD_MCP_INSTALL_DIR for another place
# than ~/.local/bin.
set -eu

repo="DeityLamb/discord-mcp"
dir="${DISCORD_MCP_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *) echo "discord-mcp: no prebuilt binary for $(uname -s); use cargo install discord-mcp" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) echo "discord-mcp: no prebuilt binary for $(uname -m); use cargo install discord-mcp" >&2; exit 1 ;;
esac
target="$arch-$os"

version="${DISCORD_MCP_VERSION:-}"
if [ -z "$version" ]; then
  # The latest release's tag, from where GitHub redirects to (no API rate limit).
  version="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")"
  version="${version##*/}"
fi

name="discord-mcp-$version-$target"
base="https://github.com/$repo/releases/download/$version"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading discord-mcp $version for $target"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected="$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
if command -v sha256sum > /dev/null; then
  actual="$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)"
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "discord-mcp: checksum mismatch, not installing" >&2
  exit 1
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/$name/discord-mcp" "$dir/discord-mcp"
echo "Installed $dir/discord-mcp"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Note: $dir is not on your PATH" ;;
esac
