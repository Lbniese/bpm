#!/bin/sh
# BPM installer. Recommended usage:
#   curl -fsSL https://raw.githubusercontent.com/Lbniese/bpm/main/install.sh | sh
#
# The script builds bpm as the CURRENT user (where the Rust toolchain lives)
# and only requests sudo for the final copy into the install directory, and
# only if that directory is not writable.
#
# Overrides (env):
#   BPM_REPO         git repo to clone / release source   (default: upstream)
#   BPM_VERSION      release version to download          (default: 0.1.0)
#   BPM_INSTALL_DIR  binary install directory             (default: /usr/local/bin)
set -eu

BPM_REPO="${BPM_REPO:-https://github.com/Lbniese/bpm}"
BPM_VERSION="${BPM_VERSION:-0.1.0}"
BPM_INSTALL_DIR="${BPM_INSTALL_DIR:-/usr/local/bin}"

INSTALL_URL="https://raw.githubusercontent.com/Lbniese/bpm/main/install.sh"

print_bold() { printf "\033[1m%s\033[0m\n" "$*"; }
print_ok()   { printf "  \033[32m✓\033[0m %s\n" "$*"; }
print_info() { printf "  \033[34mi\033[0m %s\n" "$*"; }
print_err()  { printf "  \033[31m✗\033[0m %s\n" "$*"; }

die() {
    print_err "$1"
    exit 1
}

is_root() { [ "$(id -u)" = "0" ]; }

# True if we can write files into BPM_INSTALL_DIR without elevation.
dir_writable() {
    [ -d "$BPM_INSTALL_DIR" ] || return 1
    [ -w "$BPM_INSTALL_DIR" ]
}

# Print the exact command the user should run if the automated sudo copy fails.
suggest_command() {
    if [ -n "${SUDO_USER:-}" ] && is_root; then
        cat <<EOF
  It looks like you ran this with sudo. Rust (cargo) is installed for the
  user '$SUDO_USER', not for root. Run the installer WITHOUT sudo — it will
  request sudo only for the final copy step:

      curl -fsSL $INSTALL_URL | sh

  Or install to a directory you own, avoiding sudo entirely:

      BPM_INSTALL_DIR=\$HOME/.local/bin curl -fsSL $INSTALL_URL | sh
EOF
    else
        printf "  Re-run and allow the sudo prompt, or install without sudo:\n"
        printf "      BPM_INSTALL_DIR=\$HOME/.local/bin curl -fsSL %s | sh\n" "$INSTALL_URL"
    fi
}

detect_platform() {
    arch=$(uname -m)
    os=$(uname -s | tr '[:upper:]' '[:lower:]')
    case "$arch/$os" in
        aarch64/darwin) echo "aarch64-apple-darwin" ;;
        arm64/darwin)   echo "aarch64-apple-darwin" ;;
        x86_64/darwin)  echo "x86_64-apple-darwin" ;;
        x86_64/linux)   echo "x86_64-unknown-linux-gnu" ;;
        aarch64/linux)  echo "aarch64-unknown-linux-gnu" ;;
        *)              echo "" ;;
    esac
}

# Copy $1 -> $2, elevating with sudo only when needed.
install_binary() {
    src=$1
    dst=$2
    if dir_writable || is_root; then
        install -m 755 "$src" "$dst"
        return $?
    fi
    print_info "sudo is required to write to $BPM_INSTALL_DIR"
    sudo install -m 755 "$src" "$dst"
}

try_prebuilt() {
    # $1 = platform target
    platform=$1
    release_url="$BPM_REPO/releases/download/v$BPM_VERSION/bpm-$platform.tar.gz"
    print_info "Checking for pre-built binary (v$BPM_VERSION)..."
    tmpdir=$(mktemp -d)
    if curl -fsSL "$release_url" 2>/dev/null | tar xz -C "$tmpdir" 2>/dev/null; then
        if [ -f "$tmpdir/bpm" ]; then
            print_info "Installing bpm v$BPM_VERSION..."
            install_binary "$tmpdir/bpm" "$BPM_INSTALL_DIR/bpm" || {
                print_err "Installation failed."
                suggest_command
                rm -rf "$tmpdir"
                exit 1
            }
            rm -rf "$tmpdir"
            print_ok "bpm installed to $BPM_INSTALL_DIR/bpm"
            return 0
        fi
    fi
    rm -rf "$tmpdir"
    return 1
}

ensure_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        return 0
    fi
    print_err "Rust toolchain (cargo) not found for the current user."
    if is_root && [ -n "${SUDO_USER:-}" ]; then
        suggest_command
        exit 1
    fi
    print_info "Install Rust: https://rustup.rs  — then re-run this script."
    exit 1
}

build_from_source() {
    print_info "Building from source..."
    ensure_cargo

    tmpdir=$(mktemp -d)
    print_info "Cloning $BPM_REPO..."
    git clone --depth 1 "$BPM_REPO" "$tmpdir" 2>/dev/null || {
        print_err "Failed to clone repository."
        rm -rf "$tmpdir"
        exit 1
    }

    print_info "Building (this may take a few minutes)..."
    (cd "$tmpdir" && cargo build --release) || {
        print_err "Build failed. See output above."
        rm -rf "$tmpdir"
        exit 1
    }

    install_binary "$tmpdir/target/release/bpm" "$BPM_INSTALL_DIR/bpm" || {
        print_err "Installation failed."
        suggest_command
        rm -rf "$tmpdir"
        exit 1
    }

    rm -rf "$tmpdir"
    print_ok "bpm installed to $BPM_INSTALL_DIR/bpm"
}

main() {
    print_bold "Bloom Package Manager (bpm) installer"
    echo

    platform=$(detect_platform)
    if [ -z "$platform" ]; then
        print_info "Platform not recognized: $(uname -s)/$(uname -m) — will try building from source."
    else
        print_ok "Detected: $platform"
        if try_prebuilt "$platform"; then
            return 0
        fi
        print_info "Pre-built binary not available; falling back to source build."
    fi

    build_from_source
}

main "$@"
