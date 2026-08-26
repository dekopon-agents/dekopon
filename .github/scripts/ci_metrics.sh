#!/usr/bin/env bash
set -euo pipefail

state=${RUNNER_TEMP:-/tmp}/dekopon-ci-metrics-start

network_bytes() {
  local interface=''
  if command -v ip >/dev/null; then
    interface=$(ip -o route show default 2>/dev/null \
      | awk '{ for (i = 1; i <= NF; i++) if ($i == "dev") { print $(i + 1); exit } }' \
      || true)
  fi
  if [[ -n $interface && -r /sys/class/net/$interface/statistics/rx_bytes ]]; then
    awk 'FNR == 1 && NR == 1 { rx = $1 } FNR == 1 && NR > 1 { tx = $1 } END { print rx + 0, tx + 0 }' \
      "/sys/class/net/$interface/statistics/rx_bytes" \
      "/sys/class/net/$interface/statistics/tx_bytes"
  elif [[ -r /proc/net/dev ]]; then
    # Exclude local service/container traffic when no default-route interface is discoverable.
    awk 'NR > 2 { iface = $1; sub(":", "", iface); if (iface != "lo" && iface !~ /^(docker|veth|br-)/) { rx += $2; tx += $10 } } END { print rx + 0, tx + 0 }' \
      /proc/net/dev
  else
    printf '0 0\n'
  fi
}

tree_kib() {
  local path=$1
  if [[ -e "$path" ]]; then
    du -sk "$path" 2>/dev/null | awk '{ print $1 + 0 }'
  else
    printf '0\n'
  fi
}

tree_files() {
  local path=$1
  if [[ -d "$path" ]]; then
    find "$path" -type f -print 2>/dev/null | wc -l | tr -d ' '
  else
    printf '0\n'
  fi
}

case "${1:-}" in
  start)
    mkdir -p "$(dirname "$state")"
    read -r rx_bytes tx_bytes < <(network_bytes)
    printf '%s %s %s %s %s %s\n' \
      "$(date +%s)" \
      "$rx_bytes" \
      "$tx_bytes" \
      "$(tree_kib target)" \
      "$(tree_files target)" \
      "$(tree_kib "${CARGO_HOME:-$HOME/.cargo}/registry")" \
      > "$state"
    ;;
  finish)
    if [[ ! -r "$state" ]]; then
      echo 'CI metrics baseline is absent; skipping the summary.' >&2
      exit 0
    fi

    read -r start_epoch start_rx start_tx start_target_kib start_target_files start_registry_kib \
      < "$state"
    read -r end_rx end_tx < <(network_bytes)

    elapsed=$(( $(date +%s) - start_epoch ))
    rx_delta=$(( end_rx >= start_rx ? end_rx - start_rx : 0 ))
    tx_delta=$(( end_tx >= start_tx ? end_tx - start_tx : 0 ))
    end_target_kib=$(tree_kib target)
    end_target_files=$(tree_files target)
    end_registry_kib=$(tree_kib "${CARGO_HOME:-$HOME/.cargo}/registry")

    rx_mib=$(awk -v bytes="$rx_delta" 'BEGIN { printf "%.1f", bytes / 1048576 }')
    tx_mib=$(awk -v bytes="$tx_delta" 'BEGIN { printf "%.1f", bytes / 1048576 }')
    target_before_mib=$(awk -v kib="$start_target_kib" 'BEGIN { printf "%.1f", kib / 1024 }')
    target_after_mib=$(awk -v kib="$end_target_kib" 'BEGIN { printf "%.1f", kib / 1024 }')
    registry_before_mib=$(awk -v kib="$start_registry_kib" 'BEGIN { printf "%.1f", kib / 1024 }')
    registry_after_mib=$(awk -v kib="$end_registry_kib" 'BEGIN { printf "%.1f", kib / 1024 }')
    cache_hit=${CARGO_CACHE_HIT:-unknown}
    cache_key=${CARGO_CACHE_MATCHED_KEY:-none}

    printf 'CI metrics: elapsed=%ss rx=%sMiB tx=%sMiB target=%s->%sMiB files=%s->%s registry=%s->%sMiB cache_hit=%s cache_key=%s\n' \
      "$elapsed" "$rx_mib" "$tx_mib" \
      "$target_before_mib" "$target_after_mib" \
      "$start_target_files" "$end_target_files" \
      "$registry_before_mib" "$registry_after_mib" \
      "$cache_hit" "$cache_key"

    if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
      markdown_tick='`'
      {
        printf '### CI metrics: %s%s%s\n\n' \
          "$markdown_tick" "${GITHUB_JOB:-unknown}" "$markdown_tick"
        printf '| Elapsed | RX | TX | Target MiB | Target files | Cargo registry MiB | Cache hit |\n'
        printf '|---:|---:|---:|---:|---:|---:|---:|\n'
        printf '| %ss | %s | %s | %s → %s | %s → %s | %s → %s | %s%s%s |\n\n' \
          "$elapsed" "$rx_mib" "$tx_mib" \
          "$target_before_mib" "$target_after_mib" \
          "$start_target_files" "$end_target_files" \
          "$registry_before_mib" "$registry_after_mib" \
          "$markdown_tick" "$cache_hit" "$markdown_tick"
        printf 'Matched Cargo cache key: %s%s%s\n' \
          "$markdown_tick" "$cache_key" "$markdown_tick"
      } >> "$GITHUB_STEP_SUMMARY"
    fi
    ;;
  *)
    echo "usage: $0 start|finish" >&2
    exit 2
    ;;
esac
