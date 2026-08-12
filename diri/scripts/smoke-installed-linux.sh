#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: installed runtime smoke requires Linux" >&2
    exit 64
fi

for tool in diri dirijor dirijord-rs jq; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "error: ${tool} is required for the installed runtime smoke" >&2
        exit 1
    fi
done

smoke_root="$(mktemp -d "${TMPDIR:-/tmp}/diri-installed-smoke.XXXXXX")"
daemon_pid=""
session_id=""

stop_daemon() {
    if [[ -n "${daemon_pid}" ]] && kill -0 "${daemon_pid}" 2>/dev/null; then
        kill "${daemon_pid}"
        wait "${daemon_pid}" 2>/dev/null || true
    fi
    daemon_pid=""
}

cleanup() {
    set +e
    if [[ -n "${session_id}" ]]; then
        if [[ -z "${daemon_pid}" ]] || ! kill -0 "${daemon_pid}" 2>/dev/null; then
            DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
                dirijord-rs >>"${smoke_root}/daemon.log" 2>&1 &
            daemon_pid=$!
            for _ in $(seq 1 50); do
                if DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
                    dirijor status --json >/dev/null 2>&1; then
                    break
                fi
                sleep 0.05
            done
        fi
        DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
            dirijor session release "${session_id}" --remove >/dev/null 2>&1 || true
    fi
    stop_daemon
    rm -rf "${smoke_root}"
}
trap cleanup EXIT

start_daemon() {
    DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
        dirijord-rs >>"${smoke_root}/daemon.log" 2>&1 &
    daemon_pid=$!
    # A stopped daemon can leave its socket inode behind briefly. Require a
    # successful control round-trip before treating the replacement as ready.
    for _ in $(seq 1 100); do
        if DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
            dirijor status --json >/dev/null 2>&1; then
            return
        fi
        if ! kill -0 "${daemon_pid}" 2>/dev/null; then
            cat "${smoke_root}/daemon.log" >&2
            echo "error: installed daemon exited before its socket was ready" >&2
            exit 1
        fi
        sleep 0.05
    done
    cat "${smoke_root}/daemon.log" >&2
    echo "error: installed daemon socket did not appear" >&2
    exit 1
}

read_until() {
    local expected="$1"
    for _ in $(seq 1 100); do
        if DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
            dirijor session read "${session_id}" --source scrollback 2>/dev/null \
            | grep -Fq "${expected}"; then
            return
        fi
        sleep 0.05
    done
    DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
        dirijor session read "${session_id}" --source scrollback >&2 || true
    echo "error: session output never contained ${expected}" >&2
    exit 1
}

start_daemon
DIRIJOR_APP_SUPPORT="${smoke_root}/support" dirijor doctor

project_dir="${smoke_root}/project with spaces-β"
mkdir -p "${project_dir}"
spawned="$(DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
    dirijor session spawn shell --cwd "${project_dir}" --title "Linux package smoke" --json)"
session_id="$(jq -er '.id' <<<"${spawned}")"

DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
    dirijor session send "${session_id}" "printf 'diri-linux-before-restart\\n'"
read_until "diri-linux-before-restart"

printf '%s\n' '{"prompt":"package smoke"}' \
    | DIRIJOR_APP_SUPPORT="${smoke_root}/support" DIRIJOR_SESSION_ID="${session_id}" \
        dirijor hook UserPromptSubmit >/dev/null
printf '%s\n' '{}' \
    | DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
        dirijor mcp-call --tool list_agents \
    | jq -e --arg id "${session_id}" '.ok.agents[] | select(.id == $id)' >/dev/null

# Kill only the Engine. The holder and shell must survive and be adopted by a
# new Engine process from the installed package.
stop_daemon
start_daemon
DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
    dirijor session get "${session_id}" --json >/dev/null
DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
    dirijor session send "${session_id}" "printf 'diri-linux-after-restart\\n'"
read_until "diri-linux-after-restart"

DIRIJOR_APP_SUPPORT="${smoke_root}/support" \
    dirijor session release "${session_id}" --remove >/dev/null
session_id=""

for directory in support support/logs support/holders; do
    permissions="$(stat -c '%a' "${smoke_root}/${directory}")"
    if [[ "${permissions}" != "700" ]]; then
        echo "error: ${directory} permissions are ${permissions}, expected 700" >&2
        exit 1
    fi
done

echo "Installed Linux runtime, CLI, MCP, hook, and daemon adoption smoke passed"
