//! Local Linux DVB capture using the dvbv5-zap utility from v4l-utils.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::live_tv::TunerHostInfo;
use ferrofin_traits::error::ServiceError;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio::process::{Child, ChildStdout, Command};

use crate::stream::{self, TunerStreamBody};
use crate::tuner_host::{ChannelStream, ChannelStreamKind, StoredChannel, TunerChannel, TunerHost};

const DEVICE_ROOT: &str = "/dev/dvb";
const TUNE_SECTION: &str = "ferrofin";
const LINEUP_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct LinuxDvbHost {
    in_use: Arc<Mutex<BTreeSet<u32>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Selects one station from a configured dvbv5 lineup.
pub struct DvbStreamPlan {
    lineup: PathBuf,
    number: String,
}

struct Channel<'a> {
    name: &'a str,
    section: &'a str,
    service: u16,
}

fn parse_lineup(input: &str) -> Result<BTreeMap<&str, Channel<'_>>, ServiceError> {
    let mut result = BTreeMap::new();
    let section_header = |line: &str| {
        let line = line.trim();
        let header = line.strip_prefix(['#', ';']).unwrap_or(line).trim_start();
        header.starts_with('[') && header.ends_with(']')
    };
    let offsets: Vec<_> = std::iter::once(0)
        .chain(input.match_indices('\n').map(|(index, _)| index + 1))
        .filter(|&offset| input[offset..].lines().next().is_some_and(section_header))
        .collect();
    for (start, end) in offsets
        .iter()
        .copied()
        .zip(offsets.iter().copied().skip(1).chain([input.len()]))
    {
        let section = &input[start..end];
        let mut lines = section.lines();
        let header = lines.next().unwrap_or_default().trim();
        if header.starts_with(['#', ';']) {
            continue;
        }
        let name = &header[1..header.len() - 1];
        let fields: BTreeMap<_, _> = lines
            .filter(|line| !line.trim_start().starts_with(['#', ';']))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim(), value.trim()))
            .collect();
        let (Some(&number), Some(&frequency), Some(&service)) = (
            fields.get("VCHANNEL"),
            fields.get("FREQUENCY"),
            fields.get("SERVICE_ID"),
        ) else {
            continue;
        };
        let service = service
            .strip_prefix("0x")
            .or_else(|| service.strip_prefix("0X"))
            .map_or_else(|| service.parse(), |hex| u16::from_str_radix(hex, 16));
        if number.is_empty() || frequency.parse::<std::num::NonZeroU64>().is_err() {
            return Err(ServiceError::invalid_input(
                "Invalid DVB channel number or frequency",
            ));
        }
        let service = service.map_err(|_| ServiceError::invalid_input("Invalid DVB service ID"))?;
        if result
            .insert(
                number,
                Channel {
                    name,
                    section,
                    service,
                },
            )
            .is_some()
        {
            return Err(ServiceError::invalid_input(format!(
                "Duplicate DVB channel number {number}"
            )));
        }
    }
    if result.is_empty() {
        return Err(ServiceError::invalid_input(
            "No DVB channels with VCHANNEL, FREQUENCY and SERVICE_ID",
        ));
    }
    Ok(result)
}

fn lineup_error(error: std::io::Error) -> ServiceError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ServiceError::not_found(format!("DVB lineup: {error}"))
    } else {
        ServiceError::backend_source(error)
    }
}

async fn read_lineup(path: &Path) -> Result<String, ServiceError> {
    let metadata = tokio::fs::metadata(path).await.map_err(lineup_error)?;
    if !metadata.is_file() || metadata.len() > LINEUP_LIMIT {
        return Err(ServiceError::invalid_input(
            "DVB lineup must be a regular file no larger than 1 MiB",
        ));
    }
    let mut text = String::new();
    tokio::fs::File::open(path)
        .await
        .map_err(lineup_error)?
        .take(LINEUP_LIMIT + 1)
        .read_to_string(&mut text)
        .await
        .map_err(lineup_error)?;
    if text.len() as u64 > LINEUP_LIMIT {
        return Err(ServiceError::invalid_input("DVB lineup exceeds 1 MiB"));
    }
    Ok(text)
}

