#!/usr/bin/env bash
# This scripts requires that cargo and awscli are installed and configured
source ~/.bashrc
if ! [ -x "$(command -v cargo)" ]; then
  echo 'Error: cargo is not installed.' >&2
  exit 1
fi
branch=${1:-master}

sudo apt update
# Wait for apt to be unlocked
apt_wait
sudo apt install -y iotop clang git jq pssh
pip3 install prettytable

if [[ ! -d conflux-rust ]]; then
  git clone https://github.com/Conflux-Chain/conflux-rust
fi

cd conflux-rust
git reset --hard
git checkout $branch
git pull
cargo update
cargo build --release
./dev-support/dep_pip3.sh
cd test/scripts
cp ../../target/release/conflux throttle_bitcoin_bandwidth.sh remote_start_conflux.sh remote_collect_log.sh stat_latency_map_reduce.py ~

cd ~
./throttle_bitcoin_bandwidth.sh 20 30
ls /sys/fs/cgroup/net_cls

apt_wait () {
  while sudo fuser /var/lib/dpkg/lock >/dev/null 2>&1 ; do
    sleep 1
  done
  while sudo fuser /var/lib/apt/lists/lock >/dev/null 2>&1 ; do
    sleep 1
  done
  if [ -f /var/log/unattended-upgrades/unattended-upgrades.log ]; then
    while sudo fuser /var/log/unattended-upgrades/unattended-upgrades.log >/dev/null 2>&1 ; do
      sleep 1
    done
  fi
}