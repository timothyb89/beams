# Synced into ~/.config/fish/config.fish by beam-bootstrap (shell-config
# step). Keep this minimal and beam-agnostic - it's the same file on every
# beam.

set -g fish_greeting

if type -q starship
    starship init fish | source
end
