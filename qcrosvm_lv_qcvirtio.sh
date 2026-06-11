#!/bin/bash

# Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
# SPDX-License-Identifier: BSD-3-Clause-Clear

ARGS="--vm=autoghgvmlv \
--use-non-protected-virtio \
--disk=/dev/disk/by-partlabel/lv_misc,label=10E,rw=true \
--disk=/dev/disk/by-partlabel/lv_dtbo_a,label=124,rw=true \
--disk=/dev/disk/by-partlabel/lv_dtbo_b,label=125,rw=true \
--disk=/dev/disk/by-partlabel/lv_devinfo,label=101,rw=true \
--disk=/dev/disk/by-partlabel/lv_boot_a,label=106,rw=true \
--disk=/dev/disk/by-partlabel/lv_boot_b,label=107,rw=true \
--disk=/dev/disk/by-partlabel/lv_vbmeta_a,label=10C,rw=true \
--disk=/dev/disk/by-partlabel/lv_vbmeta_b,label=10D,rw=true \
--disk=/dev/disk/by-partlabel/lv_system_a,label=112,rw=true \
--disk=/dev/disk/by-partlabel/lv_system_b,label=113,rw=true \
--vhost-user-generic "/tmp/linux-vm3-blk-skt",label=111 \
--vhost-user-generic "/tmp/linux-vm3-gpu-skt",label=114,queue-num=2 \
--vhost-user-generic "/tmp/linux-vm3-net-skt",label=126 \
--vhost-user-generic "/tmp/linux-vm3-con-skt",label=127 \
--vhost-user-generic "/tmp/linux-vm3-rng-skt",label=129 \
--vhost-user-generic "/tmp/linux-vm3-vsk-skt",label=12A \
--vhost-user-generic "/tmp/linux-vm3-gpio-skt",label=12B \
--vhost-user-generic "/tmp/linux-vm3-scmi-skt",label=12C \
--vhost-user-generic "/tmp/linux-vm3-spi-skt",label=12D \
--vhost-user-generic "/tmp/linux-vm3-can-skt",label=12E \
--vhost-user-generic "/tmp/linux-vm3-i2c-skt",label=131 \
--vhost-user-generic "/tmp/linux-vm3-dec-skt",label=133 \
--vhost-user-generic "/tmp/linux-vm3-enc-skt",label=134 \
--vhost-user-generic "/tmp/linux-vm3-rtc-skt",label=135 \
--vhost-user-generic "/tmp/linux-vm3-input-skt",label=120
"

wait_for_service() {
    local svc="$1"
    local wait_time="$2"
    local interval="$3"

    local elapsed=0
    local state=""

    echo "Waiting for $svc (timeout=${wait_time}s, interval=${interval}s)"

    while awk "BEGIN {exit !($elapsed < $wait_time)}"; do
        state=$(systemctl is-active "$svc" 2>/dev/null)
        echo " -> $svc state=$state elapsed=${elapsed}s"

        if [ "$state" = "failed" ]; then
            return 1
        fi

        sleep "$interval"
        elapsed=$(awk "BEGIN {print $elapsed + $interval}")
    done

    echo " -> timeout (last state: $state)"

    if [ "$state" = "active" ]; then
        #echo " -> $svc is active"
        return 0
    fi

    return 1
}

add_service_if_ready() {
    local wait_time=0
    local interval=1

    # parse args
    while [ $# -gt 0 ]; do
        case "$1" in
            -w)
                wait_time="$2"
                shift 2
                ;;
            -i)
                interval="$2"
                shift 2
                ;;
            *)
                break
                ;;
        esac
    done

    local svc="$1"
    local arg="$2"

    if wait_for_service "$svc" "$wait_time" "$interval"; then
        ARGS="$ARGS $arg"
    else
        echo "Skipping $svc"
    fi
}

# Add qcvirtio-camera device if service not failed
add_service_if_ready -w 0.2 -i 0.1 \
    qcvirtio-camera-agl.service \
    "--vhost-user-generic /tmp/linux-vm3-viocam-skt,label=130,queue-num=2"

echo "Starting qcrosvm:"
echo "${ARGS}"
echo

exec /usr/bin/qcrosvm ${ARGS}
