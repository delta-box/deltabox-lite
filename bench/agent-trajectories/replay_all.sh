#!/bin/sh
# Replays every trajectory with each backend, one task image at a time, and
# deletes the image afterwards (each takes ~1 GB of layers + ~3 GB unpacked).
#   ./replay_all.sh [policy] [backend...]
set -u
cd "$(dirname "$0")"
policy=${1:-per_eval}
[ $# -gt 0 ] && shift
backends=${*:-manual git}
cache=../../target/agent-traj-cache
# At most one image on disk at any time.
clean() { rm -rf "$cache"/env-* "$cache"/blobs; }
clean
for id in $(python3 -c 'import json; print(" ".join(i["instance_id"] for i in json.load(open("sources.json"))["instances"]))'); do
    for b in $backends; do
        if [ -s "results/$id.$b.$policy.csv" ] && [ "${FORCE:-0}" = 0 ]; then
            echo "skip $id $b (done)"; continue
        fi
        echo "== $id $b $(date +%T)"
        python3 replay.py "$id" --backend "$b" --policy "$policy" || echo "FAILED $id $b"
    done
    clean
    echo "   disk free: $(df -h --output=avail . | tail -1)"
done
python3 summarize.py "$policy"
