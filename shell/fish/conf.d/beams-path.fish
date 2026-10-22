# Symlinked into ~/.config/fish/conf.d/beams-path.fish by beam-bootstrap
# (shell-config step). Fish sources every file under conf.d/ once per
# shell startup - no `source` line in config.fish needed for this one.
#
# fish_add_path silently skips any path that doesn't exist *yet* - so
# normally, a tool installed after this file last ran (e.g.
# `install-toolchain-go`, from a totally different shell, or the daemon)
# wouldn't reach PATH until some *later* shell started fresh. The
# `link-tool-paths` step sidesteps that by creating these directories
# (empty, at first) well ahead of anything being installed into them, so
# the first shell that ever sources this file already finds every one of
# them and adds them to the *universal* $fish_user_paths variable - which
# fish syncs live across every other already-running fish instance too
# (confirmed directly: a second process's `fish_add_path` call shows up
# in an already-open interactive fish session within ~1s, no new shell or
# `source` needed). So the moment an install task actually drops a binary
# into one of these directories, every already-open shell can just run it
# by name - nothing here has to run again.
#
# `~/bin` is included too even though it's already added to bash's PATH
# via ~/.profile's Debian-default block - fish doesn't source that file
# at all, so without this, `beam-shell`/`ssh-agent-init` would only be
# reachable by bare name from bash, not fish (the beam's own default
# shell). `~/.cargo/bin` isn't listed here - rustup's own installer
# already wires that in for both bash and fish on its own.
fish_add_path $HOME/bin
fish_add_path $HOME/.local/bin
fish_add_path $HOME/.local/toolchains/go/bin
fish_add_path $HOME/.local/toolchains/code-server/bin
