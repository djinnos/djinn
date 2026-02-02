#!/bin/sh
# Djinn CLI Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/djinnos/djinn/main/install.sh | bash
#
# Environment variables:
#   DJINN_INSTALL_DIR - Installation directory (default: ~/.local/bin or /usr/local/bin)
#   DJINN_VERSION     - Specific version to install (default: latest)

set -e

REPO="djinnos/djinn"
BINARY_NAME="djinn"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

info() {
    printf "${GREEN}info${NC}: %s\n" "$1"
}

warn() {
    printf "${YELLOW}warn${NC}: %s\n" "$1"
}

error() {
    printf "${RED}error${NC}: %s\n" "$1" >&2
    exit 1
}

# Detect OS
detect_os() {
    case "$(uname -s)" in
        Linux*)     echo "linux" ;;
        Darwin*)    echo "darwin" ;;
        MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
        *)          error "Unsupported operating system: $(uname -s)" ;;
    esac
}

# Detect architecture
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   echo "amd64" ;;
        arm64|aarch64)  echo "arm64" ;;
        *)              error "Unsupported architecture: $(uname -m)" ;;
    esac
}

# Get latest version from GitHub
get_latest_version() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | 
        grep '"tag_name":' | 
        sed -E 's/.*"v([^"]+)".*/\1/'
}

# Determine install directory
get_install_dir() {
    if [ -n "$DJINN_INSTALL_DIR" ]; then
        echo "$DJINN_INSTALL_DIR"
    elif [ -d "$HOME/.local/bin" ]; then
        echo "$HOME/.local/bin"
    elif [ -w "/usr/local/bin" ]; then
        echo "/usr/local/bin"
    else
        echo "$HOME/.local/bin"
    fi
}

main() {
    OS=$(detect_os)
    ARCH=$(detect_arch)
    
    info "Detected OS: $OS, Arch: $ARCH"
    
    # Get version
    if [ -n "$DJINN_VERSION" ]; then
        VERSION="$DJINN_VERSION"
        info "Installing specified version: $VERSION"
    else
        info "Fetching latest version..."
        VERSION=$(get_latest_version)
        if [ -z "$VERSION" ]; then
            error "Failed to fetch latest version. Check your internet connection or try again later."
        fi
        info "Latest version: $VERSION"
    fi
    
    # Determine archive format
    if [ "$OS" = "windows" ]; then
        EXT="zip"
    else
        EXT="tar.gz"
    fi
    
    # Build download URL
    ARCHIVE_NAME="${BINARY_NAME}_${VERSION}_${OS}_${ARCH}.${EXT}"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ARCHIVE_NAME}"
    
    info "Downloading $DOWNLOAD_URL..."
    
    # Create temp directory
    TMP_DIR=$(mktemp -d)
    trap "rm -rf $TMP_DIR" EXIT
    
    # Download archive
    if ! curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/$ARCHIVE_NAME"; then
        error "Failed to download $DOWNLOAD_URL"
    fi
    
    # Extract
    info "Extracting..."
    cd "$TMP_DIR"
    if [ "$EXT" = "zip" ]; then
        unzip -q "$ARCHIVE_NAME"
    else
        tar -xzf "$ARCHIVE_NAME"
    fi
    
    # Find binary
    if [ "$OS" = "windows" ]; then
        BINARY="${BINARY_NAME}.exe"
    else
        BINARY="$BINARY_NAME"
    fi
    
    if [ ! -f "$BINARY" ]; then
        error "Binary not found in archive"
    fi
    
    # Install
    INSTALL_DIR=$(get_install_dir)
    info "Installing to $INSTALL_DIR..."
    
    # Create install dir if needed
    mkdir -p "$INSTALL_DIR"
    
    # Copy binary
    cp "$BINARY" "$INSTALL_DIR/$BINARY"
    chmod +x "$INSTALL_DIR/$BINARY"
    
    info "Successfully installed $BINARY_NAME v$VERSION to $INSTALL_DIR/$BINARY"
    
    # Check if in PATH
    if ! echo "$PATH" | grep -q "$INSTALL_DIR"; then
        warn "$INSTALL_DIR is not in your PATH"
        echo ""
        echo "Add it to your shell config:"
        echo ""
        echo "  # For bash (~/.bashrc):"
        echo "  export PATH=\"\$PATH:$INSTALL_DIR\""
        echo ""
        echo "  # For zsh (~/.zshrc):"
        echo "  export PATH=\"\$PATH:$INSTALL_DIR\""
        echo ""
    fi
    
    # Verify installation
    if command -v djinn >/dev/null 2>&1; then
        echo ""
        info "Run 'djinn' to get started!"
    else
        echo ""
        info "Run '$INSTALL_DIR/djinn' to get started!"
    fi
}

main "$@"
