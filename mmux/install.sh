#!/usr/bin/env bash
set -euxo pipefail
cd "$(dirname "$0")"
cargo install --path .
cp ./mmux.service $HOME/.config/systemd/user/
systemctl daemon-reload --user
systemctl restart mmux --user
