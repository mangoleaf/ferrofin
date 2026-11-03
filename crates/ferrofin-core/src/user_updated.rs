//! What a `UserUpdated` event sets off: ports of upstream's
//! `UserUpdatedNotifier` (the user's sessions are sent their refreshed
//! `UserDto`) and `DeviceAccessHost.OnUserUpdated` (the user's devices the
//! saved policy no longer allows are logged out).
//!
//! [`FerrofinUserManager`](crate::FerrofinUserManager) publishes
//! [`USER_UPDATED_EVENT`] after a committed rename or policy save, as
//! `UserManager.RenameUser` and `UpdatePolicyAsync` publish
//! `UserUpdatedEventArgs` and then raise `OnUserUpdated`, so every caller gets
//! both: the HTTP handlers, the startup wizard, a sign-in's new-user policy. A
//! configuration save publishes nothing (gl-864). The composition root
//! attaches the consumers with [`subscribe`].

use std::sync::Arc;

use uuid::Uuid;

use ferrofin_model::session::SessionMessageType;
use ferrofin_traits::devices::{DeviceManager, DeviceQuery};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::session::SessionManager;

use crate::event_manager::{FerrofinEventManager, consumer_done};

/// The bus event a rename or a policy save publishes: upstream's
/// `UserUpdatedEventArgs`. Its payload is the user's id as a JSON string.
pub const USER_UPDATED_EVENT: &str = "UserUpdated";

/// Subscribes one consumer on `bus` that runs both steps in upstream's order:
/// the push (`PublishAsync`), then the device check (`OnUserUpdated`).
///
/// Both are awaited, not spawned, so the request that saved answers after
/// them. Upstream awaits `PublishAsync`, but `DeviceAccessHost.OnUserUpdated`
/// is `async void`, which runs detached from its first incomplete await;
/// awaiting it here means a device the save refuses is signed out before the
/// response. Neither step takes a login lock, so both may run while the
/// sign-in path holds one, as it does while it saves a new user's policy.
pub fn subscribe(
    bus: &FerrofinEventManager,
    users: Arc<dyn UserManager>,
    sessions: Arc<dyn SessionManager>,
    devices: Arc<dyn DeviceManager>,
) {
    bus.subscribe(
        USER_UPDATED_EVENT,
        Arc::new(move |payload: &str| {
            let Ok(user_id) = serde_json::from_str::<Uuid>(payload) else {
                // Only `FerrofinUserManager` publishes it, with a user id.
                tracing::warn!(
                    payload,
                    "UserUpdated event without a user id; nothing pushed"
                );
                return consumer_done();
            };
            let users = Arc::clone(&users);
            let sessions = Arc::clone(&sessions);
            let devices = Arc::clone(&devices);
            Box::pin(async move {
                notify_user_updated(&*users, &*sessions, user_id).await;
                refresh_device_access(&*users, &*devices, &*sessions, user_id).await;
                Ok(())
            })
        }),
    );
}

/// `UserUpdatedNotifier.OnEvent`: sends `UserUpdated` with the user's DTO to
/// their sessions. Best-effort: the change it follows is already saved.
async fn notify_user_updated(
    users: &dyn UserManager,
    sessions: &dyn SessionManager,
    user_id: Uuid,
) {
    let sent = async {
        // Deleted since the save: nobody is left to tell.
        let Some(user) = users.get_user_by_id(user_id).await? else {
            return Ok(());
        };
        let dto = users.get_user_dto(&user, None).await?;
        let data = serde_json::to_string(&dto)
            .map_err(|error| ServiceError::backend(error.to_string()))?;
        sessions
            .send_message_to_user_sessions(&[user_id], SessionMessageType::UserUpdated, &data)
            .await
    }
    .await;
    if let Err(error) = sent {
        tracing::debug!(user_id = %user_id, %error, "could not push UserUpdated");
    }
}

/// `DeviceAccessHost.OnUserUpdated`, registered since Jellyfin 12.1. Upstream
/// catches and logs a failure, so this logs it and answers nothing.
async fn refresh_device_access(
    users: &dyn UserManager,
    devices: &dyn DeviceManager,
    sessions: &dyn SessionManager,
    user_id: Uuid,
) {
    if let Err(error) = update_device_access(users, devices, sessions, user_id).await {
        tracing::error!(user_id = %user_id, %error, "error updating device access for user");
    }
}

