#!/bin/bash
# RojoHub launcher: sets up the command line tool, Studio plugins and background
# service on first run, then opens the dashboard.
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/../Resources/bin/rojohub"
HOME_DIR="${ROJOHUB_HOME:-$HOME/.rojohub}"
HUB="http://127.0.0.1:${ROJOHUB_PORT:-34870}"
LOG="$HOME_DIR/app.log"
mkdir -p "$HOME_DIR/bin"
exec >>"$LOG" 2>&1

say() { osascript -e "display dialog \"$1\" with title \"RojoHub\" buttons {\"OK\"} default button 1 with icon note" >/dev/null 2>&1; }
fail() { osascript -e "display alert \"RojoHub\" message \"$1\" as critical" >/dev/null 2>&1; exit 1; }

VERSION="$("$BIN" version 2>/dev/null)" || fail "The bundled rojohub binary could not start. If macOS blocked it, open Terminal and run: xattr -dr com.apple.quarantine /Applications/RojoHub.app"
echo "=== $(date) RojoHub.app $VERSION from $DIR"

# Command line tool, pointing at this copy of the app.
cat > "$HOME_DIR/bin/rojohub" <<SHIM
#!/bin/sh
exec "$BIN" "\$@"
SHIM
chmod +x "$HOME_DIR/bin/rojohub"
for d in /opt/homebrew/bin /usr/local/bin; do
  if [ -d "$d" ] && [ -w "$d" ]; then ln -sf "$HOME_DIR/bin/rojohub" "$d/rojohub"; LINKED="$d"; break; fi
done

# First run of this version: install the Studio plugins.
MARK="$HOME_DIR/.installed-$VERSION"
if [ ! -f "$MARK" ]; then
  CHOICE=$(osascript -e 'button returned of (display dialog "RojoHub will install its Studio plugin.\n\nIt can also install the RojoHub sync plugin, which connects Studio to the right Rojo server by itself. It replaces your Rojo plugin file; a backup is kept and `rojohub install-plugin --stock` restores it.\n\nWithout it you type each port into the Rojo plugin yourself." with title "RojoHub setup" buttons {"RojoHub only", "Install both"} default button "Install both" with icon note)' 2>/dev/null)
  if [ "$CHOICE" = "RojoHub only" ]; then
    "$BIN" install-plugin --stock
  else
    "$BIN" install-plugin
  fi
  touch "$MARK"
  FIRST=1
fi

# Register the background service, or re-point it if the app moved. Leave a running,
# up-to-date hub alone so open Rojo sessions aren't interrupted.
PLIST="$HOME/Library/LaunchAgents/dev.rojohub.hub.plist"
if ! grep -qF "$BIN" "$PLIST" 2>/dev/null || ! curl -sf "$HUB/health" | grep -q "\"$VERSION\""; then
  "$BIN" install-service || fail "Could not start the background service. See $LOG"
fi

for i in $(seq 1 50); do
  curl -sf "$HUB/health" >/dev/null && break
  sleep 0.2
done
open "$HUB/"

if [ -n "$FIRST" ]; then
  if [ -n "$LINKED" ]; then WHERE="The rojohub command is installed in $LINKED."
  else WHERE="Add the rojohub command to your shell with:\necho 'export PATH=\\\"\$HOME/.rojohub/bin:\$PATH\\\"' >> ~/.zshrc"; fi
  say "RojoHub is running and starts at login.\n\n$WHERE\n\nRestart any open Studio windows. Then, in a Rojo project folder with its place open in Studio:\n  rojohub add . --from-studio\n  rojohub start\n\nFor agents: rojohub mcp-config"
fi
