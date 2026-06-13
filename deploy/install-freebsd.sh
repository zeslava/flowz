#!/bin/sh
set -e

REPO_DIR="$(pwd)"

BIN_SRC="${REPO_DIR}/target/release/flowz"
BIN_DST="/usr/local/bin/flowz"
RC_SERVER_SRC="${REPO_DIR}/deploy/flowz-server.rc"
RC_AGENT_SRC="${REPO_DIR}/deploy/flowz-agent.rc"
RC_SERVER_DST="/usr/local/etc/rc.d/flowz_server"
RC_AGENT_DST="/usr/local/etc/rc.d/flowz_agent"
CFG_DIR="/usr/local/etc/flowz"
SERVER_CFG_SRC="${REPO_DIR}/flowz-server.yaml"
AGENT_CFG_SRC="${REPO_DIR}/flowz-agent.yaml"
SERVER_CFG_DST="${CFG_DIR}/server.yaml"
AGENT_CFG_DST="${CFG_DIR}/agent.yaml"
SUDOERS_SRC="${REPO_DIR}/deploy/flowz-agent.sudoers"
SUDOERS_DST="/usr/local/etc/sudoers.d/flowz-agent"
DOAS_SRC="${REPO_DIR}/deploy/flowz-agent.doas.conf"
DOAS_DST="/usr/local/etc/doas.conf"

if [ ! -f "${BIN_SRC}" ]; then
    echo "Binary not found at ${BIN_SRC}"
    echo "Run: cargo build --release"
    exit 1
fi

echo "Installing flowz..."

# User and group
pw groupshow flowz >/dev/null 2>&1 || pw groupadd flowz
pw usershow  flowz >/dev/null 2>&1 || \
    pw useradd flowz -g flowz -d /nonexistent -s /usr/sbin/nologin
echo "  user flowz"

# Data directories
for dir in /var/db/flowz /var/db/flowz/work /var/db/flowz/artifacts \
           /var/log/flowz /var/run/flowz; do
    mkdir -p "${dir}"
    chown flowz:flowz "${dir}"
done
echo "  data directories"

# Privilege escalation: prefer doas (FreeBSD-idiomatic), fall back to sudo
if command -v doas >/dev/null 2>&1; then
    DOAS_RULE="$(cat "${DOAS_SRC}")"
    if [ ! -f "${DOAS_DST}" ] || ! grep -qF "${DOAS_RULE}" "${DOAS_DST}"; then
        printf '%s\n' "${DOAS_RULE}" >> "${DOAS_DST}"
        echo "  ${DOAS_DST} (appended)"
    else
        echo "  ${DOAS_DST} (rule present)"
    fi
elif command -v visudo >/dev/null 2>&1; then
    cp "${SUDOERS_SRC}" "${SUDOERS_DST}.tmp"
    chmod 440 "${SUDOERS_DST}.tmp"
    if visudo -cf "${SUDOERS_DST}.tmp"; then
        mv "${SUDOERS_DST}.tmp" "${SUDOERS_DST}"
        echo "  ${SUDOERS_DST}"
    else
        rm -f "${SUDOERS_DST}.tmp"
        echo "  sudoers validation failed, skipping"
    fi
else
    echo "  neither doas nor sudo found (pkg install doas), skipping"
fi

# Enable in rc.conf before stop (service refuses to act without _enable=YES)
if ! grep -q 'flowz_server_enable' /etc/rc.conf; then
    echo 'flowz_server_enable="YES"' >> /etc/rc.conf
fi
if ! grep -q 'flowz_agent_enable' /etc/rc.conf; then
    echo 'flowz_agent_enable="YES"' >> /etc/rc.conf
fi

# Stop services before replacing binary
service flowz_server stop 2>/dev/null || true
service flowz_agent stop 2>/dev/null || true

# Binary
cp "${BIN_SRC}" "${BIN_DST}"
chmod 755 "${BIN_DST}"
echo "  ${BIN_DST}"

# rc.d scripts
cp "${RC_SERVER_SRC}" "${RC_SERVER_DST}"
chmod 755 "${RC_SERVER_DST}"
echo "  ${RC_SERVER_DST}"

cp "${RC_AGENT_SRC}" "${RC_AGENT_DST}"
chmod 755 "${RC_AGENT_DST}"
echo "  ${RC_AGENT_DST}"

# Config files (keep existing)
mkdir -p "${CFG_DIR}"
if [ ! -f "${SERVER_CFG_DST}" ]; then
    cp "${SERVER_CFG_SRC}" "${SERVER_CFG_DST}"
    echo "  ${SERVER_CFG_DST} (new)"
else
    echo "  ${SERVER_CFG_DST} (kept existing)"
fi

if [ ! -f "${AGENT_CFG_DST}" ]; then
    cp "${AGENT_CFG_SRC}" "${AGENT_CFG_DST}"
    echo "  ${AGENT_CFG_DST} (new)"
else
    echo "  ${AGENT_CFG_DST} (kept existing)"
fi

echo "  /etc/rc.conf"

echo ""
echo "Done."
echo "Next steps:"
echo "  1. Edit ${SERVER_CFG_DST} — set webhook_secret"
echo "  2. doas service flowz_server start"
echo "  3. doas service flowz_agent start"
