# Linux DVB tuner

Ferrofin can use local Linux DVB adapters through `dvbv5-zap` from v4l-utils.
It copies the tuner's MPEG transport stream into the shared Live TV buffer.
Viewers and DVR recordings share that buffer. Ferrofin can remux or transcode
the stream for playback using its existing FFmpeg pipeline.

Install v4l-utils and grant the Ferrofin service user access to the frontend,
demux, and DVR devices under `/dev/dvb`. Give Ferrofin exclusive use of the
adapters during testing. It leases the first free adapter with `frontend0`,
`demux0`, and `dvr0` and holds the lease until the capture process exits.

### Build a channel lineup

For a U.S. ATSC 1.0 antenna, run the scanner helper from this repository with
Python 3 and the v4l-utils capture tools installed. Run it as a user with access
to the DVB devices. Stop any service using the adapters before scanning them.

```sh
python3 scripts/scan-linux-dvb.py ./channels.conf
```

The default scan covers RF channels 2-36 and splits them across accessible
adapters. To narrow the scan using RabbitEars station hints, pass a five-digit
U.S. ZIP code.

```sh
python3 scripts/scan-linux-dvb.py ./channels.conf --zip 46725
```

The optional lookup sends the supplied ZIP code to RabbitEars. If the lookup
fails or returns no usable frequencies, the scanner sweeps the full selected
band. Omit `--zip` for an offline scan or to look for frequencies outside the
hints. Use `--rf 25 32` to scan specific RF channels instead of a ZIP lookup.
Use `--adapter 0` to pick one adapter or `--skip-vhf` to scan UHF only.

The scanner checks signal once per frequency and keeps weak multiplexes as
comments. Use `--include-weak` to keep those services enabled. If the driver
reports a lock without C/N in dB, the scanner keeps the services and reports
the missing measurement.

Review the candidate and install it where the service can read it. For a
systemd install whose service group is `ferrofin`, for example:

```sh
sudo install -m 0640 -o root -g ferrofin ./channels.conf /etc/ferrofin/channels.conf
```

For a rescan, write to a new output path. The helper publishes a completed
lineup only after validation and refuses to overwrite an existing file.
Keep a copy of the active lineup before installing a replacement.

You can also import an existing dvbv5-format lineup, such as ZapLink's
ATSC `channels.conf`.
Each station needs a unique `VCHANNEL`, a `FREQUENCY`, a `SERVICE_ID`, and
the delivery-system and PID fields needed by dvbv5-zap. The tuner uses the
virtual channel number to match XMLTV listings. Ferrofin writes only the
selected station into the temporary tuning file so repeated section names
or a shared RF frequency cannot select a different service.

### Add the tuner

Create a tuner with the authenticated administrator API
`POST /LiveTv/TunerHosts` and this JSON body.

```json
{
  "Type": "linuxdvb",
  "Url": "/etc/ferrofin/channels.conf",
  "FriendlyName": "Local antenna"
}
```

Run **Refresh Guide** to load the channels and match your existing XMLTV
provider. Linux DVB channels have their own IDs. Before replacing an existing
M3U tuner, re-create or migrate recording rules that reference its channel IDs.
Keep the configured lineup path stable to preserve the Linux DVB channel IDs.
