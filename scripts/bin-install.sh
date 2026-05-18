#!/usr/bin/env bash
set -euo pipefail

# Determine OS
os=""
case "$(uname -s)" in
    Linux)  os="linux" ;;
    Darwin) os="macos" ;;
    *)
        echo "Unsupported operating system: $(uname -s)" >&2
        exit 1
        ;;
esac

# Determine architecture
arch=""
case "$(uname -m)" in
    x86_64)  arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *)
        echo "Unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

# Check for unsupported combinations
if [[ "$os" == "macos" && "$arch" == "x86_64" ]]; then
    echo "Unsupported platform: x86_64 macOS binaries are not available." >&2
    exit 1
fi

archive_name="hostcmd-${arch}-${os}"
url="https://github.com/esamattis/hostcmd/releases/latest/download/${archive_name}.tar.gz"

echo "Downloading ${url}..."

# Create a temporary directory for extraction
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

# Download and extract
curl -fsSL "$url" | tar -xzf - -C "$tmpdir"

# Ensure ~/.local/bin exists
install_dir="${HOME}/.local/bin"
mkdir -p "$install_dir"

# Move the binary into place
mv "${tmpdir}/hostcmd" "${install_dir}/hostcmd"
chmod +x "${install_dir}/hostcmd"

"${install_dir}/hostcmd" --version

echo "hostcmd installed to ${install_dir}/hostcmd"
