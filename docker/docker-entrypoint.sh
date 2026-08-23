#!/bin/sh
set -eu

# Seed a new persistent data volume with safe, usable defaults. Once created,
# settings.toml is managed by the application and is never overwritten here.
if [ ! -e /data/settings.toml ]; then
    cp /usr/share/paperframe/settings.toml /data/settings.toml
fi
mkdir -p /data/queue

exec /usr/local/bin/paperframe-server "$@"
