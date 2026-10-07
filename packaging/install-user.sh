#!/bin/sh
# Legt kladde fuer den aktuellen Benutzer an: Binary nach ~/.local/bin,
# Icon und Desktop-Datei in die XDG-Verzeichnisse, PDF-Standard setzen.
# Ohne root, ohne Systempfad, alles unter $HOME.
#
#   ./packaging/install-user.sh              # baut release und benutzt das
#   ./packaging/install-user.sh pfad/kladde  # ein Binary vorgeben
set -eu

hier=$(dirname "$0")
quelle=${1:-}
if [ -z "$quelle" ]; then
    cargo build --release --quiet
    quelle=target/release/kladde
fi
[ -x "$quelle" ] || { echo "kein ausfuehrbares Binary: $quelle" >&2; exit 1; }

daten=${XDG_DATA_HOME:-$HOME/.local/share}
mkdir -p "$HOME/.local/bin" "$daten/applications" \
         "$daten/icons/hicolor/scalable/apps"

install -m 755 "$quelle" "$HOME/.local/bin/kladde"
install -m 644 "$hier/kladde.svg" "$daten/icons/hicolor/scalable/apps/kladde.svg"

# Das @KLADDE@ muss weg, sonst startet die Desktop-Datei nichts.
sed "s|@KLADDE@|$HOME/.local/bin/kladde|" "$hier/kladde.desktop.in" \
    > "$daten/applications/kladde.desktop"

# Standard setzen: die Desktop-Datei allein sagt nur "ich kann PDFs".
if command -v xdg-mime >/dev/null; then
    xdg-mime default kladde.desktop application/pdf
else
    echo "xdg-mime fehlt - Standard bitte von Hand setzen:" >&2
    echo "  [Default Applications]"                 >&2
    echo "  application/pdf=kladde.desktop"         >&2
    echo "  >> \$HOME/.config/mimeapps.list"        >&2
fi
command -v update-desktop-database >/dev/null \
    && update-desktop-database "$daten/applications" || true

echo "installiert fuer $(id -un).  Nachsehen:"
echo "  xdg-mime query default application/pdf"
echo "  kladde --help"
