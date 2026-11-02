#!/bin/sh
# Ignore ffprobe arguments. Once the extractor has had time to lower this
# process's priority, report its niceness as the format duration.
sleep 0.3
printf 'format,%s\n' "$(nice)"
