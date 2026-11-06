#!/usr/bin/env bash
set -euo pipefail
. "$BEAMS_SCRIPT_ROOT/lib/common.sh"
jq -c . "$HOME/projects/data/example.json" > "$HOME/projects/data/configured.json"
report "Project data configured"