fn lineup_path(tuner: &TunerHostInfo) -> Result<&Path, ServiceError> {
    tuner
        .url
        .as_deref()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            ServiceError::invalid_input("Linux DVB requires an absolute dvbv5 lineup file path")
        })
}

fn source_for(tuner: &TunerHostInfo, channel: &StoredChannel) -> MediaSourceInfo {
    let mut source = stream::create_media_source_info(&channel.external_id, tuner, false);
    source.path = None;
    source.path_is_private = true;
    source.container = Some("ts".to_owned());
    source.supports_direct_play = false;
    source.supports_direct_stream = true;
    source.requires_looping = false;
    source
}

#[async_trait]
impl TunerHost for LinuxDvbHost {
    fn name(&self) -> &'static str {
        "Linux DVB"
    }

    fn type_id(&self) -> &'static str {
        "linuxdvb"
    }

    async fn validate(&self, tuner: &mut TunerHostInfo) -> Result<(), ServiceError> {
        let path = tokio::fs::canonicalize(lineup_path(tuner)?)
            .await
            .map_err(lineup_error)?;
        parse_lineup(&read_lineup(&path).await?)?;
        tuner.url = Some(path.to_string_lossy().into_owned());
        Ok(())
    }

    async fn get_channels(&self, tuner: &TunerHostInfo) -> Result<Vec<TunerChannel>, ServiceError> {
        let path = lineup_path(tuner)?;
        let text = read_lineup(path).await?;
        let lineup_id = ferrofin_common::extensions::get_md5(&path.to_string_lossy()).simple();
        Ok(parse_lineup(&text)?
            .into_iter()
            .map(|(number, channel)| TunerChannel {
                external_id: format!("linuxdvb_{lineup_id}_{number}"),
                guide_key: number.to_owned(),
                name: channel.name.to_owned(),
                number: Some(number.to_owned()),
                // An internal selector for this host; never handed to a client as a URL.
                url: number.to_owned(),
                ..TunerChannel::default()
            })
            .collect())
    }

    async fn channel_media_sources(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(vec![source_for(tuner, channel)])
    }

    async fn channel_stream(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
        _stream_id: Option<&str>,
    ) -> Result<ChannelStream, ServiceError> {
        Ok(ChannelStream {
            source: source_for(tuner, channel),
            kind: ChannelStreamKind::LinuxDvb(DvbStreamPlan {
                lineup: lineup_path(tuner)?.to_owned(),
                number: channel.path.clone(),
            }),
        })
    }
}

impl LinuxDvbHost {
    pub(crate) async fn open_body(
        &self,
        plan: &DvbStreamPlan,
    ) -> Result<Box<dyn TunerStreamBody>, ServiceError> {
        let lineup = read_lineup(&plan.lineup).await?;
        let channels = parse_lineup(&lineup)?;
        let channel = channels
            .get(plan.number.as_str())
            .ok_or_else(|| ServiceError::not_found(format!("DVB channel {}", plan.number)))?;
        let config = write_tune_config(channel)?;
        let lease = self.acquire_adapter(Path::new(DEVICE_ROOT)).await?;
        let mut child = Command::new("dvbv5-zap")
            .arg("-c")
            .arg(config.path())
            .arg("-a")
            .arg(lease.adapter.to_string())
            .args(["-s", "-p", "-o", "-", TUNE_SECTION])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(ServiceError::backend_source)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ServiceError::backend("Missing dvbv5-zap stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ServiceError::backend("Missing dvbv5-zap stderr"))?;
        let adapter = lease.adapter;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let message = line.trim();
                if message.starts_with("ERROR") || message.starts_with("WARNING") {
                    tracing::warn!(adapter, %message, "live tv: dvbv5-zap diagnostic");
                } else if !message.is_empty() {
                    tracing::debug!(adapter, %message, "live tv: dvbv5-zap status");
                }
            }
        });
        Ok(Box::new(DvbBody {
            child: Some(child),
            stdout,
            lease: Some(lease),
            config: Some(config),
        }))
    }
}

