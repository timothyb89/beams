# Environment-neutral starter: no cluster, bot, workstation key, or repo paths.
set -g fish_greeting

if type -q starship
    starship init fish | source
end
