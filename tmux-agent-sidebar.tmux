#!/usr/bin/env bash

PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Detect current platform
detect_platform() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)  arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *) echo "unknown" >&2 && return 1 ;;
    esac
    echo "${os}-${arch}"
}

PLATFORM="$(detect_platform 2>/dev/null || echo "")"

# 按优先级尝试各个路径，找到后验证二进制可用
try_binary() {
    local bin="$1"
    local ver
    ver="$("$bin" version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
    if [[ -n "$ver" ]]; then
        echo "$ver"
        return 0
    fi
    return 1
}

INSTALLED_VERSION=""
if [[ -n "$PLATFORM" && -x "$PLUGIN_DIR/bin/tmux-agent-sidebar-${PLATFORM}" ]]; then
    INSTALLED_VERSION="$(try_binary "$PLUGIN_DIR/bin/tmux-agent-sidebar-${PLATFORM}")" && SIDEBAR_BINARY="$PLUGIN_DIR/bin/tmux-agent-sidebar-${PLATFORM}"
fi
if [[ -z "$SIDEBAR_BINARY" && -x "$PLUGIN_DIR/bin/tmux-agent-sidebar" ]]; then
    INSTALLED_VERSION="$(try_binary "$PLUGIN_DIR/bin/tmux-agent-sidebar")" && SIDEBAR_BINARY="$PLUGIN_DIR/bin/tmux-agent-sidebar"
fi
if [[ -z "$SIDEBAR_BINARY" && -x "$PLUGIN_DIR/target/release/tmux-agent-sidebar" ]]; then
    INSTALLED_VERSION="$(try_binary "$PLUGIN_DIR/target/release/tmux-agent-sidebar")" && SIDEBAR_BINARY="$PLUGIN_DIR/target/release/tmux-agent-sidebar"
fi
if [[ -z "$SIDEBAR_BINARY" ]] && command -v "tmux-agent-sidebar" &>/dev/null; then
    INSTALLED_VERSION="$(try_binary "tmux-agent-sidebar")" && SIDEBAR_BINARY="tmux-agent-sidebar"
fi
if [[ -z "$SIDEBAR_BINARY" ]] && command -v brew &>/dev/null && [[ -x "$(brew --prefix 2>/dev/null)/bin/tmux-agent-sidebar" ]]; then
    INSTALLED_VERSION="$(try_binary "$(brew --prefix)/bin/tmux-agent-sidebar")" && SIDEBAR_BINARY="$(brew --prefix)/bin/tmux-agent-sidebar"
fi

if [[ -z "$SIDEBAR_BINARY" ]]; then
    tmux run-shell -b "bash '$PLUGIN_DIR/install-wizard.sh'"
    exit 0
fi

tmux set -g @agent_sidebar_bin "$SIDEBAR_BINARY"

tmux source-file "$PLUGIN_DIR/agent-sidebar.conf"
