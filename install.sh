#!/bin/sh
# share installer for Linux and macOS  --  https://github.com/rootagi/share
#
#   curl -fsSL https://raw.githubusercontent.com/rootagi/share/main/install.sh | sh
#
# Optional environment variables:
#   SHARE_VERSION      release tag to install, e.g. v0.1.0   (default: latest)
#   SHARE_INSTALL_DIR  where to put the binary                (default: ~/.local/bin,
#                                                              or /usr/local/bin as root)
#   NO_COLOR=1         disable colours and fancy symbols
set -eu

REPO="rootagi/share"
BIN="share"
TOTAL=8
STEP=0
T0=$(date +%s)

# ---------------------------------------------------------------------------
# Styling: colours only on a real terminal, unicode only on UTF-8 locales.
# ---------------------------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != "dumb" ]; then
  ESC=$(printf '\033')
  BOLD="${ESC}[1m"; DIM="${ESC}[2m"; RST="${ESC}[0m"
  RED="${ESC}[31m"; GRN="${ESC}[32m"; YEL="${ESC}[33m"; BLU="${ESC}[34m"; CYN="${ESC}[36m"; MAG="${ESC}[35m"
  IS_TTY=1
else
  BOLD=""; DIM=""; RST=""; RED=""; GRN=""; YEL=""; BLU=""; CYN=""; MAG=""
  IS_TTY=0
fi

case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
  *[Uu][Tt][Ff]-8*|*[Uu][Tt][Ff]8*) [ -n "${NO_COLOR:-}" ] && UNI=0 || UNI=1 ;;
  *) UNI=0 ;;
esac
if [ "$UNI" = "1" ]; then
  ARROW="▸"; OK="✔"; BAD="✖"; WARN="▲"; DOT="•"; ELL="…"; INFO="ℹ"
  TL="╭"; TR="╮"; BL="╰"; BR="╯"; H="─"; V="│"
else
  ARROW=">"; OK="+"; BAD="x"; WARN="!"; DOT="-"; ELL="..."; INFO="i"
  TL="+"; TR="+"; BL="+"; BR="+"; H="-"; V="|"
fi

HL=""; i=0
while [ "$i" -lt 48 ]; do HL="$HL$H"; i=$((i + 1)); done

# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------
have() { command -v "$1" >/dev/null 2>&1; }

boxrow() { printf '  %s%s%s %s%-46s%s %s%s%s\n' "$CYN" "$V" "$RST" "$2" "$1" "$RST" "$CYN" "$V" "$RST"; }

step() {
  STEP=$((STEP + 1))
  printf '\n%s%s [%s/%s] %s%s\n' "$BLU$BOLD" "$ARROW" "$STEP" "$TOTAL" "$1" "$RST"
}
info() { printf '    %s%-13s%s %s\n' "$DIM" "$1" "$RST" "$2"; }
ok()   { printf '    %s%s%s %s\n' "$GRN" "$OK" "$RST" "$1"; }
warn() { printf '    %s%s %s%s\n' "$YEL" "$WARN" "$1" "$RST"; }
note() { printf '    %s%s%s\n' "$DIM" "$1" "$RST"; }
blank() { printf '\n'; }

# requirement rows for the final summary:  req <ok|warn|info> <label> <text>
req() {
  case "$1" in
    ok)   sym="$GRN$OK$RST" ;;
    warn) sym="$YEL$WARN$RST" ;;
    *)    sym="$MAG$INFO$RST" ;;
  esac
  printf '    %s %s%-18s%s %s\n' "$sym" "$BOLD" "$2" "$RST" "$3"
}

err() {
  printf '\n  %s%s error:%s %s\n' "$RED$BOLD" "$BAD" "$RST" "$1" >&2
  [ -n "${2:-}" ] && printf '    %s%s%s\n' "$DIM" "$2" "$RST" >&2
  printf '\n' >&2
  exit 1
}

human() { # bytes -> "6.4 MB"
  if [ "$1" -ge 1048576 ]; then
    printf '%d.%d MB' $(($1 / 1048576)) $(($1 * 10 / 1048576 % 10))
  else
    printf '%d KB' $(($1 / 1024))
  fi
}
short() { # abbreviate a 64-char hash
  printf '%s%s%s' "$(printf '%s' "$1" | cut -c1-16)" "$ELL" "$(printf '%s' "$1" | cut -c57-64)"
}

