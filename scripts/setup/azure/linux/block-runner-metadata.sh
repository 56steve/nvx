#!/bin/bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: the metadata firewall must be installed as root" >&2
    exit 1
fi
runner_uid=$(id -u nvx-runner)
if [ "$runner_uid" -eq 0 ] || ! command -v iptables >/dev/null 2>&1 ||
    ! command -v flock >/dev/null 2>&1; then
    echo "ERROR: the runner account, IPv4 firewall, or locking tool is unavailable" >&2
    exit 1
fi

umask 077
exec 9>/run/nvx-runner-metadata.lock
flock -x -w 30 9 || {
    echo "ERROR: could not acquire the metadata firewall update lock" >&2
    exit 1
}

rule_exists() {
    local status=0
    iptables -w 10 -C "$@" >/dev/null 2>&1 || status=$?
    case "$status" in
        0 | 1) return "$status" ;;
        *)
            echo "ERROR: could not inspect the metadata firewall rules (exit=$status)" >&2
            exit "$status"
            ;;
    esac
}

match=(OUTPUT -d 169.254.169.254/32 -m owner --uid-owner "$runner_uid")
untagged_rule=("${match[@]}" -j REJECT)
pending_rule=("${match[@]}" -m comment --comment nvx-runner-metadata-pending -j REJECT)
rule=("${match[@]}" -m comment --comment nvx-runner-metadata -j REJECT)

# Keep a verified block in place throughout replacement and duplicate cleanup.
iptables -w 10 -I OUTPUT 1 "${pending_rule[@]:1}"
rule_exists "${pending_rule[@]}" || {
    echo "ERROR: the runner account is not blocked from Azure instance metadata" >&2
    exit 1
}
while rule_exists "${untagged_rule[@]}"; do
    iptables -w 10 -D "${untagged_rule[@]}"
done
while rule_exists "${rule[@]}"; do
    iptables -w 10 -D "${rule[@]}"
done
iptables -w 10 -I OUTPUT 1 "${rule[@]:1}"
rule_exists "${rule[@]}" || {
    echo "ERROR: the final metadata firewall rule was not verified" >&2
    exit 1
}
while rule_exists "${pending_rule[@]}"; do
    iptables -w 10 -D "${pending_rule[@]}"
done
