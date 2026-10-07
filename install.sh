#!/bin/sh
#
# kladde — install the latest release binary plus desktop integration
#
# Downloads the newest kladde release for this machine's OS and CPU
# architecture, installs it to ~/.local/bin, makes it available on your
# PATH (persistently via ~/.<shell>rc when needed), and installs the
# icon and the .desktop file and registers kladde as the default PDF
# handler.
#
# Written in POSIX sh so it runs on minimal systems without bash.
#
set -eu

APP="kladde"
OWNER="aronym5"
REPO="kladde"
SRC_BRANCH="main"
BIN_DIR="${KLADDE_BIN_DIR:-$HOME/.local/bin}"
SHELLRC="${KLADDE_RC:-}"

# --- Pick a suitable shell rc file for persistent PATH -----------------
if [ -z "$SHELLRC" ]; then
  if [ -n "${ZSH_VERSION:-}" ]; then
    SHELLRC="$HOME/.zshrc"
  elif [ -n "${BASH_VERSION:-}" ]; then
    SHELLRC="$HOME/.bashrc"
  else
    SHELLRC="$HOME/.profile"
  fi
fi

# --- Detect OS and CPU architecture -------------------------------------
# Assets are built per architecture as static (musl) PIE binaries: one
# file runs on every Linux of that architecture, but a file of another
# architecture cannot be executed at all (the kernel refuses it).
OS="$(uname -s)"
case "$OS" in
  Linux) ;;
  *)
    echo "==> ERROR: unsupported OS '$OS' (only Linux is supported)." >&2
    exit 1
    ;;
esac

MACH="$(uname -m)"
case "$MACH" in
  x86_64|amd64)   ARCH="x86_64"  ;;
  aarch64|arm64)  ARCH="aarch64" ;;
  *)
    echo "==> ERROR: architecture '$MACH' is not supported." >&2
    echo "    Supported: x86_64 (amd64), aarch64 (arm64)." >&2
    exit 1
    ;;
esac

ASSET="${APP}-linux-${ARCH}"

# --- Resolve the latest release asset URL -----------------------------
# GitHub's /releases/latest redirects to the newest tagged release.
URL="https://github.com/${OWNER}/${REPO}/releases/latest/download/${ASSET}"

echo "==> Installing ${APP} (${OS}/${ARCH}) to ${BIN_DIR}"

mkdir -p "${BIN_DIR}"

echo "==> Downloading latest release: ${URL}"

# Install atomically: in-place writes to a *running* binary fail with
# ETXTBSY ("Text file busy"), so download to a temp file in the same
# directory and rename it over the target. This works even while kladde
# is currently running; the old version stays in memory for the running
# instance until it is restarted.
TMP="${BIN_DIR}/.${APP}.$$"
if ! curl -fsSL "${URL}" -o "${TMP}"; then
  rm -f "${TMP}"
  echo "==> ERROR: download failed; existing ${BIN_DIR}/${APP} was left untouched." >&2
  exit 1
fi

chmod +x "${TMP}"

# Sanity probe: kladde --help prints a usage text and exits 0 before
# touching PDF files or Wayland. On the right architecture the static
# binary runs; on a mismatched one the kernel refuses it ("Exec format
# error"), so a successful run also proves the asset matches this CPU.
if ! "${TMP}" --help >/dev/null 2>&1; then
  rm -f "${TMP}"
  echo "==> ERROR: downloaded file is not a runnable ${OS}/${ARCH} ${APP} binary." >&2
  echo "    Expected asset: ${ASSET}" >&2
  exit 1
fi

if [ -d "${BIN_DIR}/${APP}" ]; then
  rm -f "${TMP}"
  echo "==> ERROR: ${BIN_DIR}/${APP} is a directory, not a file." >&2
  exit 1
fi

mv -f "${TMP}" "${BIN_DIR}/${APP}"

if [ ! -x "${BIN_DIR}/${APP}" ]; then
  echo "==> ERROR: downloaded file is not a runnable binary." >&2
  exit 1
fi

echo "==> Installed ${BIN_DIR}/${APP}"

# --- Ensure the binary is on PATH ------------------------------------
if command -v "${APP}" >/dev/null 2>&1; then
  echo "==> ${APP} is already on your PATH."
else
  case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *)
      if [ -f "${SHELLRC}" ] || touch "${SHELLRC}" 2>/dev/null; then
        # Avoid inserting duplicate entries on re-runs.
        if ! grep -Fs "export PATH=\"${BIN_DIR}:\$PATH\"" "${SHELLRC}" >/dev/null 2>&1; then
          {
            echo ""
            echo "# added by kladde installer"
            echo "export PATH=\"${BIN_DIR}:\$PATH\""
          } >> "${SHELLRC}"
        fi
      fi
      export PATH="${BIN_DIR}:${PATH}"
      echo "==> Added ${BIN_DIR} to PATH (persisted in ${SHELLRC})."
      ;;
  esac
fi

# --- Desktop integration (taken over from packaging/install-user.sh) --
# Icon and .desktop template come from the code repository's default
# branch (SRC_BRANCH above), not from the release assets.
daten=${XDG_DATA_HOME:-$HOME/.local/share}
apps_dir="$daten/applications"
icon_dir="$daten/icons/hicolor/scalable/apps"
mkdir -p "$apps_dir" "$icon_dir"

# Fetch one file from the code repo; non-fatal (binary stays installed).
fetch_repo() { # $1 = repo-relative path, $2 = destination
  if ! curl -fsSL "https://raw.githubusercontent.com/${OWNER}/${REPO}/${SRC_BRANCH}/$1" -o "$2"; then
    rm -f "$2"
    echo "==> WARNING: could not fetch $1 from ${OWNER}/${REPO}@${SRC_BRANCH}; icon/desktop skipped." >&2
    return 1
  fi
}

echo "==> Installing icon and desktop entry (from ${OWNER}/${REPO}@${SRC_BRANCH})"

icon_tmp="$icon_dir/.${APP}.svg.$$"
if fetch_repo "packaging/kladde.svg" "$icon_tmp"; then
  install -m 644 "$icon_tmp" "$icon_dir/kladde.svg"
  rm -f "$icon_tmp"
fi

desk_in_tmp="$apps_dir/.${APP}.desktop.in.$$"
desk_tmp="$apps_dir/.${APP}.desktop.$$"
if fetch_repo "packaging/kladde.desktop.in" "$desk_in_tmp"; then
  # Replace the @KLADDE@ placeholder with the absolute binary path.
  sed "s|@KLADDE@|${BIN_DIR}/${APP}|" "$desk_in_tmp" > "$desk_tmp"
  rm -f "$desk_in_tmp"
  install -m 644 "$desk_tmp" "$apps_dir/kladde.desktop"
  rm -f "$desk_tmp"
fi

# Default handler: the desktop entry alone only says "I can open PDFs".
if command -v xdg-mime >/dev/null 2>&1; then
  xdg-mime default kladde.desktop application/pdf
  echo "==> kladde set as default handler for application/pdf."
else
  echo "==> xdg-mime not found - please set the default by hand:" >&2
  echo "    [Default Applications]" >&2
  echo "    application/pdf=kladde.desktop" >&2
  echo "    >> \${HOME}/.config/mimeapps.list" >&2
fi
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$apps_dir" || true

echo
echo "Done! Run '${APP}' to start (restart your shell if '${APP}' is not found)."
echo "Check: xdg-mime query default application/pdf"
echo "Note: an already running ${APP} keeps using the previous version until you restart it."
