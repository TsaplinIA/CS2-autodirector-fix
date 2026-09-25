#!/usr/bin/env bash
set -euo pipefail

depot_downloader=${1:?usage: download-cs2-client-dll.sh <depot-downloader> <destination>}
destination=${2:?usage: download-cs2-client-dll.sh <depot-downloader> <destination>}
mkdir -p "$destination"

file_list="$destination/client-dll-filelist.txt"
printf 'game/csgo/bin/win64/client.dll\n' > "$file_list"

"$depot_downloader" \
  -app 730 \
  -os windows \
  -filelist "$file_list" \
  -dir "$destination" \
  -max-downloads 4

client_dll="$(find "$destination" -type f -path '*/game/csgo/bin/win64/client.dll' -print -quit)"
[[ -n $client_dll ]] || {
  echo "DepotDownloader completed without game/csgo/bin/win64/client.dll" >&2
  exit 1
}
printf '%s\n' "$client_dll"
