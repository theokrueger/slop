#!/usr/bin/env bash
set -euxo pipefail
cd "$(dirname "$0")"
cargo install --path .
cp ./rpanel.service $HOME/.config/systemd/user/
sudo setcap 'CAP_NET_BIND_SERVICE+ep' $HOME/.cargo/bin/rpanel
systemctl daemon-reload --user
systemctl restart rpanel --user
