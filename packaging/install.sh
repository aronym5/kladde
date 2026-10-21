#!/bin/sh
# Installiert kladde: Binary, Desktop-Datei, Icon und Lizenz unter PREFIX.
#
#   ./packaging/install.sh                 # ~/.local
#   sudo ./packaging/install.sh            # /usr/local
#   sudo ./packaging/install.sh -p /usr    # /usr
#   ./packaging/install.sh --static        # statisch bauen
#   ./packaging/install.sh pfad/kladde     # vorhandenes Binary nehmen
#
#   PREFIX/bin/kladde
#   PREFIX/share/applications/kladde.desktop
#   PREFIX/share/icons/hicolor/scalable/apps/kladde.svg
#   PREFIX/share/licenses/kladde/LICENSE
#
# Optionen:
#   --user | --system   Modus erzwingen (Default: aus der UID)
#   -p, --prefix DIR    Ziel; auch PREFIX=DIR
#   --static            ueber packaging/build-static.sh bauen
#   --no-default        PDF-Standard nicht setzen
#   DESTDIR=DIR         nur ablegen, nichts registrieren (Paketbau)
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

# Der Kopf dieser Datei ist die Hilfe.
nutzung() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; }

if [ "$(id -u)" = 0 ]; then mode=system; else mode=user; fi
prefix=${PREFIX:-}
binary= static=0 set_default=1

while [ $# -gt 0 ]; do
    case $1 in
        --user)       mode=user ;;
        --system)     mode=system ;;
        -p | --prefix) [ $# -ge 2 ] || { echo "error: -p braucht einen Wert" >&2; exit 2; }
                       prefix=$2; shift ;;
        --static)     static=1 ;;
        --no-default) set_default=0 ;;
        -h | --help)  nutzung; exit 0 ;;
        -*)           echo "error: unbekannte Option $1" >&2; exit 2 ;;
        *)            binary=$1 ;;
    esac
    shift
done

[ -n "$prefix" ] || { if [ "$mode" = system ]; then prefix=/usr/local; else prefix=$HOME/.local; fi; }

# Schreibrecht vorab pruefen, sonst kommt die Meldung aus `install`.
if [ -z "${DESTDIR:-}" ]; then
    p=$prefix
    while [ ! -e "$p" ]; do p=$(dirname "$p"); done
    [ -w "$p" ] || { echo "error: $prefix ist nicht beschreibbar:" >&2
                     echo "       sudo $0 --prefix $prefix" >&2; exit 1; }
fi

if [ -z "$binary" ]; then
    if [ "$static" = 1 ]; then
        "$root/packaging/build-static.sh"
        arch=$(uname -m | sed 's/amd64/x86_64/; s/arm64/aarch64/')
        binary=$root/target/release/kladde-linux-$arch
    else
        ( cd "$root" && cargo build --release )
        binary=$root/target/release/kladde
    fi
fi
[ -x "$binary" ] || { echo "error: kein Binary: $binary" >&2; exit 1; }

d=${DESTDIR:-} exe=$prefix/bin/kladde share=$prefix/share doc=$share/licenses/kladde
install -d "$d$prefix/bin" "$d$share/applications" \
           "$d$share/icons/hicolor/scalable/apps" "$d$doc"
install -m 755 "$binary"                    "$d$exe"
install -m 644 "$root/packaging/kladde.svg" "$d$share/icons/hicolor/scalable/apps/kladde.svg"
install -m 644 "$root/LICENSE"              "$d$doc/LICENSE"
sed "s|@KLADDE@|$exe|" "$root/packaging/kladde.desktop.in" \
    > "$d$share/applications/kladde.desktop"
chmod 644 "$d$share/applications/kladde.desktop"

if [ -n "${DESTDIR:-}" ]; then
    echo ">> DESTDIR: nur abgelegt, nichts registriert"
else
    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database "$share/applications" || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && gtk-update-icon-cache -q -t -f "$share/icons/hicolor" 2>/dev/null || true

    pdf_hinweis() { echo ">> PDF-Standard nicht gesetzt, als Benutzer:" >&2
                    echo "     xdg-mime default kladde.desktop application/pdf" >&2; }
    if [ "$set_default" = 1 ] && command -v xdg-mime >/dev/null 2>&1; then
        # Als root nicht root's Standard verbiegen, sondern den des Aufrufers.
        if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ] \
           && command -v runuser >/dev/null 2>&1; then
            runuser -u "$SUDO_USER" -- env HOME="$(eval echo "~$SUDO_USER")" \
                xdg-mime default kladde.desktop application/pdf \
                && echo ">> PDF-Standard fuer $SUDO_USER gesetzt" || pdf_hinweis
        elif [ "$(id -u)" != 0 ] && xdg-mime default kladde.desktop application/pdf; then
            echo ">> PDF-Standard fuer $(id -un) gesetzt"
        else
            pdf_hinweis
        fi
    fi
fi

echo
echo "installiert nach $prefix:"
echo "  $exe"
echo "  $share/applications/kladde.desktop"
echo "  $share/icons/hicolor/scalable/apps/kladde.svg"
echo "  $doc/LICENSE"
