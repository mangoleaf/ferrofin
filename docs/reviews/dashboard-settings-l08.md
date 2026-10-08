# L08: automatic metadata refresh interval

The existing implementation correctly consumes `AutomaticRefreshIntervalDays`.
A positive interval makes remote metadata due once the stored refresh timestamp
is old enough; zero and negative values disable this age trigger. A scan or
explicit refresh must still run: the field does not create a per-library timer.

Source: Jellyfin `4910aafa1a`, `MetadataService.RefreshMetadata` and
`RefreshWithProviders`; Ferrofin `refresh_plan::remote_due` and the scanner's
per-item refresh planning. No production change was needed.

Validation: all 83 provider refresh-plan tests pass. The existing scanner
integration test `an_elapsed_refresh_interval_refetches` verifies a new remote
request, probing and timestamp update. Two isolated native HTTP runs, Ferrofin
and Jellyfin 12.1.0, agree on all four observations: an item aged 60 days does
not refresh with zero or negative intervals, refreshes at 30 days, and does not
refresh again while fresh. Saved library options were changed over HTTP; only
the disposable fixture's timestamp was aged directly in SQLite.

Evidence: `/tmp/ferrofin-dashboard-l08-tests.log`,
`/tmp/ferrofin-dashboard-l08-ferrofin-85xet766/results.json`, and
`/tmp/ferrofin-dashboard-l08-jellyfin-ndjxeqvw/results.json`.