/// `DeviceAccessHost.UpdateDeviceAccess`: unless the user may use every device
/// (`EnableAllDevices`), logs out (`Logout(Device)`) each of their devices
/// `CanAccessDevice` refuses. Empty device ids are skipped, as upstream skips
/// them.
async fn update_device_access(
    users: &dyn UserManager,
    devices: &dyn DeviceManager,
    sessions: &dyn SessionManager,
    user_id: Uuid,
) -> Result<(), ServiceError> {
    // Deleted since the save: its devices went with it.
    let Some(user) = users.get_user_by_id(user_id).await? else {
        return Ok(());
    };
    if users
        .get_user_dto(&user, None)
        .await?
        .policy
        .is_some_and(|policy| policy.enable_all_devices)
    {
        return Ok(());
    }
    let owned = devices
        .get_devices(&DeviceQuery {
            user_id: Some(user_id),
            ..Default::default()
        })
        .await?
        .items;
    for device in &owned {
        if !device.device_id.is_empty()
            && !devices.can_access_device(&user, &device.device_id).await?
        {
            sessions.logout_device(device).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ferrofin_model::configuration::UserConfiguration;
    use ferrofin_traits::library::AdminFlagOutcome;

    use super::*;
    use crate::FerrofinUserManager;
    use crate::test_support::test_db;

    /// A bus whose one consumer records each announced user id, parsed as
    /// [`subscribe`]'s consumer parses it.
    fn recording_bus() -> (FerrofinEventManager, Arc<Mutex<Vec<Uuid>>>) {
        let bus = FerrofinEventManager::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        bus.subscribe(
            USER_UPDATED_EVENT,
            Arc::new(move |payload: &str| {
                let id = serde_json::from_str::<Uuid>(payload).expect("a user id");
                sink.lock().unwrap().push(id);
                consumer_done()
            }),
        );
        (bus, seen)
    }

    /// gl-1179: whoever calls the manager, a committed rename and every policy
    /// save are announced, as upstream's `RenameUser` and `UpdatePolicyAsync`
    /// publish `UserUpdatedEventArgs`. A configuration save, a refused rename
    /// and a sign-in's administrator flag (`UpdateUserAsync` in upstream's LDAP
    /// plugin) are not.
    #[tokio::test]
    async fn a_rename_and_a_policy_save_are_announced_and_nothing_else_is() {
        let (bus, seen) = recording_bus();
        let users = FerrofinUserManager::new(test_db().await).with_events(Arc::new(bus));
        let user = users.create_user("synthetic-announce").await.unwrap();
        let id = Uuid::parse_str(&user.id).unwrap();

        users
            .update_configuration(id, &UserConfiguration::default())
            .await
            .unwrap();
        assert!(
            users
                .rename_user(id, "synthetic-announce", "synthetic-announce")
                .await
                .is_err()
        );
        assert_eq!(
            users.apply_external_administrator(id, true).await.unwrap(),
            AdminFlagOutcome::Applied
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );

        users
            .rename_user(id, "synthetic-announce", "synthetic-renamed")
            .await
            .unwrap();
        let policy = users
            .get_user_dto(&users.get_user_by_id(id).await.unwrap().unwrap(), None)
            .await
            .unwrap()
            .policy
            .unwrap();
        users.update_policy(id, &policy).await.unwrap();
        users.update_policy_guarded(id, &policy).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), [id, id, id]);
    }

    /// A policy save announces only once it has released the user's login
    /// lock, as upstream raises the event once `_userLock` is released: a
    /// consumer that takes that lock (here through `change_password`, with an
    /// empty password, which skips the hashing) would otherwise wait on the
    /// save forever.
    #[tokio::test]
    async fn a_policy_save_announces_after_releasing_the_users_login_lock() {
        let bus = FerrofinEventManager::new();
        let users = FerrofinUserManager::new(test_db().await).with_events(Arc::new(bus.clone()));
        let user = users.create_user("synthetic-locked").await.unwrap();
        let id = Uuid::parse_str(&user.id).unwrap();
        let locker = users.clone();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        bus.subscribe(
            USER_UPDATED_EVENT,
            Arc::new(move |payload: &str| {
                let id = serde_json::from_str::<Uuid>(payload).expect("a user id");
                let (locker, flag) = (locker.clone(), Arc::clone(&flag));
                Box::pin(async move {
                    locker.change_password(id, "").await?;
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            }),
        );
        let policy = users
            .get_user_dto(&user, None)
            .await
            .unwrap()
            .policy
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            users.update_policy(id, &policy),
        )
        .await
        .expect("the save does not hold its lock across the announcement")
        .unwrap();
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "the consumer ran"
        );
    }
}
