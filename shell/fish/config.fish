# Synced into ~/.config/fish/config.fish by beam-bootstrap (shell-config
# step). Keep this minimal and beam-agnostic - it's the same file on every
# beam.

set -g fish_greeting

# Fish is the beam's default login shell, so this runs for *every* fish
# invocation - an interactive login and a noninteractive `tsh ssh ... --
# cmd` exec alike. ssh-agent-init.fish always ensures the agent is running,
# and only opportunistically prompts to unlock the key when it's actually
# interactive (a real tty) - so a noninteractive command never blocks on a
# password prompt, it just runs against whatever the agent already holds.
if test -f "$HOME/bin/ssh-agent-init.fish"
    source "$HOME/bin/ssh-agent-init.fish"
end

if type -q starship
    starship init fish | source
end
