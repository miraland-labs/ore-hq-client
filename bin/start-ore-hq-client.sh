#!/usr/bin/env bash
# or (#!/bin/bash)

# Start ore-hq-client

set -e

CLI=$HOME/miraland-labs/ore-hq-client/target/release/ore-hq-client

MKP="$HOME/.config/solana/bp-inscription-wallet.json"

CORES=8
EXP_MIN_DIFF=18

# The command you want to run
# ~/miner/ore-hq-client/target/release$ ./ore-hq-client --use-http --url 192.168.2.107:3000 --keypair ~/.config/solana/bp-inscription-wallet.json mine --cores 8 --expected-min-difficulty 17

# CMD="$HOME/miner/ore-hq-client/target/release/ore-hq-client \
#         --use-http \
#         --url 192.168.2.107:3000 \
#         --keypair $MKP \
#         mine --cores 8 \
#         --expected-min-difficulty 17

# bash -c "$CMD"

# CMD="$CLI \
#         --use-http \
#         --url 192.168.2.107:3000 \
#         --keypair $MKP \
#         mine --cores $CORES \
#         --expected-min-difficulty $EXP_MIN_DIFF"

CMD="$CLI \
        --use-http \
        --url 192.168.2.107:3000 \
        --keypair $MKP \
        protomine --cores $CORES"

echo $CMD
until bash -c "$CMD"; do
    echo "Starting client command failed. Restart..."
    sleep 2
done