fn write_tune_config(channel: &Channel<'_>) -> Result<tempfile::NamedTempFile, ServiceError> {
    use std::io::Write as _;

    let mut config = tempfile::NamedTempFile::new().map_err(ServiceError::backend_source)?;
    writeln!(config, "[{TUNE_SECTION}]\nSERVICE_ID = {}", channel.service)
        .map_err(ServiceError::backend_source)?;
    for line in channel.section.lines().skip(1) {
        if !line.trim_start().starts_with(['#', ';'])
            && line
                .split_once('=')
                .is_none_or(|(key, _)| key.trim() != "SERVICE_ID")
        {
            writeln!(config, "{line}").map_err(ServiceError::backend_source)?;
        }
    }
    Ok(config)
}

impl LinuxDvbHost {
    async fn acquire_adapter(&self, device_root: &Path) -> Result<AdapterLease, ServiceError> {
        let mut adapters = tokio::fs::read_dir(device_root)
            .await
            .map_err(ServiceError::backend_source)?;
        let mut available = BTreeSet::new();
        while let Some(entry) = adapters
            .next_entry()
            .await
            .map_err(ServiceError::backend_source)?
        {
            let name = entry.file_name();
            let Some(adapter) = name
                .to_str()
                .and_then(|name| name.strip_prefix("adapter"))
                .and_then(|n| n.parse().ok())
            else {
                continue;
            };
            let path = entry.path();
            if tokio::fs::try_exists(path.join("frontend0"))
                .await
                .unwrap_or(false)
                && tokio::fs::try_exists(path.join("demux0"))
                    .await
                    .unwrap_or(false)
                && tokio::fs::try_exists(path.join("dvr0"))
                    .await
                    .unwrap_or(false)
            {
                available.insert(adapter);
            }
        }
        let mut in_use = self
            .in_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let adapter = available
            .difference(&in_use)
            .next()
            .copied()
            .ok_or_else(|| ServiceError::Conflict("No free Linux DVB adapter".to_owned()))?;
        in_use.insert(adapter);
        Ok(AdapterLease {
            adapter,
            in_use: Arc::clone(&self.in_use),
        })
    }
}

struct AdapterLease {
    adapter: u32,
    in_use: Arc<Mutex<BTreeSet<u32>>>,
}

impl Drop for AdapterLease {
    fn drop(&mut self) {
        self.in_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.adapter);
    }
}

struct DvbBody {
    child: Option<Child>,
    stdout: ChildStdout,
    lease: Option<AdapterLease>,
    config: Option<tempfile::NamedTempFile>,
}

#[async_trait]
impl TunerStreamBody for DvbBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        let mut data = vec![0; 64 * 1024];
        let count = tokio::time::timeout(Duration::from_secs(30), self.stdout.read(&mut data))
            .await
            .map_err(|_| ServiceError::backend("Linux DVB capture timed out"))?
            .map_err(ServiceError::backend_source)?;
        if count == 0
            && let Some(child) = self.child.as_mut()
            && let Some(status) = child.try_wait().map_err(ServiceError::backend_source)?
            && !status.success()
        {
            return Err(ServiceError::backend(format!(
                "dvbv5-zap exited with {status}"
            )));
        }
        data.truncate(count);
        Ok((count != 0).then_some(data))
    }
}

impl Drop for DvbBody {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let lease = self.lease.take();
            let config = self.config.take();
            let _ = child.start_kill();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                    drop((lease, config));
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_service_config_with_duplicate_station_names() {
        let input = "[WABC]\nVCHANNEL=7.1\nFREQUENCY=123\nSERVICE_ID=1\nVIDEO_PID=49\n\n[WABC]\nVCHANNEL=7.2\nFREQUENCY=123\nSERVICE_ID=0x02\nVIDEO_PID=65\n";
        let channels = parse_lineup(input).unwrap();
        let config = write_tune_config(&channels["7.2"]).unwrap();
        let content = std::fs::read_to_string(config.path()).unwrap();
        assert_eq!(
            content,
            "[ferrofin]\nSERVICE_ID = 2\nVCHANNEL=7.2\nFREQUENCY=123\nVIDEO_PID=65\n"
        );
        assert!(parse_lineup(&format!("{input}{input}")).is_err());
    }
    const LINEUP: &str = "[WABC]\nVCHANNEL=7.1\nFREQUENCY=177000000\nSERVICE_ID=1\nVIDEO_PID=49\n";

