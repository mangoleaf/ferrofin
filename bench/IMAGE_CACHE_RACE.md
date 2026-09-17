# Bug: concurrent cold-cache poster requests can return empty images

Status: open server bug, recorded locally for review; not submitted upstream.

In `bench/runs/noisy-preview`, the 600-slot shape scenario used ten VUs against fresh
server caches. Jellyfin 12 returned three empty image bodies; Ferrofin returned four
(including one transport failure). Slot 3 contains HTTP 200 `image/jpeg` responses with
zero bytes on both servers. Later requests for those cache keys returned non-empty JPEGs.
No slot contained duplicate image URLs within its own batch in that recording.

## Cause supported by code and observations

`crates/ferrofin-drawing/src/processor.rs`, `encode_to_cache`, tests existence and then
encodes straight to the final cache filename. `image_encoder.rs`, `write_jpeg`, creates
that file before its buffered encoder finishes. Another request can observe an existing
but incomplete file, or a competing writer can truncate the final file during a read.

Jellyfin 12's `src/Jellyfin.Drawing/ImageProcessor.cs` also checks `File.Exists` before
encoding. Its encoding semaphore does not protect a cache-hit reader. The Skia encoder
opens `SKFileWStream(outputPath)` directly on the final path. This is the same unsafe
publication pattern; matching the upstream bug is not a correctness requirement.
The exact interleaving in the saved requests was not traced.

## Reproduce

Use a disposable benchmark fixture and fresh image cache. Issue concurrent GETs for the
same primary poster and resize options (`fillHeight=300&fillWidth=200&quality=96`).
Repeat with fresh caches and record status, content type, bytes, and decode success.
The existing `SHAPE_VUS=10` scenario is a recorded reproduction; serialize with
`SHAPE_VUS=1` to avoid overlapping first encodes across screens during validation.
Keep these failures visible in archived reports. Do not suppress zero-byte checks.

## Proposed server fix and acceptance

Encode into a unique temporary file in the same directory and atomically publish only
when encoding and flushing succeed. Clean up temporary files on failure. Consider
per-key coordination to avoid duplicate encodes, without serializing unrelated images.

Add a deterministic test that pauses a writer after opening its output and starts a
second request: readers must never receive partial output. Also cover competing writers,
encode failure, valid cache reuse, and multiple independent keys. Confirm complete,
decodable responses under a concurrent cold-cache live test before closing this bug.

The benchmark mitigation is not a fix for this server bug. No server code is changed
as part of the harness review repairs.
