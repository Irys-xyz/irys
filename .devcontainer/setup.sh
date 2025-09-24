#!/bin/bash
# custom devcontainer setup script
# this is designed for linux hosts! (notably the tempfs in the main devcontainer config - everything else should be platform-agnostic)
mkdir -p ~/.cargo
cat << 'EOF' >> ~/.cargo/config.toml
[build]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
EOF

sudo apt update

sudo apt install -y --no-install-recommends \
     curl jq build-essential libssl-dev libffi-dev python3 python3-venv python3-dev python3-pip wget git clang libssl-dev pkg-config libclang-dev libgmp-dev bc zstd

sudo wget https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64/cuda-keyring_1.1-1_all.deb
sudo dpkg -i cuda-keyring_1.1-1_all.deb
sudo rm cuda-keyring_1.1-1_all.deb
sudo apt-get update

sudo apt install -y --no-install-recommends cuda-toolkit-12-6 

cat << 'EOF' >> /home/vscode/.bashrc
HISTSIZE=10000
HISTFILESIZE=20000
export PATH="/usr/local/cuda/bin:$PATH"
export LD_LIBRARY_PATH="/usr/local/cuda/lib64:$LD_LIBRARY_PATH"
PROMPT_COMMAND='history -a'
bind '"\e[A": history-search-backward'
bind '"\e[B": history-search-forward'
EOF