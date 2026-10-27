#!/bin/sh
# workerd through the glibc loader copied into the dev node image.
for ld in /opt/glibc/ld-linux-*; do :; done
exec "$ld" --library-path /opt/glibc /usr/local/lib/workerd "$@"
