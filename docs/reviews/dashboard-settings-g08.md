# G08: verify restart and shutdown

The existing lifecycle implementation is supported. `POST /System/Restart`
drains HTTP, tears down lifetime-owned background work, cancels scheduled jobs,
stops/joins the scanner and closes the database before constructing another
host. `POST /System/Shutdown` follows the same teardown and exits. HTTP draining
has a configured deadline so a long-lived connection cannot prevent restart
indefinitely. Restart-required state alone does not cause a signal or shutdown
to restart the process.

Expanded the real-server restart regression to save UI culture, Quick Connect
availability and the image encoding limit. The next lifetime preserves the full
configuration, applies Quick Connect's saved value, clears the restart flag,
and accepts the existing session token. The existing test also verifies stable
server identity, working metrics, discovery socket release/rebinding and a
clean shutdown. The backup-restore test verifies that teardown completes before
the next lifetime opens the restored database.

Both real HTTP/UDP integration tests pass, along with formatting and strict
workspace Clippy. Production code is unchanged. Final workspace tests, doctests
and coverage remain batch-end gates.
