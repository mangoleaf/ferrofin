# Music artwork

A library scan stores the same Primary images for an album and its tracks as
Jellyfin does.

## Tracks

A track has a Primary image only when its own file embeds a picture
(Jellyfin's `AudioImageProvider`). The picture whose comment names the front
cover is taken, then one naming a cover, then the first. It is stored in the
track's own art folder under the metadata directory, never beside the media.

A track without an embedded picture has no image of its own. Clients show its
album's cover through the track's `AlbumId` and `AlbumPrimaryImageTag`, as they
do on Jellyfin.

The picture is extracted once per album, `Album` tag and first album artist,
as Jellyfin shares it: the other tracks with all three and a picture copy that
file, and its dimensions and blurhash, instead of running ffmpeg again. A track
whose `Album` tag or album artist is blank extracts its own. The album is the
track's `MusicAlbum` item, found by id (multi-disc descendants included), never
by title, so two albums with the same name do not share a cover. A track whose
copy is already on disk reuses it.
An audiobook with an embedded picture gets it the same way, extracted for
itself.

## Albums

An album with an image beside it (`folder.jpg`, `cover.jpg` and the other names
Jellyfin's local image provider reads) names that file in place. An album with
an uploaded or downloaded image in its own art folder keeps it.

An album with neither takes a copy of its first track's cover (files before
folders, then by sort name) in its own art folder (Jellyfin's
`MusicAlbumImageProvider`). The copy is made by the scan's closing dynamic image
pass, whatever "Save artwork into media folders" says: nothing is written into
the album's media folder. It is not drawn again when its file's date changes,
only on a refresh that runs every provider, and an image that is not the
album's own (a `folder.jpg` beside it) is never replaced.

Uploading an image to a track or an album stores it as that item's own image.
An album's upload leaves its tracks' covers alone.

## Rows earlier builds stored

Earlier builds stored every album's cover as a shared copy in
`<metadata-library>/album-covers/<album-id>/<sha256>.<extension>`, and named
that copy as the album's Primary and as every track's, with or without an
embedded picture. No build writes there any more.

The next library scan replaces each such row with the image the item gets
above: a track without a picture loses its Primary, a track with one gets its
own copy, and an album names its `folder.jpg` or takes its copy. No migration
or one-shot repair is needed. A previous version still running beside the new
one during a rolling update may write such rows again; the new version's next
scan replaces them again. A locked album or track keeps such a row, as its lock
keeps the stored row (and refuses an album's copy).

The files in `album-covers/` stay on disk. Deleting an image never removes one,
as an album's rows not yet scanned again may still name it.
