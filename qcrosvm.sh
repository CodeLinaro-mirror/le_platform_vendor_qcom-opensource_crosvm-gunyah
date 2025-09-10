#!/bin/sh

# Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
# SPDX-License-Identifier: BSD-3-Clause-Clear

# We support maximum 16 bytes for audio variant,
# and maximum 496 bytes for user-defined cmdline.
# Hence, we define MAX_AUDIO_FW_LENGTH = 16 and MAX_USER_CMD_LENGTH = 496 in ABL.

usage() {
    echo "usage: qcrosvm.sh -h: show help"
    echo "usage: qcrosvm.sh <la or lv> [ cmdline e.g. initcall_debug ]"
}

# Argument la or lv is mandantory.
if [ "$#" -lt "1" -o "$1" = "-h" ]; then
    usage
    exit
fi

AUDDIO_VARIANT_PATH="/sys/kernel/debug/qcom_socinfo/adsp/variant"
AWE_VARIANT="lemans.adsp.aweQ"
AR_VARIANT="lemans.adsp.prodQ"
GVMINFO_IMG_SIZE=4096

gvm_variant=$1
echo "$0 Setup /tmp/${gvm_variant}_gvminfo.img"

user_cmdline=$2;
audio_variant=$(cat ${AUDDIO_VARIANT_PATH})
if [ -n "${audio_variant}" ]; then
  echo "$0: Audio variant=${audio_variant}"
fi
if [ -n "${user_cmdline}" ]; then
  echo "$0: User cmdline=${user_cmdline}"
fi

if [ "${audio_variant}" = "${AWE_VARIANT}" ]; then
  gvminfo="awe\0${user_cmdline}"
elif [ "${audio_variant}" = "${AR_VARIANT}" ]; then
  gvminfo="ar\0\0${user_cmdline}"
else
  gvminfo="\0\0\0\0${user_cmdline}"
fi

seg1_count=$(($(echo ${gvminfo} | wc -c) - 2))
seg2_count=$((${GVMINFO_IMG_SIZE} - ${seg1_count}))

cd /tmp
printf "${gvminfo}" | dd of=seg1.img bs=1 count=${seg1_count} 1>/dev/null 2>&1
dd if=/dev/zero of=seg2.img bs=1 count=${seg2_count} 1>/dev/null 2>&1
cat < seg2.img >> seg1.img
mv seg1.img /tmp/${gvm_variant}_gvminfo.img
rm -rf seg1.img seg2.img
cd -