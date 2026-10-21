#!/usr/bin/env fish
# ~/.ssh/agent-init.fish
# Usage: source ~/.ssh/agent-init.fish [path_to_private_key]
#
# Fish port of bin/ssh-agent-init. `ssh-agent -s` emits Bourne-shell
# assignments (SSH_AUTH_SOCK=...; export SSH_AUTH_SOCK; ...), which fish
# can't `source` directly, so we parse the two lines we need by hand.
#
# Steps 1-3 (find or start the agent) always run, interactive or not - safe
# to source from anywhere, including a noninteractive `tsh ssh ... -- cmd`
# (fish is the beam's default login shell, so config.fish - and this - runs
# for those too). Step 4 (unlock + ssh-add the key, which may prompt for a
# passphrase) only runs when stdin/stdout are both a tty, so a noninteractive
# session just gets a running agent with whatever keys it already holds,
# rather than hanging on a password prompt nobody can answer.

function _ssh_agent_load_env
    set -l env_file $argv[1]
    for line in (cat "$env_file" 2>/dev/null)
        if string match -q 'SSH_AUTH_SOCK=*' -- $line
            set -gx SSH_AUTH_SOCK (string replace -r '^SSH_AUTH_SOCK=([^;]*);.*$' '$1' -- $line)
        else if string match -q 'SSH_AGENT_PID=*' -- $line
            set -gx SSH_AGENT_PID (string replace -r '^SSH_AGENT_PID=([^;]*);.*$' '$1' -- $line)
        end
    end
end

function _setup_ssh_agent
    set -l key_file $argv[1]
    if test -z "$key_file"
        set key_file "$HOME/.ssh/beams_id_ed25519"
    end

    set -l rt_dir $XDG_RUNTIME_DIR
    if test -z "$rt_dir"
        set rt_dir "$HOME/.ssh"
    end
    set -l env_file "$rt_dir/agent.env"

    # 1. Load saved environment if SSH_AUTH_SOCK is unset or stale
    if test -z "$SSH_AUTH_SOCK"; and test -f "$env_file"
        _ssh_agent_load_env "$env_file"
    end

    # 2. Test if the socket is alive and responsive
    set -l agent_status 2
    if test -n "$SSH_AUTH_SOCK"
        ssh-add -l >/dev/null 2>&1
        set agent_status $status
    end

    # 3. If unreachable (status 2), clean up and spawn a new agent
    if test "$agent_status" -eq 2
        pkill -u "$USER" -x ssh-agent 2>/dev/null
        mkdir -p (dirname "$env_file")
        ssh-agent -s > "$env_file"
        chmod 600 "$env_file"
        _ssh_agent_load_env "$env_file"
    end

    # 4. Check key presence and import if not already loaded - only when
    #    interactive, so a noninteractive caller never hits a password
    #    prompt it can't answer.
    if not test -f "$key_file"
        echo "ssh-agent: Key not found at $key_file" >&2
    else if isatty stdin; and isatty stdout
        set -l key_fp (ssh-keygen -lf "$key_file" 2>/dev/null | awk '{print $2}')
        if not ssh-add -l 2>/dev/null | grep -qF "$key_fp"
            ssh-add "$key_file"
        end
    end
    # else: noninteractive and the key isn't loaded yet - leave it locked.
    # Whatever needs it (ssh, git) will fail with its own clear error
    # instead of blocking on an unanswerable prompt.
end

_setup_ssh_agent $argv
functions -e _setup_ssh_agent
functions -e _ssh_agent_load_env
