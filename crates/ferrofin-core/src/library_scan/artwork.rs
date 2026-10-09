//! Automatic acquisition: per-type limits, width checks and lazy replacement.
use super::{ItemImageInfo, RemoteImage, TmdbClient, file_date_modified, image_type_file_stem};
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
        match write_acquired_image(directory, kind, &download) {
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
fn write_acquired_image(
    directory: &Path,
    kind: ImageType,
    download: &ferrofin_providers::ArtworkDownload,
) -> std::io::Result<ItemImageInfo> {
    let base = image_type_file_stem(kind);
    let ext = ferrofin_model::net::mime_types::to_extension(&download.content_type)
        .unwrap_or(".jpg")
        .trim_start_matches('.');
    let stem = if kind == ImageType::Backdrop {
        unused_backdrop_stem(directory)
    } else {
        base.to_owned()
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

/// Once at least one new backdrop exists, remove old internal backdrops that
/// the caller did not retain. This never deletes a media sidecar.
pub(super) fn prune_old_backdrops(directory: &Path, current: &[ItemImageInfo]) {
    let kept: Vec<PathBuf> = current
        .iter()
        .filter(|image| image.image_type == ImageType::Backdrop)
        .map(|image| PathBuf::from(&image.path))
        .collect();
    if !kept.iter().any(|path| path.starts_with(directory)) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if super::parse_art_file_stem(&path) == Some(ImageType::Backdrop)
            && !kept.contains(&path)
            && let Err(err) = std::fs::remove_file(&path)
        {
            tracing::warn!(%err, path = %path.display(), "failed to prune replaced backdrop");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::configuration::{ImageOption, LibraryOptions, TypeOptions};
    use std::sync::{Arc, Mutex};

    fn server() -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{Read as _, Write as _};
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
                    b"/first".as_slice()
                } else {
                    path.as_bytes()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(body);
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
        assert_eq!(std::fs::read(&images[0].path).unwrap(), b"/first");
        assert_eq!(std::fs::read(&images[1].path).unwrap(), b"/unknown-width");
        let more = acquire_images(
            &TmdbClient::new(),
            tmp.path(),
            vec![candidate(&base, "/full", ImageType::Backdrop, None)],
            ImageAcquisitionPolicy::new(Some(&library), "Movie"),
            &images,
            false,
        )
        .await;
        assert!(more.is_empty());
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
            std::fs::read(tmp.path().join("primary.png")).unwrap(),
            b"/poster"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("disc.png")).unwrap(),
            b"/disc"
        );
    }

    #[tokio::test]
    async fn bad_urls_skip_but_other_failures_stop_that_providers_type_pass() {
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
        assert!(images.is_empty());
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
        assert_eq!(std::fs::read(&next[0].path).unwrap(), b"/fallback");
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
        prune_old_backdrops(&dir, &failed);
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
        prune_old_backdrops(&dir, &current);
        assert!(!old.exists());
        assert_eq!(std::fs::read(local).unwrap(), b"sidecar");
        assert_eq!(std::fs::read(&current[1].path).unwrap(), b"/new");
    }
}
