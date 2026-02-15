#!/bin/bash
set -e
chmod +x ~/tandem
pkill -f 'tandem serve' 2>/dev/null || true
sleep 1
rm -rf ~/project
nohup ~/tandem serve --listen 0.0.0.0:5555 --repo ~/project > ~/tandem.log 2>&1 &
sleep 2 && cat ~/tandem.log
