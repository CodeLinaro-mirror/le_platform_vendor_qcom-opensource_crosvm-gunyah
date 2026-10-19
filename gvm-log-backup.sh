#!/bin/sh

# Backup GVM console logs (Proposal C: Systemd POST).
#
# Invoked from a qcrosvm unit's ExecStopPost= after the VM stops, so the
# previous console log is preserved before the next start truncates it.
#
# Usage:
#   gvm-log-backup.sh <log_path> [-n count] [-d dir] [-s max_size]
#
#   <log_path>   Console log to back up (e.g. /tmp/la_gvm.txt). Required.
#   -n count     Number of copies to keep. Default: 10.
#   -d dir       Full backup directory. If omitted, /data/logs/<vm> is used,
#                where <vm> is derived from the log basename (la_gvm.txt -> la_gvm).
#   -s max_size  Max size of the newest copy in bytes (K/M/G suffix allowed).
#                If exceeded, the copy is truncated keeping the newest (tail)
#                content. Default: 10M.
#
# All diagnostics are printed to stdout/stderr and therefore land in the
# journal (journalctl -u qcrosvm.service). Any error exits non-zero; the unit
# should invoke this with a leading '-' so a failure never affects the service.

TAG="[gvm-log-backup]"

log() { echo "$TAG $*"; }
err() { echo "$TAG ERROR: $*" >&2; }

# Parse a size string (bytes, or with K/M/G suffix) into bytes.
parse_size() {
    local s="$1" num unit
    num="${s%[KkMmGg]}"
    unit="${s#$num}"
    case "$num" in
        ''|*[!0-9]*) return 1 ;;
    esac
    case "$unit" in
        ''|b|B)   echo "$num" ;;
        k|K)      echo "$((num * 1024))" ;;
        m|M)      echo "$((num * 1024 * 1024))" ;;
        g|G)      echo "$((num * 1024 * 1024 * 1024))" ;;
        *)        return 1 ;;
    esac
}

LOG="$1"
if [ -z "$LOG" ]; then
    err "missing <log_path> argument"
    exit 1
fi
shift

COUNT=10
BACKUP_DIR=""
MAX_SIZE=$((10 * 1024 * 1024))   # 10M default

while [ $# -gt 0 ]; do
    case "$1" in
        -n) COUNT="$2"; shift 2 ;;
        -d) BACKUP_DIR="$2"; shift 2 ;;
        -s)
            MAX_SIZE=$(parse_size "$2") || { err "invalid -s size: $2"; exit 1; }
            shift 2
            ;;
        *) err "unknown argument: $1"; exit 1 ;;
    esac
done

# Validate count is a positive integer.
case "$COUNT" in
    ''|*[!0-9]*) err "invalid -n count: $COUNT"; exit 1 ;;
esac
if [ "$COUNT" -lt 1 ]; then
    err "invalid -n count (must be >= 1): $COUNT"
    exit 1
fi

# Nothing to back up if the source log is gone.
if [ ! -f "$LOG" ]; then
    log "source log '$LOG' does not exist, nothing to back up"
    exit 0
fi

# Derive vm/stem/ext from the log basename (la_gvm.txt -> vm=la_gvm, ext=txt).
base="$(basename "$LOG")"
stem="${base%.*}"
ext="${base##*.}"
if [ "$ext" = "$base" ]; then
    ext=""   # no extension
fi

# Resolve backup dir: explicit -d wins; otherwise /data/logs/<vm>.
if [ -z "$BACKUP_DIR" ]; then
    BACKUP_DIR="/data/logs/$stem"
fi

if ! mkdir -p "$BACKUP_DIR"; then
    err "failed to create backup dir '$BACKUP_DIR'"
    exit 1
fi

# Build a backup name for a given index: <stem>.<N>.<ext> (or <stem>.<N>).
backup_name() {
    if [ -n "$ext" ]; then
        echo "$BACKUP_DIR/$stem.$1.$ext"
    else
        echo "$BACKUP_DIR/$stem.$1"
    fi
}

# Rotate: .N-1 -> .N ... .1 -> .2 (the old .COUNT is overwritten = dropped,
# so at most COUNT copies are kept).
i=$((COUNT - 1))
while [ "$i" -ge 1 ]; do
    src="$(backup_name "$i")"
    dst="$(backup_name "$((i + 1))")"
    if [ -f "$src" ]; then
        mv -f "$src" "$dst"
    fi
    i=$((i - 1))
done

# Save current log as .1.
dot1="$(backup_name 1)"
if ! cp -f "$LOG" "$dot1"; then
    err "failed to copy '$LOG' -> '$dot1'"
    exit 1
fi

# Truncate .1 to the newest MAX_SIZE bytes if it exceeds the limit.
# ponytail: console logs are append-only, so the crash scene is at the tail;
# keep the tail. Raise -s if the crash window needs more history.
cur_size=$(stat -c %s "$dot1" 2>/dev/null || echo 0)
if [ "$cur_size" -gt "$MAX_SIZE" ]; then
    tmp="$dot1.trunc.$$"
    if tail -c "$MAX_SIZE" "$dot1" > "$tmp" && mv -f "$tmp" "$dot1"; then
        log "truncated '$dot1' from $cur_size to $MAX_SIZE bytes (kept newest)"
    else
        rm -f "$tmp"
        err "failed to truncate '$dot1'"
        exit 1
    fi
fi

log "backed up '$LOG' -> '$dot1' (keeping $COUNT copies in '$BACKUP_DIR')"
exit 0
