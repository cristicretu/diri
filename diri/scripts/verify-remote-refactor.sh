#!/usr/bin/env bash
#
# Checks the regressions found while reviewing the remote PTY Holder refactor
# are still fixed. Each check is a property that was BROKEN on the refactor
# branch before it was merged forward, so a failure here means one came back.
#
# Cheap and offline: no build, no network. Run it from anywhere in the repo.
#
#   diri/scripts/verify-remote-refactor.sh

set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
cd "${workspace_dir}"

manifests="crates/diri-engine/manifests"
rc=0

pass() { printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; rc=1; }
check() { # check <description> <command...>
    local description="$1"; shift
    if "$@" >/dev/null 2>&1; then pass "${description}"; else fail "${description}"; fi
}

echo "Remote PTY Holder refactor — regression checks"
echo

# The catalog once shrank from 20 Agents to 7. A missing manifest never errors; the
# Agent silently degrades to a bare login shell.
count="$(find "${manifests}" -name '*.json' -type f | wc -l | tr -d ' ')"
if [[ "${count}" -ge 22 ]]; then
    pass "Agent catalog carries ${count} manifests"
else
    fail "Agent catalog carries only ${count} manifests (expected at least 22)"
fi

# Eighteen launchable Agents declare returnToLoginShell. Dropping it means an Agent
# that exits or self-updates ends the session instead of landing at a prompt.
login_shell="$(grep -l returnToLoginShell "${manifests}"/*.json | wc -l | tr -d ' ')"
if [[ "${login_shell}" -eq 18 ]]; then
    pass "returnToLoginShell declared on ${login_shell} Agents"
else
    fail "returnToLoginShell declared on ${login_shell} Agents (expected 18)"
fi

# Without a caller-minted conversation id, Gemini cannot resume at all.
check "gemini mints a conversation id (--session-id)" \
    grep -q '"sessionIDFlag": "--session-id"' "${manifests}/gemini.json"

check "opencode declares how to resume" \
    python3 -c "import json,sys;sys.exit(0 if json.load(open('${manifests}/opencode.json'))['agent'].get('resume') else 1)"

# `agent` resolves whatever unrelated binary sits first on PATH.
check "cursor launches cursor-agent, not a bare 'agent'" \
    grep -q '"binary": "cursor-agent"' "${manifests}/cursor.json"

# Every descriptor must decode the way the client decodes it.
check "every manifest parses and carries an id" \
    python3 -c "
import glob, json, sys
for path in sorted(glob.glob('${manifests}/*.json')):
    agent = json.load(open(path)).get('agent')
    if not agent or not agent.get('id'):
        print(path); sys.exit(1)
"

# Handoff refused every call: the remote branch errored and local->local was
# rejected as 'already local'.
check "session.migrate refuses only without a transport" \
    grep -q 'self.remote.is_none()' crates/diri-engine/src/control.rs

check "resume relaunches a session whose Agent died" \
    grep -q 'Evicting the corpse' crates/diri-engine/src/control.rs

check "toolchain stays on the version main ships" \
    grep -q 'channel = "1.97.1"' rust-toolchain.toml

# Extracting the terminal crate forked before checkpoint v2 existed.
check "checkpoints still persist scrollback" \
    grep -q 'pub fn history_snapshot' crates/diri-terminal-state/src/lib.rs

check "terminal file/URL references survive" \
    grep -q 'pub fn reference_at' crates/diri-term/src/element.rs

check "packaging fails on a shrunken catalog" \
    grep -q 'bundled_manifests' scripts/package.sh

check "release prerequisites are documented" \
    grep -q 'aarch64-unknown-linux-musl' PACKAGING.md

echo
if [[ "${rc}" -eq 0 ]]; then
    echo "All checks passed."
else
    echo "Some checks failed: a reviewed regression came back." >&2
fi
exit "${rc}"
