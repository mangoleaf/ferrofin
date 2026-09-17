#!/bin/sh
# Ignore ffprobe arguments and emit deterministic CSV for the process-path test.
printf '%s\n' 'packet,1.0,K_' 'stream,1.0'
