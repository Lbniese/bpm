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
#   BPM_VERSION      release version to download          (default: latest, else latest-redirect)
#   BPM_INSTALL_DIR  binary install directory             (default: /usr/local/bin)
set -eu

BPM_REPO="${BPM_REPO:-https://github.com/Lbniese/bpm}"
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

# Resolve the release version to download.
# - If BPM_VERSION is set and non-empty, use it exactly (after validation).
# - Otherwise, try the GitHub API for the latest release tag.
# - If the API is unreachable, set BPM_VERSION to empty and use the
#   latest-asset redirect URL in try_prebuilt.
resolve_version() {
    if [ -n "${BPM_VERSION:-}" ]; then
        # Validate: reject values starting with '-' (option injection) or
        # containing shell separators.
        case "$BPM_VERSION" in
            -*|*/*|*\ *)
                die "Invalid BPM_VERSION: $BPM_VERSION"
                ;;
        esac
        return 0
    fi
    # Try the GitHub API for the latest release tag.
    BPM_VERSION=$(curl -fsSL -H "User-Agent: bpm-installer" "https://api.github.com/repos/${BPM_REPO#https://}/releases/latest" 2>/dev/null \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name":[[:space:]]*"v?([^"]+)".*/\1/') || true
    if [ -z "$BPM_VERSION" ]; then
        print_info "Could not reach the GitHub API for the latest release; using latest release redirect."
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

# Return 0 iff the downloaded binary actually supports npm-style package-name
# resolution (the `fetch` command exposes a `--registry` flag). Pre-built
# release assets that predate the registry feature lack this flag and would
# fail on `bpm fetch lodash` with `RelativeUrlWithoutBase`; we detect that and
# fall back to a source build instead of shipping a broken binary.
#
# $1 = path to the candidate binary.
# Reject binaries that lack the npm-name resolution (`--registry` on `fetch`)
# or the single-package bin install (`bin directory` on `install`). Either
# gap means the asset is a stale pre-feature build that would fail real use.
verify_binary() {
    bin=$1
    [ -x "$bin" ] || return 1
    "$bin" fetch --help 2>&1 | grep -q -- '--registry' && \
        "$bin" install --help 2>&1 | grep -q -- 'bin directory'
}

# Verify a release asset before extraction or execution.
#
# Releases publish a detached signature over a sorted SHA256SUMS manifest.
# The installer verifies the signature with a pinned public key, selects the
# exact platform tarball line, verifies the archive hash, inspects the archive
# shape, and only then extracts and executes. Any failure (missing verifier,
# missing/invalid signature, bad checksum, unsafe archive) returns nonzero so
# the caller falls back to a source build. Checksums alone are NOT trusted —
# the detached signature is mandatory for prebuilt use.
#
# The production key lives in `.github/release-signing-public.pem` in the
# repository and is copied into this installer to keep verification fully
# hermetic. The `_BPM_TEST_PUBKEY_FILE` override is HERMETIC-TEST ONLY and is
# not used by normal installs.
release_signing_pubkey() {
    cat <<'BPM_RELEASE_PUBKEY'
-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEhNDYaModKhZ5vrJ9U5zWncqiusE4
Ue2hXf/9nfdY+bb+jjb5sk1dR9626QFTSYS0KIrhKjq0zUXCYsbM8kzO7A==
-----END PUBLIC KEY-----
BPM_RELEASE_PUBKEY
}

# Write the pinned public key to `$1`. Returns nonzero when no usable key
# can be loaded.
materialize_pubkey() {
    if [ -n "${_BPM_TEST_PUBKEY_FILE:-}" ]; then
        cat "$_BPM_TEST_PUBKEY_FILE" >"$1" 2>/dev/null || return 1
        return 0
    fi
    release_signing_pubkey >"$1"
    # Require a PEM header before using the embedded key.
    grep -q -- 'BEGIN PUBLIC KEY' "$1" 2>/dev/null
}

# Download `$1` into file `$2`, failing closed on any HTTP error.
download_asset() {
    curl -fsSL -o "$2" "$1" 2>/dev/null
}

# Verify the detached signature `$2` over manifest `$1` with public key `$3`.
# Returns nonzero on any verification failure.
verify_signature() {
    openssl dgst -sha256 -verify "$3" -signature "$2" "$1" >/dev/null 2>&1
}

# Print the SHA-256 (lowercase hex) of file `$1`.
sha256_file() {
    # openssl dgst prints `<path>); form on some OpenSSL builds; take the last
    # whitespace-separated token and force lowercase.
    openssl dgst -sha256 "$1" 2>/dev/null | awk '{print $NF}' | tr '[:upper:]' '[:lower:]'
}

# Print the recorded hash for exactly one basename `$2` in manifest `$1`, or
# empty if the entry is missing, duplicated, malformed, or carries a path
# component. Rejects anything that is not `<64 hex><two spaces><basename>`.
select_checksum() {
    manifest=$1
    want=$2
    found=""
    while IFS= read -r line; do
        # Strip the trailing CR some CDNs add.
        line=${line%"$(printf '\r')"}
        hash=${line%% *}
        rest=${line#*  }
        [ "$rest" != "$line" ] || continue   # require exactly two spaces
        case "$hash" in
            *[!0-9a-f]*) continue ;;
        esac
        [ ${#hash} -eq 64 ] || continue
        [ "$rest" = "$want" ] || continue
        if [ -n "$found" ]; then
            # Duplicate entry for the same basename: reject.
            return 1
        fi
        found=$hash
    done <"$manifest"
    [ -n "$found" ] || return 1
    printf '%s' "$found"
}

# Inspect archive `$1` and require it to contain exactly one regular file
# named `bpm` — no absolute paths, no `..`, no links/devices, and no extras.
# The archive is never touched before this check passes.
inspect_archive() {
    tarball=$1
    entries=$(tar -tf "$tarball" 2>/dev/null) || return 1
    [ -n "$entries" ] || return 1
    count=0
    while IFS= read -r entry; do
        [ -n "$entry" ] || continue
        [ "$entry" = "bpm" ] || return 1

        # `tar -tvf` prints metadata with a one-character type flag; reject
        # anything that is not a regular file before extraction.
        meta=$(tar -tvf "$tarball" "$entry" 2>/dev/null)
        [ -n "$meta" ] || return 1
        type_char=${meta%${meta#?}}
        [ "$type_char" = "-" ] || return 1
        count=$((count + 1))
    done <<EOF
$entries
EOF
    [ "$count" -eq 1 ] || return 1
}

try_prebuilt() {
    # $1 = platform target
    platform=$1
    tmpdir=$(mktemp -d)

    # Resolve one immutable release namespace for all three assets so exact-
    # version and latest-redirect modes never mix versions.
    if [ -n "${BPM_VERSION:-}" ]; then
        base_url="$BPM_REPO/releases/download/v$BPM_VERSION"
        print_info "Checking for pre-built binary (v$BPM_VERSION)..."
    else
        base_url="$BPM_REPO/releases/latest/download"
        print_info "Checking for latest pre-built binary..."
    fi

    tarball_name="bpm-$platform.tar.gz"
    tarball="$tmpdir/$tarball_name"
    manifest="$tmpdir/SHA256SUMS"
    sig="$tmpdir/SHA256SUMS.sig"
    pubkey="$tmpdir/release-signing-public.pem"

    # Verification requires openssl; without it, fall back to source.
    command -v openssl >/dev/null 2>&1 || {
        print_info "openssl unavailable; falling back to source build."
        rm -rf "$tmpdir"
        return 1
    }

    # No production signing key configured: stay inert, fall back to source.
    materialize_pubkey "$pubkey" || {
        print_info "No release signing key configured; falling back to source build."
        rm -rf "$tmpdir"
        return 1
    }

    # Download the tarball, checksum manifest, and detached signature as
    # distinct files (never pipe archive bytes into tar).
    download_asset "$base_url/$tarball_name" "$tarball" || { rm -rf "$tmpdir"; return 1; }
    download_asset "$base_url/SHA256SUMS" "$manifest" || { rm -rf "$tmpdir"; return 1; }
    download_asset "$base_url/SHA256SUMS.sig" "$sig" || { rm -rf "$tmpdir"; return 1; }

    # 1. Verify the detached signature over the manifest BEFORE trusting any
    #    checksum line.
    verify_signature "$manifest" "$sig" "$pubkey" || {
        print_info "Release signature invalid; falling back to source build."
        rm -rf "$tmpdir"
        return 1
    }
    print_info "Release signature verified."

    # 2. Select exactly this platform's line and verify the archive hash.
    expected=$(select_checksum "$manifest" "$tarball_name") || {
        print_info "Release manifest missing/malformed entry; falling back to source build."
        rm -rf "$tmpdir"
        return 1
    }
    actual=$(sha256_file "$tarball")
    [ -n "$actual" ] && [ "$actual" = "$expected" ] || {
        print_info "Release archive checksum mismatch; falling back to source build."
        rm -rf "$tmpdir"
        return 1
    }
    print_info "Release checksum verified."

    # 3. Inspect the archive shape before extracting.
    inspect_archive "$tarball" || {
        print_info "Release archive shape unsafe; falling back to source build."
        rm -rf "$tmpdir"
        return 1
    }
    print_info "Release archive shape verified."

    # Only after signature + checksum + shape pass do we extract and execute.
    tar -xzf "$tarball" -C "$tmpdir" 2>/dev/null || { rm -rf "$tmpdir"; return 1; }
    if [ -f "$tmpdir/bpm" ] && verify_binary "$tmpdir/bpm"; then
        print_info "Installing ${BPM_VERSION:-latest} release binary..."
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

    # Prefer a local checkout: if we are invoked from inside the bpm repo,
    # build that (already on disk, no clone, uses the current code).
    if [ -f Cargo.toml ] && grep -q '^name = "bpm"' Cargo.toml 2>/dev/null; then
        print_info "Using local checkout at $(pwd)..."
        srcdir=$(pwd)
    else
        tmpdir=$(mktemp -d)
        print_info "Cloning $BPM_REPO..."
        git clone --depth 1 "$BPM_REPO" "$tmpdir" 2>/dev/null || {
            print_err "Failed to clone repository."
            rm -rf "$tmpdir"
            exit 1
        }
        srcdir=$tmpdir
    fi

    print_info "Building (this may take a few minutes)..."
    (cd "$srcdir" && cargo build --release) || {
        print_err "Build failed. See output above."
        [ -n "${tmpdir:-}" ] && rm -rf "$tmpdir"
        exit 1
    }

    install_binary "$srcdir/target/release/bpm" "$BPM_INSTALL_DIR/bpm" || {
        print_err "Installation failed."
        suggest_command
        [ -n "${tmpdir:-}" ] && rm -rf "$tmpdir"
        exit 1
    }

    [ -n "${tmpdir:-}" ] && rm -rf "$tmpdir"
    print_ok "bpm installed to $BPM_INSTALL_DIR/bpm"
}

main() {
    print_bold "Bloom Package Manager (bpm) installer"
    echo

    resolve_version

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
