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

echo "Building flowz..."
cargo build --release

echo "Installing flowz..."

# User and group
doas pw groupshow flowz >/dev/null 2>&1 || doas pw groupadd flowz
doas pw usershow  flowz >/dev/null 2>&1 || \
    doas pw useradd flowz -g flowz -d /nonexistent -s /usr/sbin/nologin
echo "  user flowz"

# Data directories
for dir in /var/db/flowz /var/db/flowz/work /var/db/flowz/artifacts \
           /var/log/flowz /var/run/flowz; do
    doas mkdir -p "${dir}"
    doas chown flowz:flowz "${dir}"
done
echo "  data directories"

# Privilege escalation: prefer doas (FreeBSD-idiomatic), fall back to sudo
if command -v doas >/dev/null 2>&1; then
    DOAS_RULE="$(cat "${DOAS_SRC}")"
    if [ ! -f "${DOAS_DST}" ] || ! grep -qF "${DOAS_RULE}" "${DOAS_DST}"; then
        printf '%s\n' "${DOAS_RULE}" | doas tee -a "${DOAS_DST}" >/dev/null
        echo "  ${DOAS_DST} (appended)"
    else
        echo "  ${DOAS_DST} (rule present)"
    fi
elif command -v visudo >/dev/null 2>&1; then
    cp "${SUDOERS_SRC}" "${SUDOERS_DST}.tmp"
    doas chmod 440 "${SUDOERS_DST}.tmp"
    if visudo -cf "${SUDOERS_DST}.tmp"; then
        doas mv "${SUDOERS_DST}.tmp" "${SUDOERS_DST}"
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
    echo 'flowz_server_enable="YES"' | doas tee -a /etc/rc.conf >/dev/null
fi
if ! grep -q 'flowz_agent_enable' /etc/rc.conf; then
    echo 'flowz_agent_enable="YES"' | doas tee -a /etc/rc.conf >/dev/null
fi

# Stop services before replacing binary
doas service flowz_server stop 2>/dev/null || true
doas service flowz_agent stop 2>/dev/null || true

# Binary
doas cp "${BIN_SRC}" "${BIN_DST}"
doas chmod 755 "${BIN_DST}"
echo "  ${BIN_DST}"

# rc.d scripts
doas cp "${RC_SERVER_SRC}" "${RC_SERVER_DST}"
doas chmod 755 "${RC_SERVER_DST}"
echo "  ${RC_SERVER_DST}"

doas cp "${RC_AGENT_SRC}" "${RC_AGENT_DST}"
doas chmod 755 "${RC_AGENT_DST}"
echo "  ${RC_AGENT_DST}"

# Config files (keep existing)
doas mkdir -p "${CFG_DIR}"
if [ ! -f "${SERVER_CFG_DST}" ]; then
    doas cp "${SERVER_CFG_SRC}" "${SERVER_CFG_DST}"
    echo "  ${SERVER_CFG_DST} (new)"
else
    echo "  ${SERVER_CFG_DST} (kept existing)"
fi

if [ ! -f "${AGENT_CFG_DST}" ]; then
    doas cp "${AGENT_CFG_SRC}" "${AGENT_CFG_DST}"
    echo "  ${AGENT_CFG_DST} (new)"
else
    echo "  ${AGENT_CFG_DST} (kept existing)"
fi

echo "  /etc/rc.conf"

echo ""
echo "Done."
echo "  Edit ${SERVER_CFG_DST} if needed (webhook_secret, github_token, etc.)"
echo ""

doas service flowz_server start
doas service flowz_agent start
