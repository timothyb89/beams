#!/usr/bin/env bash
# Checks session-bind verification against real OpenSSH for each host key
# type (RSA sha2-512/256, ECDSA P-256/384/521): runs `beam-bootstrap
# agent-proxy-spike` in front of a throwaway agent and logs in to local
# sshds that each offer only one host key algorithm.
#
#   bootstrap/tests/ssh-agent-hostkey-algos.sh path/to/beam-bootstrap
set -u
BIN=${1:?path to beam-bootstrap}
W=$(mktemp -d /tmp/hostkey-algos.XXXXXX)
ssh-agent -a "$W/up.sock" -s >"$W/agent.env"
AGENT_PID=$(sed -n 's/^SSH_AGENT_PID=\([0-9]*\);.*/\1/p' "$W/agent.env")
ssh-keygen -q -t ed25519 -N '' -f "$W/id"
SSH_AUTH_SOCK="$W/up.sock" ssh-add -q "$W/id"
"$BIN" agent-proxy-spike --listen "$W/proxy.sock" --upstream "$W/up.sock" >"$W/proxy.log" 2>&1 &
PROXY=$!
sleep 0.5
port=2251
for spec in "rsa:rsa-sha2-512" "rsa:rsa-sha2-256" "ecdsa:ecdsa-sha2-nistp256" "ecdsa384:ecdsa-sha2-nistp384" "ecdsa521:ecdsa-sha2-nistp521"; do
    type=${spec%%:*}; algo=${spec#*:}
    case $type in
        rsa) gen=(-t rsa -b 3072) ;;
        ecdsa) gen=(-t ecdsa -b 256) ;;
        ecdsa384) gen=(-t ecdsa -b 384) ;;
        ecdsa521) gen=(-t ecdsa -b 521) ;;
    esac
    hk="$W/hostkey-$type"
    [ -f "$hk" ] || ssh-keygen -q "${gen[@]}" -N '' -f "$hk"
    cat >"$W/sshd-$port.conf" <<EOF
Port $port
ListenAddress 127.0.0.1
HostKey $hk
HostKeyAlgorithms $algo
PidFile $W/sshd-$port.pid
AuthorizedKeysFile $W/id.pub
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
EOF
    /usr/sbin/sshd -f "$W/sshd-$port.conf" -E "$W/sshd-$port.log"
    sleep 0.3
    before=$(grep -c '"event":"session_bind"' "$W/proxy.log")
    SSH_AUTH_SOCK="$W/proxy.sock" ssh -F /dev/null -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o HostKeyAlgorithms=$algo -o BatchMode=yes -o IdentityFile=none -p $port "$(id -un)@127.0.0.1" true 2>/dev/null
    rc=$?
    bind=$(grep '"event":"session_bind"' "$W/proxy.log" | tail -n +$((before + 1)) | head -1)
    verified=$(echo "$bind" | grep -o '"verified":[^,}]*')
    keytype=$(echo "$bind" | grep -o '"host_key_type":"[^"]*"')
    echo "$algo: login rc=$rc $keytype $verified"
    kill "$(cat "$W/sshd-$port.pid")"
    port=$((port + 1))
done
kill "$PROXY" "$AGENT_PID"
rm -rf "$W"
