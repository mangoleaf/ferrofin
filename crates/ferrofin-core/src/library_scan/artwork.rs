//! Automatic acquisition: per-type limits, width checks and lazy replacement.
use super::{ItemImageInfo, RemoteImage, TmdbClient, file_date_modified};
use ferrofin_model::entities::ImageType;
use ferrofin_providers::image_policy::ImageAcquisitionPolicy;
use std::path::{Path, PathBuf};

/// Download one provider's candidates; callers pass the images already kept
/// or acquired so later providers fill only the remaining capacity.
pub(super) async fn acquire_images(
    tmdb: &TmdbClient,
    directory: &Path,
    candidates: Vec<RemoteImage>,
    policy: ImageAcquisitionPolicy<'_>,
    current: &[ItemImageInfo],
    replacing: bool,
) -> Vec<ItemImageInfo> {
    use ferrofin_providers::ArtworkDownloadFailure;
    let mut images = Vec::new();
    let mut stopped = std::collections::HashSet::new();
    let mut candidates = candidates;
    // Upstream prefers backdrops without language even over the requested one.
    candidates.sort_by_key(|image| {
        image.image_type == ImageType::Backdrop
            && image
                .language
                .as_deref()
                .is_some_and(|language| !language.is_empty())
    });
    for candidate in candidates {
        let kind = candidate.image_type;
        let count = current
            .iter()
            .chain(&images)
            .filter(|image| image.image_type == kind)
            .count();
        if stopped.contains(&kind)
            || count >= policy.limit(kind)
            || !policy.accepts(kind, candidate.width)
        {
            continue;
        }
        let download = match tmdb.download_artwork(&candidate.url).await {
            Ok(download) => download,
            Err(ArtworkDownloadFailure::Skip) => continue,
            Err(ArtworkDownloadFailure::Stop) => {
                stopped.insert(kind);
                continue;
            }
        };
        if kind == ImageType::Backdrop
            && !replacing
            && download.content_length.is_some_and(|length| {
                current
                    .iter()
                    .chain(&images)
                    .filter(|image| image.image_type == kind)
                    .any(|image| {
                        std::fs::metadata(&image.path)
                            .is_ok_and(|metadata| metadata.len() == length)
                    })
            })
        {
            continue;
        }
        match write_acquired_image(directory, policy.kind(), kind, &download) {
            Ok(image) => images.push(image),
            Err(err) => {
                tracing::warn!(%err, directory = %directory.display(), "failed to write acquired artwork");
            }
        }
    }
    images
}

/// Backdrops use a free slot so a failed replacement cannot destroy an old
/// image. Singular images are atomically replaced only after a full download.
/// The file is named for an item of `item_kind`
/// ([`internal_image_stem`](ferrofin_providers::local_images::internal_image_stem)).
pub(super) fn write_acquired_image(
    directory: &Path,
    item_kind: &str,
    kind: ImageType,
    download: &ferrofin_providers::ArtworkDownload,
) -> std::io::Result<ItemImageInfo> {
    let base = ferrofin_providers::local_images::internal_image_stem(item_kind, kind);
    let ext = ferrofin_model::net::mime_types::to_extension(&download.content_type)
        .unwrap_or(".jpg")
        .trim_start_matches('.');
    let stem = if kind == ImageType::Backdrop {
        unused_backdrop_stem(directory)
    } else {
        base
    };
    let dest = directory.join(format!("{stem}.{ext}"));
    std::fs::create_dir_all(directory)?;
    ferrofin_util::file_helper::atomic_write(&dest, &download.bytes)?;
    if kind != ImageType::Backdrop {
        for other in super::ART_FILE_EXTENSIONS {
            let old = directory.join(format!("{stem}.{other}"));
            if old != dest {
                let _ = std::fs::remove_file(old);
            }
        }
    }
    Ok(ItemImageInfo {
        date_modified: file_date_modified(&dest),
        path: dest.to_string_lossy().into_owned(),
        image_type: kind,
        width: 0,
        height: 0,
        blur_hash: None,
    })
}