    #[test]
    fn commented_sections_and_fields_cannot_change_the_active_service() {
        let input = format!(
            "{LINEUP}# FREQUENCY=99\n; VIDEO_PID=42\n# [DISABLED]\nVCHANNEL=9.1\nFREQUENCY=123\nSERVICE_ID=9\n; [DISABLED AGAIN]\nVCHANNEL=10.1\nFREQUENCY=456\nSERVICE_ID=10\n[OTHER]\nVCHANNEL=7.2\nFREQUENCY=177000000\nSERVICE_ID=2\n"
        );
        let channels = parse_lineup(&input).unwrap();
        assert_eq!(channels.keys().copied().collect::<Vec<_>>(), ["7.1", "7.2"]);
        let file = write_tune_config(&channels["7.1"]).unwrap();
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "[ferrofin]\nSERVICE_ID = 1\nVCHANNEL=7.1\nFREQUENCY=177000000\nVIDEO_PID=49\n"
        );
    }

    #[test]
    fn rejects_invalid_service_fields_and_empty_lineups() {
        for input in [
            LINEUP.replace("177000000", "0"),
            LINEUP.replace("SERVICE_ID=1", "SERVICE_ID=65536"),
            LINEUP.replace("VCHANNEL=7.1", "# VCHANNEL=7.1"),
            String::new(),
        ] {
            assert!(matches!(
                parse_lineup(&input),
                Err(ServiceError::InvalidInput(_))
            ));
        }
    }

    #[tokio::test]
    async fn validates_and_imports_a_canonical_lineup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channels.conf");
        std::fs::write(&path, LINEUP).unwrap();
        let alias = dir.path().join("alias.conf");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let host = LinuxDvbHost::default();
        let mut tuner = TunerHostInfo {
            url: Some(alias.to_str().unwrap().to_owned()),
            ..TunerHostInfo::default()
        };
        host.validate(&mut tuner).await.unwrap();
        assert_eq!(lineup_path(&tuner).unwrap(), path.canonicalize().unwrap());
        let channel = host.get_channels(&tuner).await.unwrap().pop().unwrap();
        assert_eq!(channel.number.as_deref(), Some("7.1"));
        assert_eq!(channel.guide_key, "7.1");
        let stored = StoredChannel {
            external_id: channel.external_id,
            path: channel.url,
            ..StoredChannel::default()
        };
        let media = host
            .channel_media_sources(&tuner, &stored)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(media.requires_opening && media.requires_closing);
        assert_eq!(media.container.as_deref(), Some("ts"));
        assert!(media.path.is_none());

        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            host.validate(&mut tuner).await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            host.get_channels(&tuner).await,
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn only_complete_adapters_can_be_leased_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        for adapter in [0, 2, 3] {
            let device = dir.path().join(format!("adapter{adapter}"));
            std::fs::create_dir(&device).unwrap();
            for node in ["frontend0", "demux0", "dvr0"] {
                if adapter != 0 || node != "dvr0" {
                    std::fs::File::create(device.join(node)).unwrap();
                }
            }
        }
        let host = LinuxDvbHost::default();
        let first = host.acquire_adapter(dir.path()).await.unwrap();
        let second = host.acquire_adapter(dir.path()).await.unwrap();
        assert_eq!((first.adapter, second.adapter), (2, 3));
        assert!(matches!(
            host.acquire_adapter(dir.path()).await,
            Err(ServiceError::Conflict(_))
        ));
        drop(first);
        assert_eq!(host.acquire_adapter(dir.path()).await.unwrap().adapter, 2);
    }
}