# ---------------------------------------------------------------------------
# Downloader (curl preferred, wget as fallback). Missing tools are reported
# in step 1, so the user sees a clear message inside the normal flow.
# ---------------------------------------------------------------------------
DL=""
if have curl; then
  DL="curl"
  fetch() { curl -fsSL "$1"; }
  download() {
    if [ "$IS_TTY" = "1" ]; then curl -fL -# -o "$2" "$1"; else curl -fsSL -o "$2" "$1"; fi
  }
  latest_tag() {
    curl -fsSIL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's|.*/tag/||'
  }
elif have wget; then
  DL="wget"
  fetch() { wget -qO- "$1"; }
  download() {
    if [ "$IS_TTY" = "1" ]; then
      wget -q --show-progress -O "$2" "$1" 2>/dev/null || wget -q -O "$2" "$1"
    else
      wget -q -O "$2" "$1"
    fi
  }
  latest_tag() {
    wget -S --spider --max-redirect=0 "https://github.com/$REPO/releases/latest" 2>&1 \
      | grep -i '^ *location:' | head -n1 | sed 's|.*/tag/||' | tr -d '\r '
  }
fi

# ---------------------------------------------------------------------------
# Banner
# ---------------------------------------------------------------------------
blank
printf '  %s%s%s%s%s\n' "$CYN" "$TL" "$HL" "$TR" "$RST"
boxrow "share  -  LAN file server & live monitor" "$BOLD"
boxrow "installer for Linux & macOS" "$DIM"
printf '  %s%s%s%s%s\n' "$CYN" "$BL" "$HL" "$BR" "$RST"
blank
printf '  %sThis script will:%s\n' "$BOLD" "$RST"
printf '    %s check that your system has what it needs\n' "$DOT"
printf '    %s download the official release from github.com/%s\n' "$DOT" "$REPO"
printf '    %s verify it against the published SHA-256 checksum\n' "$DOT"
printf '    %s copy one file (%s) into a folder of your own\n' "$DOT" "$BIN"
printf '  %sNo sudo, no system files touched, nothing left running.%s\n' "$DIM" "$RST"

# ---------------------------------------------------------------------------
# 1. Check requirements
# ---------------------------------------------------------------------------
step "Checking requirements"
[ -n "$DL" ] || err "Neither curl nor wget was found." "Install one of them (for example: sudo apt install curl) and run this again."
ok "Downloader: $DL"
have tar || err "'tar' was not found." "Install it (for example: sudo apt install tar) and run this again."
ok "Archive tool: tar"
if have sha256sum; then
  ok "Checksum tool: sha256sum"; HAVE_SHA=1
elif have shasum; then
  ok "Checksum tool: shasum"; HAVE_SHA=1
else
  warn "No sha256sum/shasum found - the download cannot be verified"; HAVE_SHA=0
fi
note "Rust is NOT needed here: this installer downloads a prebuilt binary."

# ---------------------------------------------------------------------------
# 2. Detect system
# ---------------------------------------------------------------------------
step "Detecting your system"
os=$(uname -s)
arch=$(uname -m)

case "$arch" in
  x86_64|amd64)  cpu="x86_64";  cpu_name="64-bit Intel/AMD" ;;
  aarch64|arm64) cpu="aarch64"; cpu_name="64-bit ARM" ;;
  *) err "Unsupported CPU architecture '$arch'." "Released builds exist for x86_64 and aarch64. Build from source (needs Rust 1.85+): cargo install --git https://github.com/$REPO" ;;
esac

case "$os" in
  Linux)  target="$cpu-unknown-linux-musl"; os_name="Linux";  kind="static build, runs on any distro" ;;
  Darwin) target="$cpu-apple-darwin";       os_name="macOS";  kind="native macOS build" ;;
  *) err "Unsupported operating system '$os'." "On Windows, use install.ps1 instead." ;;
esac

info "OS" "$os_name"
info "CPU" "$cpu_name ($arch)"
info "Build" "$target"
note "$kind"
info "Downloader" "$DL"

# ---------------------------------------------------------------------------
# 3. Resolve version
# ---------------------------------------------------------------------------
step "Finding the release"
info "Repository" "github.com/$REPO"
tag="${SHARE_VERSION:-}"
if [ -z "$tag" ]; then
  info "Requested" "latest release"
  note "following github.com/$REPO/releases/latest (no API, no rate limit)"
  tag=$(latest_tag) || tag=""
  [ -n "$tag" ] || err "Could not determine the latest release of $REPO." "Check your internet connection, or pin one: SHARE_VERSION=v0.1.0"