fn unused_backdrop_stem(directory: &Path) -> String {
    let mut index = 0_u64;
    loop {
        let stem = if index == 0 {
            "backdrop".to_owned()
        } else {
            format!("backdrop{index}")
        };
        if super::existing_art_file(directory, &stem).is_none() {
            return stem;
        }
        index += 1;
    }
}

/// Under "Replace existing images", once the item keeps an image of a type in
/// one of its art `directories`, removes the other files of that type in any
/// of them that the caller did not retain: the backdrops a refresh replaced,
/// and a singular type's file an earlier build saved under its own name in
/// the `{ID}` or `{id}` folder, which would otherwise come back once the new
/// image is deleted (`ImageSaver.SaveImage`'s "Delete the current path"). The
/// directories are judged together, as a refresh writes to the item's own
/// folder only. This never deletes a media sidecar.
pub(super) fn prune_replaced_in(directories: &[PathBuf], current: &[ItemImageInfo]) {
    let kept: Vec<PathBuf> = current
        .iter()
        .map(|image| PathBuf::from(&image.path))
        .collect();
    let replaced: std::collections::HashSet<ImageType> = current
        .iter()
        .filter(|image| {
            directories
                .iter()
                .any(|directory| Path::new(&image.path).starts_with(directory))
        })
        .map(|image| image.image_type)
        .collect();
    if replaced.is_empty() {
        return;
    }
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if super::parse_art_file_stem(&path).is_some_and(|kind| replaced.contains(&kind))
                && !kept.contains(&path)
                && let Err(err) = std::fs::remove_file(&path)
            {
                tracing::warn!(%err, path = %path.display(), "failed to prune a replaced image");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::configuration::{ImageOption, LibraryOptions, TypeOptions};
    use std::sync::{Arc, Mutex};

    /// The body served for `path`: enough of an image to be kept, and which
    /// one it is.
    fn image(path: &str) -> Vec<u8> {
        format!("GIF89a{path}").into_bytes()
    }

    fn server() -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{Read as _, Write as _};
        // Images are served on loopback, which downloads refuse by default.
        ferrofin_providers::destination::test_support::allow_loopback_images();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..count]);
                let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
                seen.lock().unwrap().push(path.clone());
                let status = match path.as_str() {
                    "/forbidden" => "403 Forbidden",
                    "/missing" => "404 Not Found",
                    "/error" => "500 Internal Server Error",
                    _ => "200 OK",
                };
                let body = if path == "/duplicate" {
                    image("/first")
                } else {
                    image(&path)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        (format!("http://{address}"), requests)
    }

    fn candidate(base: &str, path: &str, image_type: ImageType, width: Option<i32>) -> RemoteImage {
        RemoteImage {
            url: format!("{base}{path}"),
            image_type,
            width,
            language: None,
        }
    }

    fn options(primary: i32, backdrops: i32) -> LibraryOptions {
        LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("Movie".to_owned()),
                image_options: vec![
                    ImageOption {
                        type_: ImageType::Primary,
                        limit: primary,
                        min_width: 600,
                    },
                    ImageOption {
                        type_: ImageType::Backdrop,
                        limit: backdrops,
                        min_width: 1280,
                    },
                    ImageOption {
                        type_: ImageType::Disc,
                        limit: 1,
                        min_width: 0,
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn stored(path: &Path, image_type: ImageType) -> ItemImageInfo {
        ItemImageInfo {
            path: path.to_string_lossy().into_owned(),
            image_type,
            date_modified: file_date_modified(path),
            width: 0,
            height: 0,
            blur_hash: None,
        }
    }

    #[tokio::test]
    async fn limits_widths_language_and_duplicate_backdrops_control_downloads() {
        let (base, requests) = server();
        let tmp = tempfile::tempdir().unwrap();
        let library = options(0, 2);
        let mut tagged = candidate(&base, "/tagged", ImageType::Backdrop, Some(1920));
        tagged.language = Some("en".to_owned());
        let images = acquire_images(
            &TmdbClient::new(),
            tmp.path(),
            vec![
                tagged,
                candidate(&base, "/disabled", ImageType::Primary, None),
                candidate(&base, "/small", ImageType::Backdrop, Some(1279)),
                candidate(&base, "/first", ImageType::Backdrop, Some(1280)),
                candidate(&base, "/duplicate", ImageType::Backdrop, Some(1920)),
                candidate(&base, "/unknown-width", ImageType::Backdrop, None),
                candidate(&base, "/past-limit", ImageType::Backdrop, Some(1920)),
            ],
            ImageAcquisitionPolicy::new(Some(&library), "Movie"),
            &[],
            false,
        )
        .await;
        assert_eq!(images.len(), 2);
        assert_eq!(
            *requests.lock().unwrap(),
            ["/first", "/duplicate", "/unknown-width"]
        );
        assert_eq!(std::fs::read(&images[0].path).unwrap(), image("/first"));
        assert_eq!(
            std::fs::read(&images[1].path).unwrap(),
            image("/unknown-width")
        );
        let more = acquire_images(
            &TmdbClient::new(),
            tmp.path(),
            vec![candidate(&base, "/full", ImageType::Backdrop, None)],
            ImageAcquisitionPolicy::new(Some(&library), "Movie"),
            &images,
            false,
        )
        .await;
        assert!(more.is_empty(), "{more:?}");
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn singular_types_have_separate_files_and_a_limit_above_one_still_means_one() {
        let (base, requests) = server();
        let tmp = tempfile::tempdir().unwrap();
        let library = options(3, 0);
        let images = acquire_images(
            &TmdbClient::new(),
            tmp.path(),
            vec![
                candidate(&base, "/small-primary", ImageType::Primary, Some(599)),
                candidate(&base, "/poster", ImageType::Primary, None),
                candidate(&base, "/second-poster", ImageType::Primary, Some(1000)),
                candidate(&base, "/disc", ImageType::Disc, None),
            ],
            ImageAcquisitionPolicy::new(Some(&library), "Movie"),
            &[],
            false,
        )
        .await;
        assert_eq!(images.len(), 2);
        assert_eq!(*requests.lock().unwrap(), ["/poster", "/disc"]);
        assert_eq!(
            std::fs::read(tmp.path().join("poster.png")).unwrap(),
            image("/poster")
        );
        assert_eq!(
            std::fs::read(tmp.path().join("disc.png")).unwrap(),
            image("/disc")
        );
    }

    #[tokio::test]
    async fn bad_urls_skip_but_other_failures_stop_that_providers_type_pass() {
        // Every retry of `/error` is still sent; the limiter's own suite
        // tests the backoff between them.
        ferrofin_providers::rate_limit::test_support::skip_backoff();
        let (base, requests) = server();
        let tmp = tempfile::tempdir().unwrap();
        let policy = ImageAcquisitionPolicy::new(None, "Person");
        let tmdb = TmdbClient::new();
        let images = acquire_images(
            &tmdb,
            tmp.path(),
            vec![
                candidate(&base, "/forbidden", ImageType::Primary, None),
                candidate(&base, "/missing", ImageType::Primary, None),
                candidate(&base, "/error", ImageType::Primary, None),
                candidate(&base, "/should-not-run", ImageType::Primary, None),
            ],
            policy,
            &[],
            true,
        )
        .await;
        assert!(images.is_empty(), "{images:?}");
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|path| path == "/should-not-run")
        );
        let next = acquire_images(
            &tmdb,
            tmp.path(),
            vec![candidate(&base, "/fallback", ImageType::Primary, None)],
            policy,
            &images,
            true,
        )
        .await;
        assert_eq!(std::fs::read(&next[0].path).unwrap(), image("/fallback"));
    }

    #[tokio::test]
    async fn backdrop_replacement_is_lazy_and_existing_sidecars_count_toward_the_limit() {
        let (base, _) = server();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("metadata");
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("backdrop.jpg");
        std::fs::write(&old, b"old").unwrap();
        let library = options(1, 2);
        let policy = ImageAcquisitionPolicy::new(Some(&library), "Movie");
        let tmdb = TmdbClient::new();
        let failed = acquire_images(
            &tmdb,
            &dir,
            vec![candidate(&base, "/missing", ImageType::Backdrop, None)],
            policy,
            &[],
            true,
        )
        .await;
        prune_replaced_in(std::slice::from_ref(&dir), &failed);
        assert_eq!(std::fs::read(&old).unwrap(), b"old");
        let local = tmp.path().join("sidecar.png");
        std::fs::write(&local, b"sidecar").unwrap();
        let mut current = vec![stored(&local, ImageType::Backdrop)];
        current.extend(
            acquire_images(
                &tmdb,
                &dir,
                vec![
                    candidate(&base, "/new", ImageType::Backdrop, None),
                    candidate(&base, "/unneeded", ImageType::Backdrop, None),
                ],
                policy,
                &current,
                true,
            )
            .await,
        );
        assert_eq!(current.len(), 2);
        assert!(old.exists(), "old files remain until acquisition succeeds");
        prune_replaced_in(std::slice::from_ref(&dir), &current);
        assert!(!old.exists());
        assert_eq!(std::fs::read(local).unwrap(), b"sidecar");
        assert_eq!(std::fs::read(&current[1].path).unwrap(), image("/new"));
    }

    /// An item's folders are pruned together: once the own folder keeps a new
    /// backdrop, an old one in the legacy folder goes; with no kept backdrop
    /// in any of them, nothing is pruned.
    #[test]
    fn backdrops_are_pruned_across_the_items_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let own = tmp.path().join("OWN");
        let legacy = tmp.path().join("own");
        for dir in [&own, &legacy] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let new = own.join("backdrop.jpg");
        let old = legacy.join("backdrop.jpg");
        std::fs::write(&new, b"new").unwrap();
        std::fs::write(&old, b"old").unwrap();
        let folders = [own, legacy];
        prune_replaced_in(&folders, &[]);
        assert!(
            old.exists() && new.exists(),
            "no kept backdrop: nothing pruned"
        );
        prune_replaced_in(&folders, &[stored(&new, ImageType::Backdrop)]);
        assert!(new.exists());
        assert!(
            !old.exists(),
            "the replaced backdrop in the legacy folder goes"
        );
    }

    /// A singular type's file an earlier build saved in the `{ID}` folder
    /// under its own name goes once the refresh keeps a new one in the own
    /// folder; a type the refresh kept nothing of in the folders stays, and
    /// so does every kept file.
    #[test]
    fn a_replaced_earlier_singular_image_is_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let own = tmp.path().join("0c").join("0c6e");
        let earlier = tmp.path().join("0C6E");
        for dir in [&own, &earlier] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let poster = own.join("poster.jpg");
        let old_primary = earlier.join("primary.jpg");
        let old_logo = earlier.join("logo.png");
        let kept_thumb = earlier.join("thumb.jpg");
        for file in [&poster, &old_primary, &old_logo, &kept_thumb] {
            std::fs::write(file, b"x").unwrap();
        }
        let media = tmp.path().join("media-logo.png");
        prune_replaced_in(
            &[own, earlier],
            &[
                stored(&poster, ImageType::Primary),
                stored(&kept_thumb, ImageType::Thumb),
                stored(&media, ImageType::Logo),
            ],
        );
        assert!(poster.exists() && kept_thumb.exists());
        assert!(!old_primary.exists(), "the replaced Primary goes");
        assert!(old_logo.exists(), "no Logo kept in the folders: it stays");
    }
}
