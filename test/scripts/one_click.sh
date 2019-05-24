#!/usr/bin/env bash
set -euxo pipefail

if [ $# -lt 2 ]; then
    echo "Parameters required: <key_pair> <instance_count> [<branch_name>]"
    exit 1
fi
key_pair="$1"
slave_count=$2
branch="${3:-lpl_test}"
slave_role=${key_pair}_exp_slave

./create_slave_image.sh $key_pair $branch

master_ip=`cat ips`
slave_image=`cat slave_image`

ssh ubuntu@${master_ip} "cd ./conflux-rust/test/scripts;./launch-on-demand.sh $slave_count $key_pair $slave_role $slave_image; python3 ./exp_latency.py --exp-name latency_latest"

rm -rf tmp_data
mkdir tmp_data
cd tmp_data
../list-on-demand.sh $slave_role || true
../terminate-on-demand.sh
cd ..

# Comment this line if the data on the master instances are needed for further analysis
./terminate-on-demand.sh