# Symlinked into ~/.config/fish/conf.d/beams-launch.fish by beam-bootstrap
# (shell-config step). Inside a launch started by the workstation
# (docs/development/launches.md), tells the daemon the shell is usable:
# an interactive launch starts its post work once both this and the TUI
# have reported. Fires at the first prompt, in the background, once per
# shell; the daemon ignores repeats from other panes. Outside a launch
# (no BEAMS_LAUNCH_ID) it does nothing.
if status is-interactive; and set -q BEAMS_LAUNCH_ID
    function __beams_launch_ready --on-event fish_prompt
        functions -e __beams_launch_ready
        set -l bin $BEAM_BOOTSTRAP_BIN
        if test -z "$bin"
            set bin (command -s beam-bootstrap)
        end
        if test -n "$bin"
            command $bin launch ready --component shell >/dev/null 2>&1 &
            disown 2>/dev/null
        end
    end
end
