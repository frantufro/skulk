#!/bin/sh
set -e

REPO="frantufro/skulk"
BIN_NAME="skulk"

# Detect OS
case "$(uname -s)" in
  Linux)  OS="unknown-linux-gnu" ;;
  Darwin) OS="apple-darwin" ;;
  *)
    echo "Unsupported OS: $(uname -s)" >&2
    exit 1
    ;;
esac

# Detect architecture
case "$(uname -m)" in
  x86_64)
    if [ "$OS" = "apple-darwin" ]; then
      echo "x86_64 macOS is not supported. Use an Apple Silicon Mac or build from source." >&2
      exit 1
    fi
    ARCH="x86_64"
    ;;
  arm64|aarch64)   ARCH="aarch64" ;;
  *)
    echo "Unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

TARGET="${ARCH}-${OS}"
URL="https://github.com/${REPO}/releases/latest/download/${BIN_NAME}-${TARGET}.tar.gz"

# Choose install directory
if [ "$(id -u)" = "0" ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="${HOME}/.local/bin"
  mkdir -p "$INSTALL_DIR"
fi

echo "Downloading skulk for ${TARGET}..."
curl --proto '=https' --tlsv1.2 -fsSL "$URL" | tar -xz -C "$INSTALL_DIR" "$BIN_NAME"
chmod +x "${INSTALL_DIR}/${BIN_NAME}"

echo "Installed ${BIN_NAME} to ${INSTALL_DIR}/${BIN_NAME}"

# Warn if install dir is not in PATH
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "Note: ${INSTALL_DIR} is not in your PATH."
    echo "Add the following to your shell config (~/.bashrc, ~/.zshrc, etc.):"
    echo ""
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    ;;
esac
