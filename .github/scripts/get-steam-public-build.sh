#!/usr/bin/env bash
set -euo pipefail

steamcmd_path=${1:?usage: get-steam-public-build.sh <steamcmd-path>}
output="$("$steamcmd_path" +login anonymous +app_info_update 1 +app_info_print 730 +quit)"

# app_info_print is Valve KeyValues text. We only accept buildid inside the
# public branch, not a build ID from another branch or depot.
build_id="$(awk '
  /"branches"/ { branches = 1; next }
  branches && /"public"/ { public = 1; next }
  public && /"buildid"/ { gsub(/"/, "", $2); print $2; exit }
' <<<"$output")"

[[ $build_id =~ ^[0-9]+$ ]] || {
  echo "SteamCMD did not return a public build ID for app 730." >&2
  exit 1
}
printf '%s\n' "$build_id"
