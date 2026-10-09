# U20: channel selection

Verified the visibility implementation integrated in U19 against Jellyfin
`4910aafa1a`, `MediaBrowser.Controller/Channels/Channel.cs::IsVisible` and
`IsChannelVisible`. `EnableAllChannels` and `EnabledChannels` control channel
entries and their content. A nonempty legacy `BlockedChannels` list replaces
the allow-list; administrators have no override. Live TV guide channel IDs are
not plugin channel references and keep their separate Live TV rules.

The current Web editor clears the legacy field; pinned upstream policy writes
do not update it. Adoption preserves that preference and visibility reads it.
Ferrofin's stock channel-provider list is empty: this verification uses stored
channel entries and does not claim support for loading native .NET providers.

Added a matrix regression for all/selected/none, deny-list precedence and both
administrator states. All 22 visibility tests, formatting and strict workspace
Clippy pass. A disposable native server passed 24 HTTP assertions on channel
entries and content, with policies changed through the API and legacy state
seeded only into the fixture database. Results:
`/tmp/ferrofin-dashboard-u20-results.json`; fixture:
`/tmp/ferrofin-dashboard-u20-4r228c72`.

No production code changed in this finding. U19 records the measured cost of
the shared visibility implementation.
