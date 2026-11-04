# Synced into ~/.config/fish/config.fish by beam-bootstrap (shell-config
# step). Keep this minimal and beam-agnostic - it's the same file on every
# beam.

set -g fish_greeting

# Fish is the beam's default login shell, so this runs for *every* fish
# invocation - an interactive login and a noninteractive `tsh ssh ... --
# cmd` exec alike. The SSH agent is beam-bootstrapd's relay: it holds no
# keys, and each use of one is approved on the workstation (in the attached
# `beams ssh`). Say why when you use it: `SSH_KEY_REASON="..." git clone ...`.
set -gx SSH_AUTH_SOCK $HOME/.beam-bootstrap/ssh-agent.sock
set -gx TELEPORT_ADD_KEYS_TO_AGENT no

if type -q starship
    starship init fish | source
end
