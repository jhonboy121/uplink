#!/bin/sh
# A bad network for this box's calls, both ways, to test rate control and reconnecting against a
# phone. Only call traffic is shaped: UDP (iroh's direct paths) and TCP 443 (the relays). SSH, and
# the adb tunnel over it, never are, so a profile cannot lock you out.
#
#   netem.sh <profile> [seconds]   apply a profile; with seconds, undo it after that long
#   netem.sh off                   back to the plain network
#   netem.sh show                  what is applied now
#
# Profiles are netem arguments, the same both ways; `custom "<netem args>"` takes your own.
# A profile with a rate also has a limit: the packets its delay holds plus about 250 ms of queue
# at that rate (1250-byte packets), as a real bottleneck buffers. netem's own default of 1000
# packets is many seconds at these rates, which no network holds: every packet waits and none
# is dropped, so the round trip climbs without end.
set -eu

IFB=ifb-uplink
RELAY_PORT=443
# The interface the default route leaves by.
DEV=$(ip route show default | awk '{ for (i = 1; i < NF; i++) if ($i == "dev") { print $(i + 1); exit } }')
[ -n "$DEV" ] || { echo "no default route" >&2; exit 1; }

profile() {
    case "$1" in
        3g) echo "rate 1mbit delay 150ms 30ms distribution normal loss 1% limit 40" ;;
        4g-bad) echo "rate 3mbit delay 80ms 20ms distribution normal loss 2% limit 100" ;;
        slow) echo "rate 500kbit delay 200ms limit 23" ;;
        lossy) echo "delay 40ms loss 5%" ;;
        jitter) echo "delay 100ms 80ms distribution normal" ;;
        # A step down that rate control has to find: plenty of delay budget, little bandwidth.
        squeeze) echo "rate 800kbit delay 30ms limit 23" ;;
        cut) echo "loss 100%" ;;
        *) return 1 ;;
    esac
}

off() {
    tc qdisc del dev "$DEV" root 2>/dev/null || true
    tc qdisc del dev "$DEV" ingress 2>/dev/null || true
    ip link del "$IFB" 2>/dev/null || true
}

# Call traffic on `dev`/`parent` to `target`: UDP, and TCP to or from the relays' port.
classify() {
    dev=$1 parent=$2
    shift 2
    tc filter add dev "$dev" parent "$parent" protocol ip prio 1 u32 match ip protocol 17 0xff "$@"
    tc filter add dev "$dev" parent "$parent" protocol ip prio 2 u32 match ip dport "$RELAY_PORT" 0xffff "$@"
    tc filter add dev "$dev" parent "$parent" protocol ip prio 3 u32 match ip sport "$RELAY_PORT" 0xffff "$@"
}

apply() {
    args=$1
    off
    # Out: a prio qdisc sends everything to its middle band, untouched; call traffic to the
    # third, which has the netem.
    tc qdisc add dev "$DEV" root handle 1: prio bands 3 priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1
    # shellcheck disable=SC2086 # the profile is a list of netem arguments
    tc qdisc add dev "$DEV" parent 1:3 handle 30: netem $args
    classify "$DEV" 1: flowid 1:3
    # In: call traffic is redirected through an ifb, whose egress has the same netem.
    modprobe ifb 2>/dev/null || true
    ip link add "$IFB" type ifb
    ip link set "$IFB" up
    # shellcheck disable=SC2086
    tc qdisc add dev "$IFB" root netem $args
    tc qdisc add dev "$DEV" handle ffff: ingress
    classify "$DEV" ffff: action mirred egress redirect dev "$IFB"
    echo "netem on $DEV, both ways, UDP + TCP $RELAY_PORT only: $args"
}

case "${1:-}" in
    off)
        off
        echo "netem off on $DEV"
        ;;
    show)
        tc qdisc show dev "$DEV"
        tc qdisc show dev "$IFB" 2>/dev/null || true
        ;;
    custom)
        [ -n "${2:-}" ] || { echo "custom needs netem arguments, e.g. \"rate 2mbit loss 3%\"" >&2; exit 1; }
        apply "$2"
        ;;
    "")
        echo "usage: $0 <3g|4g-bad|slow|lossy|jitter|squeeze|cut|custom \"args\"|off|show> [seconds]" >&2
        exit 1
        ;;
    *)
        args=$(profile "$1") || { echo "unknown profile: $1" >&2; exit 1; }
        apply "$args"
        if [ -n "${2:-}" ]; then
            sleep "$2"
            off
            echo "netem off on $DEV after $2 s"
        fi
        ;;
esac