else
  info "Requested" "$tag (pinned via SHARE_VERSION)"
fi
case "$tag" in v*) ;; *) tag="v$tag" ;; esac

name="$BIN-$tag-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"
ok "Version $tag"

# ---------------------------------------------------------------------------
# 4. Download
# ---------------------------------------------------------------------------
step "Downloading"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t share-install)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/x"

info "File" "$name"
info "From" "$base/"
info "To" "$tmp (temporary)"
download "$base/$name" "$tmp/$name" \
  || err "Download failed." "Does release $tag exist? See https://github.com/$REPO/releases"
size=$(wc -c < "$tmp/$name" | tr -d ' ')
ok "Downloaded $(human "$size")"

# ---------------------------------------------------------------------------
# 5. Verify checksum
# ---------------------------------------------------------------------------
step "Verifying integrity"
sha_of() {
  if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
  elif have shasum;  then shasum -a 256 "$1" | cut -d' ' -f1
  else return 1; fi
}

VERIFIED=0
info "Checksum file" "$base/SHA256SUMS"
if sums=$(fetch "$base/SHA256SUMS" 2>/dev/null); then
  expected=$(printf '%s\n' "$sums" | grep -F "$name" | grep -oE '[0-9a-fA-F]{64}' | head -n1 || true)
  [ -n "$expected" ] || err "SHA256SUMS has no entry for $name." "The release may be incomplete. Please report it at https://github.com/$REPO/issues"
  if actual=$(sha_of "$tmp/$name"); then
    expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
    info "Expected" "$(short "$expected")"
    info "Computed" "$(short "$actual")"
    [ "$actual" = "$expected" ] || err "Checksum mismatch - the download is corrupted or has been tampered with." "Nothing was installed. Try again; if it keeps failing please report it."
    ok "SHA-256 matches, the file is intact"
    VERIFIED=1
  else
    warn "No sha256sum/shasum on this system, skipping verification"
  fi
else
  warn "Could not fetch SHA256SUMS, skipping verification"
fi

# ---------------------------------------------------------------------------
# 6. Extract
# ---------------------------------------------------------------------------
step "Unpacking"
count=$(tar -tzf "$tmp/$name" | wc -l | tr -d ' ')
info "Archive" "$count file(s)"
tar -tzf "$tmp/$name" | head -n 6 | while IFS= read -r f; do
  printf '      %s%s %s%s\n' "$DIM" "$DOT" "$f" "$RST"
done
[ "$count" -gt 6 ] && note "  $ELL and $((count - 6)) more"
tar -xzf "$tmp/$name" -C "$tmp/x" || err "Could not extract $name."
bin_path=$(find "$tmp/x" -type f -name "$BIN" | head -n1)
[ -n "$bin_path" ] || err "No '$BIN' binary found inside $name." "Please report it at https://github.com/$REPO/issues"
ok "Found the binary: ${bin_path#"$tmp/x/"}"

# ---------------------------------------------------------------------------
# 7. Install
# ---------------------------------------------------------------------------
step "Installing"
if [ -n "${SHARE_INSTALL_DIR:-}" ]; then
  dir="$SHARE_INSTALL_DIR"; why="from SHARE_INSTALL_DIR"
elif [ "$(id -u)" = "0" ]; then
  dir="/usr/local/bin"; why="running as root"
else
  dir="$HOME/.local/bin"; why="your user folder, no sudo needed"
fi
info "Folder" "$dir"
note "$why"

mkdir -p "$dir" 2>/dev/null || err "Cannot create $dir." "Pick a writable folder: SHARE_INSTALL_DIR=\$HOME/bin  (or run with sudo)"
[ -w "$dir" ] || err "$dir is not writable." "Pick a writable folder: SHARE_INSTALL_DIR=\$HOME/bin  (or run with sudo)"

old=""
if [ -x "$dir/$BIN" ]; then
  old=$("$dir/$BIN" --version 2>/dev/null | head -n1 || true)
  [ -n "$old" ] && warn "Replacing existing install ($old)" || warn "Replacing existing file at $dir/$BIN"
fi

cp "$bin_path" "$dir/$BIN"
chmod 755 "$dir/$BIN"
info "Permissions" "755 (executable)"
ok "Copied to $dir/$BIN"

