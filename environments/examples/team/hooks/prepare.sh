#!/usr/bin/env bash
set -euo pipefail
. "$BEAMS_SCRIPT_ROOT/lib/common.sh"
mkdir -p "$HOME/.config"
touch "$HOME/.config/team-ready"
report "Shell files ready"
