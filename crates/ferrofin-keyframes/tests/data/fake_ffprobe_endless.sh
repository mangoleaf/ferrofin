#!/bin/sh
# Ignore ffprobe arguments and write packets until killed: a probe that never ends.
while :; do
  printf '%s\n' 'packet,1.0,K_'
done
