//! Port of `Jellyfin.Api.Models.SystemInfoDtos`.
//!
//! These are the API-facing storage DTOs the `SystemController.GetSystemStorage`
//! action returns. They are a thin re-projection of the domain
//! [`SystemStorageInfo`](crate::system::SystemStorageInfo) /
//! [`FolderStorageInfo`](crate::system::FolderStorageInfo) /
//! [`LibraryStorageInfo`](crate::system::LibraryStorageInfo) types, dropping the
//! `ResolvedPath` field the DTO does not expose.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::system::{FolderStorageInfo, LibraryStorageInfo, SystemStorageInfo};

/// Contains information about a specific folder (API DTO).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct FolderStorageDto {
    /// Gets the path of the folder in question.
    pub path: String,

    /// Gets the free space of the underlying storage device of the path.
    pub free_space: i64,

    /// Gets the used space of the underlying storage device of the path.
    pub used_space: i64,

    /// Gets the kind of storage device of the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_type: Option<String>,

    /// Gets the device identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

impl FolderStorageDto {
    /// Projects a domain [`FolderStorageInfo`] to its API DTO
    /// (C# `FolderStorageDto.FromFolderStorageInfo`).
    #[must_use]
    pub fn from_folder_storage_info(info: FolderStorageInfo) -> Self {
        Self {
            path: info.path,
            free_space: info.free_space,
            used_space: info.used_space,
            storage_type: info.storage_type,
            device_id: info.device_id,
        }
    }
}

/// Contains information about a library's storage (API DTO).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryStorageDto {
    /// Gets or sets the library id.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the name of the library.
    pub name: String,

    /// Gets or sets the storage information about the folders used in a library.
    pub folders: Vec<FolderStorageDto>,
}

impl LibraryStorageDto {
    /// Projects a domain [`LibraryStorageInfo`] to its API DTO
    /// (C# `LibraryStorageDto.FromLibraryStorageInfo`).
    #[must_use]
    pub fn from_library_storage_info(info: LibraryStorageInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            folders: info
                .folders
                .into_iter()
                .map(FolderStorageDto::from_folder_storage_info)
                .collect(),
        }
    }
}

/// Contains information about the system's storage (API DTO).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SystemStorageDto {
    /// Gets or sets the storage information of the program data folder.
    pub program_data_folder: FolderStorageDto,

    /// Gets or sets the storage information of the web UI resources folder.
    pub web_folder: FolderStorageDto,

    /// Gets or sets the storage information of the folder where images are cached.
    pub image_cache_folder: FolderStorageDto,

    /// Gets or sets the storage information of the cache folder.
    pub cache_folder: FolderStorageDto,

    /// Gets or sets the storage information of the folder where logfiles are saved.
    pub log_folder: FolderStorageDto,

    /// Gets or sets the storage information of the folder where metadata is stored.
    pub internal_metadata_folder: FolderStorageDto,

    /// Gets or sets the storage information of the transcoding cache.
    pub transcoding_temp_folder: FolderStorageDto,

    /// Gets or sets the storage information of all libraries.
    pub libraries: Vec<LibraryStorageDto>,
}

impl SystemStorageDto {
    /// Projects a domain [`SystemStorageInfo`] to its API DTO
    /// (C# `SystemStorageDto.FromSystemStorageInfo`).
    #[must_use]
    pub fn from_system_storage_info(info: SystemStorageInfo) -> Self {
        Self {
            program_data_folder: FolderStorageDto::from_folder_storage_info(
                info.program_data_folder,
            ),
            web_folder: FolderStorageDto::from_folder_storage_info(info.web_folder),
            image_cache_folder: FolderStorageDto::from_folder_storage_info(info.image_cache_folder),
            cache_folder: FolderStorageDto::from_folder_storage_info(info.cache_folder),
            log_folder: FolderStorageDto::from_folder_storage_info(info.log_folder),
            internal_metadata_folder: FolderStorageDto::from_folder_storage_info(
                info.internal_metadata_folder,
            ),
            transcoding_temp_folder: FolderStorageDto::from_folder_storage_info(
                info.transcoding_temp_folder,
            ),
            libraries: info
                .libraries
                .into_iter()
                .map(LibraryStorageDto::from_library_storage_info)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FolderStorageInfo;

    use crate::system::{LibraryStorageInfo, SystemStorageInfo};

    fn sample_folder_info(path: &str) -> FolderStorageInfo {
        FolderStorageInfo {
            path: path.to_owned(),
            resolved_path: format!("/mnt{path}"),
            free_space: 10,
            used_space: 5,
            storage_type: Some("ext4".to_owned()),
            device_id: Some("dev0".to_owned()),
        }
    }

    #[test]
    fn folder_dto_drops_resolved_path() {
        let dto = FolderStorageDto::from_folder_storage_info(sample_folder_info("/data"));
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["Path"], "/data");
        assert_eq!(json["FreeSpace"], 10);
        assert!(json.get("ResolvedPath").is_none());
    }

    #[test]
    fn folder_dto_field_names_are_pascal_case() {
        let dto = FolderStorageDto::from_folder_storage_info(sample_folder_info("/data"));
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["Path"], "/data");
        assert_eq!(json["FreeSpace"], 10);
        assert_eq!(json["UsedSpace"], 5);
        assert_eq!(json["StorageType"], "ext4");
        assert_eq!(json["DeviceId"], "dev0");
    }

