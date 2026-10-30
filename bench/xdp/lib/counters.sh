#!/usr/bin/env bash

nic_versions() {
    local iface=$1 dev drv fw mtu out=""
    for dev in $(slaves_of "$iface"); do
        drv=$(basename "$(readlink -f "/sys/class/net/$dev/device/driver" 2>/dev/null)" 2>/dev/null)
        mtu=$(cat "/sys/class/net/$dev/mtu" 2>/dev/null || true)
        fw=$(ethtool -i "$dev" 2>/dev/null | awk '/^firmware-version:/{print $2; exit}' || true)
        out="${out:+$out }$dev=${drv:-?}/${fw:-?}/mtu${mtu:-?}"
    done
    echo "$out"
}

expand_cpu_list() {
    local part lo hi i
    for part in ${1//,/ }; do
        if [[ $part == *-* ]]; then
            lo=${part%-*}; hi=${part#*-}
            [[ $lo =~ ^[0-9]+$ && $hi =~ ^[0-9]+$ ]] || continue
            for ((i = lo; i <= hi; i++)); do echo "$i"; done
        elif [[ $part =~ ^[0-9]+$ ]]; then
            echo "$part"
        fi
    done
}

comp_vectors() {
    local iface=$1 dev irq name idx cpus
    for dev in $(slaves_of "$iface"); do
        for irq in $(ls "/sys/class/net/$dev/device/msi_irqs" 2>/dev/null | sort -n); do
            [[ -d /proc/irq/$irq ]] || continue
            name=$(ls "/proc/irq/$irq" 2>/dev/null | grep comp | head -1)
            [[ -n $name ]] || continue
            idx=${name#*comp}
            idx=${idx%%@*}
            [[ $idx =~ ^[0-9]+$ ]] || idx=-1
            cpus=$(cat "/proc/irq/$irq/effective_affinity_list" 2>/dev/null \
                   || cat "/proc/irq/$irq/smp_affinity_list" 2>/dev/null || echo '?')
            echo "$dev $irq $idx $name $cpus"
        done
    done
    return 0
}

comp_irq_queues() {
    local iface=$1 dev irq idx name cpus cpu
    while read -r dev irq idx name cpus; do
        [[ -n ${dev:-} ]] || continue
        for cpu in $(expand_cpu_list "$cpus"); do echo "$dev $idx $cpu"; done
    done < <(comp_vectors "$iface")
    return 0
}

tx_queue_blocks() {
    local iface=$1 cpus=$2 dev loops slaves s=0 offset=0 want usable room
    loops=$(expand_cpu_list "$cpus" | wc -l | tr -d ' ')
    (( loops > 0 )) || loops=1
    slaves=$(slaves_of "$iface" | wc -l | tr -d ' ')
    (( slaves > 0 )) || slaves=1
    for dev in $(slaves_of "$iface"); do
        want=$(( loops > s ? (loops - s + slaves - 1) / slaves : 0 ))
        usable=$(ethtool -l "$dev" 2>/dev/null \
                 | awk '/^Current hardware settings/{f=1} f&&/^Combined:/{print $2; exit}')
        if [[ $usable =~ ^[0-9]+$ ]] && (( usable >= want )); then
            room=$(( usable - want ))
            echo "$dev $(( offset < room ? offset : room )) $want"
        else
            echo "$dev 0 $want"
        fi
        offset=$(( offset + want ))
        s=$(( s + 1 ))
    done
    return 0
}

comp_irq_map() {
    local iface=$1 dev irq idx name cpus
    while read -r dev irq idx name cpus; do
        [[ -n ${dev:-} ]] || continue
        printf '%s=%s ' "$name" "$cpus"
    done < <(comp_vectors "$iface")
    return 0
}

thread_siblings() {
    expand_cpu_list \
        "$(cat "/sys/devices/system/cpu/cpu$1/topology/thread_siblings_list" 2>/dev/null || true)"
}

irq_counts() {
    awk -v x=$(($1 + 2)) -v y=$(($2 + 2)) \
        'NR > 1 && $1 ~ /^[0-9]+:$/ { a += $x; b += $y } END { print a + 0, b + 0 }' \
        /proc/interrupts
}

sample_host() {
    local cpus=$1 line hp="" f
    local -a lines
    mapfile -t lines < /proc/meminfo
    for line in "${lines[@]}"; do
        [[ $line == HugePages_Free:* ]] || continue
        f=${line#HugePages_Free:}; hp=${f// /}
        break
    done
    printf 'hp=%s' "$hp"
    [[ -n $cpus ]] || return 0
    mapfile -t lines < /proc/stat
    for line in "${lines[@]}"; do
        [[ $line == cpu[0-9]* ]] || continue
        set -- $line
        [[ " $cpus " == *" ${1#cpu} "* ]] || continue
        printf ' %s=%s:%s:%s:%s:%s:%s:%s:%s' "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9"
    done
    return 0
}

host_state() {
    local cpus=$1 out="" k v _ c f names
    read -r k v _ < /proc/loadavg
    out+="loadavg=$k;$v"
    for k in isolcpus nohz_full rcu_nocbs; do
        v=$(tr ' ' '\n' < /proc/cmdline | awk -F= -v k="$k" '$1 == k { print $2; exit }')
        [[ -n $v ]] && out+=" $k=$v"
    done
    c=${cpus%% *}; c=${c:-0}
    f=/sys/devices/system/cpu/cpu$c/cpufreq/scaling_governor
    [[ -r $f ]] && out+=" governor=$(< "$f")"
    f=/sys/kernel/mm/transparent_hugepage/enabled
    [[ -r $f ]] && out+=" thp=$(sed 's/.*\[\(.*\)\].*/\1/' "$f")"
    while read -r k v _; do
        case $k in HugePages_Total:) out+=" hugepages_total=$v" ;; HugePages_Free:) out+=" hugepages_free=$v" ;; esac
    done < /proc/meminfo
    out+=" clocksource=$(cat /sys/devices/system/clocksource/*/current_clocksource 2>/dev/null | head -1)"
    f=/sys/devices/system/cpu/smt/active
    [[ -r $f ]] && out+=" smt=$(< "$f")"
    out+=" numa=$(cat /sys/devices/system/node/node*/cpulist 2>/dev/null | tr '\n' ';' | sed 's/;$//')"
    names=""
    for c in $cpus; do
        f=/sys/devices/system/cpu/cpu$c/topology/thread_siblings_list
        [[ -r $f ]] && names+="cpu$c:$(< "$f");"
    done
    [[ -n $names ]] && out+=" siblings=${names%;}"
    c=${cpus%% *}; c=${c:-0}
    names=""
    for f in /sys/devices/system/cpu/cpu$c/cpuidle/state*; do
        [[ -d $f ]] || continue
        names+="$(< "$f/name"):$(< "$f/latency"):$(< "$f/disable");"
    done
    [[ -n $names ]] && out+=" cpuidle_cpu$c=${names%;}"
    if pgrep -x irqbalance >/dev/null 2>&1; then out+=" irqbalance=running"; else out+=" irqbalance=absent"; fi
    if command -v timedatectl >/dev/null 2>&1; then
        out+=" ntp=$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo unknown)"
    fi
    names=$(pgrep -a -f 'agave-validator|solana-validator|fdctl|firedancer' 2>/dev/null \
            | awk '{ print $2 }' | xargs -r -n1 basename 2>/dev/null | sort -u | tr '\n' ';' | sed 's/;$//')
    out+=" validators=${names:-none}"
    printf '%s\n' "$out" | tr ' \t' ' ' 
    return 0
}

watched_cpus() {
    local iface=$1 cpus=$2 blocks
    {
        expand_cpu_list "$cpus"
        [[ -n ${LAT_TX_CPU:-} ]] && echo "$LAT_TX_CPU"
        [[ -n ${LAT_RX_CPU:-} ]] && echo "$LAT_RX_CPU"
        blocks=$(tx_queue_blocks "$iface" "$cpus")
        awk 'NR == FNR { first[$1] = $2; cnt[$1] = $3; next }
             ($1 in first) && $2 >= first[$1] && $2 < first[$1] + cnt[$1] { print $3 }' \
            <(echo "$blocks") <(comp_irq_queues "$iface")
    } | grep -E '^[0-9]+$' | sort -n -u | tr '\n' ' ' | sed 's/ $//'
    return 0
}
