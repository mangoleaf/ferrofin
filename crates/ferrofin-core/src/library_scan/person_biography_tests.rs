//! When [`LibraryScanner::enrich_people`] asks TMDB for a credited person's
//! biography (gl-99).
//!
//! Upstream never asks from the scan: `PeopleValidator` refreshes a person
//! that is new or never refreshed, then only after 30 days while it still
//! lacks an overview. The scan's own fetch is a head start on that first
//! refresh, so it asks only for a person never refreshed, and only once per
//! person: one TMDB has no biography for would otherwise be asked again for
//! every saved item crediting them.

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, WrittenPerson};
use uuid::Uuid;

use super::LibraryScanner;
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
use crate::people_repository::FerrofinPeopleRepository;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// A TMDB that knows every person but has no biography for any, and a
/// birthday only for person 4; returns its base URL and the TMDB id of each
/// `/person/{id}` request, in order.
fn spawn_tmdb_without_biographies() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let id = request
                .split_once("/person/")
                .and_then(|(_, rest)| rest.split(['?', ' ', '/']).next())
                .map(str::to_owned);
            // Something to store but no biography: still answered.
            let body = if id.as_deref() == Some("4") {
                r#"{"id": 4, "name": "P", "birthday": "1970-01-01"}"#
            } else {
                r#"{"id": 1, "name": "P"}"#
            };
            log.lock().expect("log").extend(id);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, asked)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_is_asked_for_a_biography_once_and_only_until_refreshed() {
    let (base, asked) = spawn_tmdb_without_biographies();
    let db = crate::test_support::test_db().await;
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let (ada, ben, cy, dee) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let person = |id: Uuid, refreshed: bool| BaseItemEntity {
        id: ferrofin_db::store::guid_to_db(id),
        type_: stored_type_name(BaseItemKind::Person)
            .expect("person type")
            .to_owned(),
        name: Some("P".into()),
        date_last_refreshed: refreshed.then(chrono::Utc::now),
        ..Default::default()
    };
    persistence
        .save_items(&[
            person(ada, false),
            person(ben, false),
            person(cy, true),
            person(dee, false),
        ])
        .await
        .expect("seed");
    let tmp = tempfile::tempdir().expect("tempdir");
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(persistence.clone()),
    );
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(items)
        .with_metadata(
            Arc::new(ferrofin_providers::TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        );
    let people = FerrofinPeopleRepository::new(db.clone());
    // No overview yet, and (mostly) a TMDB id to look one up by.
    let credit = |id, tmdb_id| WrittenPerson {
        id,
        needs_details: true,
        image_url: None,
        provider_id: tmdb_id,
    };
    // Ada is credited three times (three roles, the first without a TMDB
    // id), Cy was refreshed, and Dee gets a birthday but no biography.
    let cast = || {
        vec![
            credit(dee, Some(4)),
            credit(ada, None),
            credit(ada, Some(1)),
            credit(ben, Some(2)),
            credit(cy, Some(3)),
            credit(ada, Some(1)),
        ]
    };

    // Two items crediting the same cast. The client remembers only the last
    // `/person` answer (Ben's), so any repeat would reach TMDB again.
    scanner.enrich_people(&people, cast()).await;
    scanner.enrich_people(&people, cast()).await;

    assert_eq!(
        *asked.lock().expect("log"),
        ["4", "1", "2"],
        "each never-refreshed person is asked once; one already refreshed is \
         left to the people task"
    );
    let dee_row = FerrofinItemRepository::new(db, Arc::new(ItemTypeLookup::new()))
        .retrieve_item(dee)
        .await
        .expect("read")
        .expect("Dee");
    assert!(dee_row.premiere_date.is_some(), "what TMDB had is stored");
}