# ---------------------------------------------------------------------------
# 8. Final check + cleanup
# ---------------------------------------------------------------------------
step "Finishing up"
RUNS=0
if ver=$("$dir/$BIN" --version 2>/dev/null | head -n1) && [ -n "$ver" ]; then
  ok "Binary runs: $ver"; RUNS=1
else
  warn "Installed, but '$BIN --version' did not run here (wrong CPU/OS build?)"
fi
rm -rf "$tmp"
ok "Removed temporary files"

on_path=1
case ":$PATH:" in *":$dir:"*) ;; *) on_path=0 ;; esac
if [ "$on_path" = "1" ]; then
  ok "$dir is on your PATH"
else
  warn "$dir is not on your PATH yet"
  case "$(basename "${SHELL:-sh}")" in
    zsh)  rc="~/.zshrc";  line="export PATH=\"$dir:\$PATH\"" ;;
    bash) rc="~/.bashrc"; line="export PATH=\"$dir:\$PATH\"" ;;
    fish) rc="";          line="fish_add_path $dir" ;;
    *)    rc="-";         line="export PATH=\"$dir:\$PATH\"" ;;
  esac
  if [ "$rc" = "-" ]; then
    note "Add this line to your shell profile:"
    printf '      %s%s%s\n' "$BOLD" "$line" "$RST"
  elif [ -n "$rc" ]; then
    note "To fix it, run this:"
    printf '      %s$ echo '"'"'%s'"'"' >> %s && . %s%s\n' "$BOLD" "$line" "$rc" "$rc" "$RST"
  else
    note "To fix it, run this:"
    printf '      %s$ %s%s\n' "$BOLD" "$line" "$RST"
  fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
T1=$(date +%s)
blank
printf '  %s%s share %s installed%s %sin %ss%s\n' "$GRN$BOLD" "$OK" "$tag" "$RST" "$DIM" "$((T1 - T0))" "$RST"

blank
printf '  %sGet started%s\n' "$BOLD" "$RST"
printf '    %s$ share ~/Downloads%s            %sserve a folder on your network%s\n' "$CYN" "$RST" "$DIM" "$RST"
printf '    %s$ share ./file.iso --qr%s        %sshare one file with a QR code%s\n' "$CYN" "$RST" "$DIM" "$RST"
printf '    %s$ share --help%s                 %sall options%s\n' "$CYN" "$RST" "$DIM" "$RST"

blank
printf '  %sRequirements%s\n' "$BOLD" "$RST"
req ok "Rust / Cargo" "not needed - you installed a prebuilt binary"
if have rustc; then
  note "  (Rust $(rustc --version 2>/dev/null | cut -d' ' -f2) is on this machine, but share does not use it)"
fi
if [ "$os" = "Linux" ]; then
  req ok "Runtime libraries" "none - static build (no OpenSSL, Node.js or Python)"
else
  req ok "Runtime libraries" "none - no OpenSSL, Node.js or Python needed"
fi
if [ "$VERIFIED" = "1" ]; then
  req ok "Integrity" "SHA-256 checksum verified"
else
  req warn "Integrity" "checksum was NOT verified - consider re-running"
fi
if [ "$RUNS" != "1" ]; then
  req warn "Binary check" "could not run '$BIN --version' - see the warning above"
fi
req info "Build from source" "only if you want to: Rust 1.85+ (https://rustup.rs), then"
printf '    %s                       cargo install --git https://github.com/%s%s\n' "$DIM" "$REPO" "$RST"
req info "Homebrew" "brew install rootagi/tap/share"

blank
printf '  %sGood to know%s\n' "$BOLD" "$RST"
printf '    %s First HTTPS visit shows a self-signed certificate warning. Choose%s\n' "$DOT" "$RST"
printf '      %sAdvanced > Proceed, or use --http if you do not need encryption.%s\n' "$DIM" "$RST"
if [ "$os" = "Darwin" ]; then
  printf '    %s macOS may ask to allow incoming connections - click Allow.%s\n' "$DOT" "$RST"
else
  printf '    %s If other devices cannot connect, allow the port (default 8080/tcp)%s\n' "$DOT" "$RST"
  printf '      %sin your firewall (ufw, firewalld or nftables).%s\n' "$DIM" "$RST"
fi

blank
printf '  %sDocs & issues: https://github.com/%s%s\n' "$DIM" "$REPO" "$RST"
blank
