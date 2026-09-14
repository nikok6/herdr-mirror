#!/usr/bin/env bash
# Fetch the prebuilt herdr-mirror binary for this platform from GitHub
# Releases, verified against SHA256SUMS. Run by the herdr plugin [[build]]
# step with cwd = plugin root. No cargo fallback: dev installs (herdr plugin
# link) build with `cargo build --release` themselves.
set -euo pipefail

cd "$(dirname "$0")/.."
DEST="target/release/herdr-mirror"

fail() {
  echo "herdr-mirror fetch failed: $1" >&2
  echo "to build from source instead: cargo build --release" >&2
  exit 1
}

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || fail "cannot read version from Cargo.toml"

# owner/repo from the git remote (plugins are installed by git clone)
SLUG="$(git config --get remote.origin.url 2>/dev/null |
  sed -n 's#.*[:/]\([^/]*/[^/]*\)\.git$#\1#p; s#.*[:/]\([^/]*/[^/]*\)$#\1#p' | head -1)"
[ -n "$SLUG" ] || fail "cannot derive owner/repo from the git remote"

case "$(uname -s)" in
  Darwin) OS="darwin" ;;
  Linux) OS="linux" ;;
  MINGW* | MSYS* | CYGWIN*) OS="windows" ;;
  *) fail "unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) ARCH="aarch64" ;;
  x86_64 | amd64) ARCH="x86_64" ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac
ASSET="herdr-mirror-${OS}-${ARCH}"
[ "$OS" = "windows" ] && ASSET="${ASSET}.exe"
if [ "$OS" = "windows" ]; then
  DEST="target/release/herdr-mirror.exe"
fi
BASE="https://github.com/${SLUG}/releases/download/v${VERSION}"

# SHA-256 verifier. coreutils `sha256sum` on Linux and recent macOS; `shasum`
# (perl Digest::SHA) on older macOS and Debian-family. Neither is universal:
# Arch's perl ships no /usr/bin/shasum, so requiring it made install fail there.
if command -v sha256sum >/dev/null 2>&1; then
  sha256_check() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_check() { shasum -a 256 -c -; }
else
  fail "no SHA-256 tool found: install coreutils (sha256sum) or perl (shasum)"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "fetching ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/${ASSET}" "${BASE}/${ASSET}" || fail "download failed: ${BASE}/${ASSET}"
curl -fsSL --retry 2 -o "${TMP}/SHA256SUMS" "${BASE}/SHA256SUMS" || fail "download failed: ${BASE}/SHA256SUMS"

# Look the hash up first, so a release missing this asset can't be reported as
# a corrupt download: under `pipefail` a no-match grep fails the whole pipeline
# and would otherwise land on the mismatch message below.
EXPECTED="$(grep " ${ASSET}\$" "${TMP}/SHA256SUMS")" ||
  fail "${ASSET} is not listed in SHA256SUMS — the v${VERSION} release looks incomplete"
(cd "$TMP" && printf '%s\n' "$EXPECTED" | sha256_check) ||
  fail "checksum MISMATCH for ${ASSET} — the download is corrupt or tampered with; do not use it"

mkdir -p "$(dirname "$DEST")"
install -m 755 "${TMP}/${ASSET}" "$DEST"
echo "installed ${ASSET} v${VERSION} at ${DEST}"

# Link the CLI at the stable path the README documents. Keybindings must use
# the absolute ~/.local/bin/herdr-mirror (herdr runs shell bindings through a
# login sh that never reads ~/.zshrc, so PATH can't be trusted there), and
# `herdr-mirror <cmd>` should work from a shell. Refreshed on every update;
# a live file or link we don't manage is left alone.
#
# The link must NOT target pwd: herdr runs this build in
# <plugins>/.tmp-install-*/checkout and only afterwards renames the checkout
# to <plugins>/github/<id>-<hash>, deleting the temp dir — a link to pwd
# dangles the moment the install succeeds. Derive that final path instead
# (<hash> = first 12 hex chars of sha256(<id>), herdr's managed-path scheme;
# the link briefly dangles until herdr's rename lands, which is fine). Run by
# hand in a dev checkout (no .tmp-* ancestor under a plugins dir) it links
# the checkout itself. `herdr-mirror status` reports link health, and the
# daemon toasts if it ever goes stale, so a future herdr layout change is
# loud, not a silent dead key.
sha256_hex() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | cut -d' ' -f1
  else
    shasum -a 256 | cut -d' ' -f1
  fi
}
final_bin_path() {
  dir="$(pwd)"
  while [ "$dir" != "/" ]; do
    case "$(basename "$dir")" in
      .tmp-*)
        if [ "$(basename "$(dirname "$dir")")" = "plugins" ]; then
          ID="$(sed -n 's/^id = "\(.*\)"/\1/p' herdr-plugin.toml | head -1)"
          HASH="$(printf %s "$ID" | sha256_hex | cut -c1-12)"
          echo "$(dirname "$dir")/github/${ID}-${HASH}/${DEST}"
          return 0
        fi
        ;;
    esac
    dir="$(dirname "$dir")"
  done
  echo "$(pwd)/${DEST}"
}
if [ "$OS" = "windows" ]; then
  LINK="${HOME}/.local/bin/herdr-mirror.exe"
else
  LINK="${HOME}/.local/bin/herdr-mirror"
fi
TARGET="$(final_bin_path)"

# A link is foreign when it's live, isn't our target, and points outside a
# herdr plugins dir — e.g. a dev checkout the user linked deliberately.
# A dangling link is never foreign: whatever it was, it's broken, and
# replacing it is what makes the documented keybindings work again.
is_foreign_link() {
  [ -L "$LINK" ] && [ -e "$LINK" ] || return 1
  CUR="$(readlink "$LINK")"
  [ "$CUR" = "$TARGET" ] && return 1
  case "$CUR" in
    */herdr/plugins/* | */herdr-dev/plugins/*) return 1 ;; # an older install of ours
  esac
  return 0
}

if [ "$OS" = "windows" ]; then
  # Windows symlinks need Developer Mode/admin and MSYS `ln -s` may silently
  # write a plain copy instead; either way a symlink check is unreliable, so
  # the stable CLI path is a refreshed copy on every install.
  if mkdir -p "${HOME}/.local/bin" && rm -f "$LINK" && cp "$TARGET" "$LINK"; then
    echo "copied ${LINK}"
  else
    echo "note: could not install ${LINK}; run the plugin's Mirror: start action to repair it"
  fi
elif [ -e "$LINK" ] && [ ! -L "$LINK" ]; then
  echo "note: ${LINK} exists and is not a symlink; left untouched (README keybindings expect herdr-mirror there)"
elif is_foreign_link; then
  echo "note: ${LINK} -> ${CUR} left untouched; not managed by this install"
else
  # linking is best-effort: the binary is already installed, so a failure here
  # (say, an unwritable ~/.local/bin) must not fail the whole plugin install
  if mkdir -p "${HOME}/.local/bin" && rm -f "$LINK" && ln -s "$TARGET" "$LINK"; then
    echo "linked ${LINK} -> ${TARGET}"
  else
    echo "note: could not link ${LINK}; run the plugin's Mirror: start action to repair it"
  fi
fi
