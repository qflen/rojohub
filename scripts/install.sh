#!/bin/sh
# Installs the RojoHub binary from GitHub Releases into ~/.rojohub/bin, verifies its SHA-256,
# and links it into /opt/homebrew/bin or /usr/local/bin when one of them is writable.
#
#   curl -fsSL https://github.com/qflen/rojohub/releases/latest/download/install.sh | sh
#
#   ROJOHUB_VERSION=0.4.0        install that release instead of the latest
#   ROJOHUB_INSTALL_DIR=/some/dir  put the binary there instead of ~/.rojohub/bin
set -eu

REPO="${ROJOHUB_REPO:-qflen/rojohub}"
VERSION="${ROJOHUB_VERSION:-latest}"
fail() { echo "rojohub install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) fail "unsupported OS $(uname -s); see https://github.com/$REPO/releases" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=arm64 ;;
  x86_64|amd64) arch=x64 ;;
  *) fail "unsupported CPU $(uname -m); see https://github.com/$REPO/releases" ;;
esac
asset="rojohub-$os-$arch"
if [ "$VERSION" = latest ]; then base="https://github.com/$REPO/releases/latest/download"
else base="https://github.com/$REPO/releases/download/v$VERSION"
fi

dir="${ROJOHUB_INSTALL_DIR:-${ROJOHUB_HOME:-$HOME/.rojohub}/bin}"
mkdir -p "$dir"
tmp="$dir/.rojohub.download.$$"
fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL --retry 3 "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
  else fail "need curl or wget"
  fi
}
echo "Downloading $asset from $base"
fetch "$base/$asset" "$tmp" || { rm -f "$tmp"; fail "could not download $base/$asset"; }
fetch "$base/SHA256SUMS" "$tmp.sums" || { rm -f "$tmp" "$tmp.sums"; fail "could not download $base/SHA256SUMS"; }
want="$(grep -E "[[:space:]]\*?$asset\$" "$tmp.sums" | awk '{print $1}' | head -1)"
if command -v sha256sum >/dev/null 2>&1; then got="$(sha256sum "$tmp" | awk '{print $1}')"
else got="$(shasum -a 256 "$tmp" | awk '{print $1}')"
fi
[ -n "$want" ] && [ "$want" = "$got" ] || { rm -f "$tmp" "$tmp.sums"; fail "checksum mismatch for $asset (expected ${want:-nothing listed}, got $got)"; }
rm -f "$tmp.sums"
chmod 755 "$tmp"
mv "$tmp" "$dir/rojohub"

linked=""
for d in /opt/homebrew/bin /usr/local/bin; do
  if [ -d "$d" ] && [ -w "$d" ] && [ "$d" != "$dir" ]; then ln -sf "$dir/rojohub" "$d/rojohub"; linked="$d"; break; fi
done
echo "Installed rojohub $("$dir/rojohub" version) to $dir/rojohub"
if [ -n "$linked" ]; then echo "Linked as $linked/rojohub"
else echo "Add it to your PATH: export PATH=\"$dir:\$PATH\""
fi
echo
echo "Next:"
echo "  rojohub install-plugin     # RojoHub plugin + RojoHub sync plugin into Studio"
echo "  rojohub install-service    # run the hub at login"
echo "  rojohub mcp-config         # register the MCP server with your agent"
