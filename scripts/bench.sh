#!/usr/bin/env bash
# Throughput / concurrency benchmark for a running `share` server.
# Development tool only: the share binary itself never runs scripts.
#
#   scripts/bench.sh https://192.168.1.15:8080 big.iso [clients...]
#   scripts/bench.sh https://192.168.1.15:8080 big.iso 1 2 5 10
#
# Run it from a *different* machine on the network to measure the real path
# (Wi-Fi / Ethernet), and from the server itself (loopback) to measure the
# application + TLS ceiling without the network.
set -euo pipefail

url=${1:?usage: bench.sh BASE_URL FILE_PATH [CLIENTS...]}
file=${2:?usage: bench.sh BASE_URL FILE_PATH [CLIENTS...]}
shift 2
if [ $# -gt 0 ]; then clients=("$@"); else clients=(1 2 5 10); fi

enc=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$file" 2>/dev/null || printf '%s' "$file")
target="$url/download/$enc"

echo "target: $target"
for n in "${clients[@]}"; do
  tmp=$(mktemp -d)
  start=$(date +%s.%N)
  for i in $(seq 1 "$n"); do
    curl -ksS -o /dev/null -w '%{size_download} %{speed_download}\n' "$target" >"$tmp/$i" &
  done
  wait
  end=$(date +%s.%N)
  total=$(cat "$tmp"/* | awk '{b+=$1} END {print b}')
  per=$(cat "$tmp"/* | awk '{s+=$2; n++} END {printf "%.1f", s/n/1e6}')
  agg=$(awk -v b="$total" -v s="$start" -v e="$end" 'BEGIN {printf "%.1f", b/(e-s)/1e6}')
  printf '%2d client(s): aggregate %8s MB/s   mean per client %8s MB/s\n' "$n" "$agg" "$per"
  rm -rf "$tmp"
done
