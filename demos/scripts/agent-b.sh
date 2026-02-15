#!/bin/bash
set -e
chmod +x ~/tandem
~/tandem init --tandem-server=localhost:13013 --workspace=agent-b ~/work
mkdir -p ~/work/src
echo 'pub fn handle_request(path: &str) -> u16 { 200 }' > ~/work/src/api.rs
cd ~/work && ~/tandem --config=fsmonitor.backend=none new -m 'feat: add API routes'
