#!/bin/bash

TUN_DEVICE_NAME=frank_tun

cargo b --release
# sudo setcap cap_net_admin=eip target/release/bluefin

sudo ./target/release/bluefin &
pid=$!

# sudo ip addr add 192.168.0.1/24 dev $TUN_DEVICE_NAME
# sudo ip link set up dev $TUN_DEVICE_NAME
trap "kill $pid" INT TERM
wait $pid
