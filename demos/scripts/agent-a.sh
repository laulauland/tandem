#!/bin/bash
set -e
chmod +x ~/tandem
~/tandem init --tandem-server=localhost:13013 ~/work
mkdir -p ~/work/src
echo 'pub fn authenticate(token: &str) -> bool { !token.is_empty() }' > ~/work/src/auth.rs
echo 'pub mod auth;' > ~/work/src/lib.rs
cd ~/work && ~/tandem --config=fsmonitor.backend=none new -m 'feat: add auth module'
