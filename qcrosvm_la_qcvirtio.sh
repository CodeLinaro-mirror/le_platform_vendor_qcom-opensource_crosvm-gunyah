#!/bin/sh

# Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
# SPDX-License-Identifier: BSD-3-Clause-Clear

ARGS="--vm=autoghgvm \
--use-non-protected-virtio \
--disk=/dev/disk/by-partlabel/la_devinfo,label=21,rw=true \
--disk=/dev/disk/by-partlabel/la_v_boot_a,label=24,rw=true \
--disk=/dev/disk/by-partlabel/la_v_boot_b,label=25,rw=true \
--disk=/dev/disk/by-partlabel/la_dtbo_a,label=28,rw=true \
--disk=/dev/disk/by-partlabel/la_dtbo_b,label=29,rw=true \
--disk=/dev/disk/by-partlabel/la_boot_a,label=2A,rw=true \
--disk=/dev/disk/by-partlabel/la_boot_b,label=2B,rw=true \
--disk=/dev/disk/by-partlabel/la_vbmeta_a,label=30,rw=true \
--disk=/dev/disk/by-partlabel/la_vbmeta_b,label=31,rw=true \
--disk=/dev/disk/by-partlabel/la_misc,label=32,rw=true \
--disk=/dev/disk/by-partlabel/la_super,label=36,rw=true \
--vhost-user-generic "/tmp/linux-vm2-blk-skt",label=35 \
--vhost-user-generic "/tmp/linux-vm2-gpu-skt",label=4E,queue-num=2 \
--vhost-user-generic "/tmp/linux-vm2-snd-skt",label=4F \
--vhost-user-generic "/tmp/linux-vm2-net-skt",label=52 \
--vhost-user-generic "/tmp/linux-vm2-con-skt",label=53 \
--vhost-user-generic "/tmp/linux-vm2-vsk-skt",label=54 \
--vhost-user-generic "/tmp/linux-vm2-input-skt",label=40 \
--vhost-user-generic "/tmp/linux-vm2-gpio-skt",label=55 \
--vhost-user-generic "/tmp/linux-vm2-scmi-skt",label=56 \
--vhost-user-generic "/tmp/linux-vm2-spi-skt",label=57 \
--vhost-user-generic "/tmp/linux-vm2-can-skt",label=58 \
--vhost-user-generic "/tmp/linux-vm2-i2c-skt",label=59 \
--vhost-user-generic "/tmp/linux-vm2-rtc-skt",label=5A \
--vhost-user-generic "/tmp/linux-vm2-dec-skt",label=3B \
--vhost-user-generic "/tmp/linux-vm2-enc-skt",label=3D \
--vhost-user-generic "/tmp/linux-vm2-usb-skt",label=3C
"

wait_for_service() {
    svc="$1"
    service_wait_time="$2"
    service_interval="$3"

    elapsed=0
    state=""

    echo "Waiting for $svc (timeout=${service_wait_time}s, interval=${service_interval}s)"

    while awk "BEGIN {exit !($elapsed < $service_wait_time)}"; do
        state=$(systemctl is-active "$svc" 2>/dev/null)
        echo " -> $svc state=$state elapsed=${elapsed}s"

        if [ "$state" = "failed" ]; then
            return 1
        fi

        sleep "$service_interval"
        elapsed=$(awk "BEGIN {print $elapsed + $service_interval}")
    done

    echo " -> timeout (last state: $state)"

    if [ "$state" = "active" ]; then
        return 0
    fi

    return 1
}

add_service_if_ready() {
    wait_time=0
    interval=1

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

    service="$1"
    arg="$2"

    if wait_for_service "$service" "$wait_time" "$interval"; then
        ARGS="$ARGS $arg"
    else
        echo "Skipping $service"
    fi
}

# Add qcvirtio-camera device if service not failed
add_service_if_ready -w 0.2 -i 0.1 \
    qcvirtio-camera-agl.service \
    "--vhost-user-generic /tmp/linux-vm2-viocam-skt,label=3A,queue-num=2"

echo "Starting qcrosvm:"
echo "${ARGS}"
echo

exec /usr/bin/qcrosvm ${ARGS}