    #[test]
    fn folder_dto_omits_optionals_when_none() {
        let dto = FolderStorageDto::default();
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("StorageType").is_none());
        assert!(json.get("DeviceId").is_none());
    }

    #[test]
    fn folder_dto_round_trips() {
        let dto = FolderStorageDto::from_folder_storage_info(sample_folder_info("/data"));
        let back: FolderStorageDto =
            serde_json::from_str(&serde_json::to_string(&dto).unwrap()).unwrap();
        assert_eq!(dto, back);
    }

    #[test]
    fn library_dto_projects_and_names_are_pascal_case() {
        let info = LibraryStorageInfo {
            id: Uuid::from_u128(7),
            name: "Movies".to_owned(),
            folders: vec![sample_folder_info("/movies")],
        };
        let dto = LibraryStorageDto::from_library_storage_info(info);
        assert_eq!(dto.id, Uuid::from_u128(7));
        assert_eq!(dto.name, "Movies");
        assert_eq!(dto.folders.len(), 1);

        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("Id").is_some());
        assert_eq!(json["Name"], "Movies");
        assert_eq!(json["Folders"][0]["Path"], "/movies");
    }

    #[test]
    fn library_dto_round_trips() {
        let info = LibraryStorageInfo {
            id: Uuid::from_u128(7),
            name: "Movies".to_owned(),
            folders: vec![sample_folder_info("/movies")],
        };
        let dto = LibraryStorageDto::from_library_storage_info(info);
        let back: LibraryStorageDto =
            serde_json::from_str(&serde_json::to_string(&dto).unwrap()).unwrap();
        assert_eq!(dto, back);
    }

    #[test]
    fn system_dto_projects_every_folder() {
        let info = SystemStorageInfo {
            program_data_folder: sample_folder_info("/pd"),
            web_folder: sample_folder_info("/web"),
            image_cache_folder: sample_folder_info("/imgcache"),
            cache_folder: sample_folder_info("/cache"),
            log_folder: sample_folder_info("/log"),
            internal_metadata_folder: sample_folder_info("/meta"),
            transcoding_temp_folder: sample_folder_info("/transcode"),
            libraries: vec![LibraryStorageInfo {
                id: Uuid::from_u128(1),
                name: "Shows".to_owned(),
                folders: vec![sample_folder_info("/shows")],
            }],
        };
        let dto = SystemStorageDto::from_system_storage_info(info);
        assert_eq!(dto.program_data_folder.path, "/pd");
        assert_eq!(dto.web_folder.path, "/web");
        assert_eq!(dto.image_cache_folder.path, "/imgcache");
        assert_eq!(dto.cache_folder.path, "/cache");
        assert_eq!(dto.log_folder.path, "/log");
        assert_eq!(dto.internal_metadata_folder.path, "/meta");
        assert_eq!(dto.transcoding_temp_folder.path, "/transcode");
        assert_eq!(dto.libraries.len(), 1);
        assert_eq!(dto.libraries[0].name, "Shows");
    }

    #[test]
    fn system_dto_field_names_are_pascal_case() {
        let dto = SystemStorageDto::from_system_storage_info(SystemStorageInfo::default());
        let json = serde_json::to_value(&dto).unwrap();
        for key in [
            "ProgramDataFolder",
            "WebFolder",
            "ImageCacheFolder",
            "CacheFolder",
            "LogFolder",
            "InternalMetadataFolder",
            "TranscodingTempFolder",
            "Libraries",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn system_dto_round_trips() {
        let dto = SystemStorageDto::from_system_storage_info(SystemStorageInfo::default());
        let back: SystemStorageDto =
            serde_json::from_str(&serde_json::to_string(&dto).unwrap()).unwrap();
        assert_eq!(dto, back);
    }
}
