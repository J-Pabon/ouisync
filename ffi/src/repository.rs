use super::utils::{self, Bytes, Port};
use crate::{
    interface::{ClientState, Notification},
    registry::Handle,
    session::{ServerState, SessionHandle, SubscriptionHandle},
};
use camino::Utf8PathBuf;
use ouisync_lib::{
    crypto::Password,
    device_id,
    network::{self, Registration},
    path, Access, AccessMode, AccessSecrets, EntryType, Error, Event, LocalSecret, Payload,
    Repository, RepositoryDb, Result, ShareToken,
};
use std::{borrow::Cow, os::raw::c_char, ptr, slice, str::FromStr};
use tokio::sync::{broadcast::error::RecvError, oneshot};
use tracing::Instrument;

pub const ENTRY_TYPE_INVALID: u8 = 0;
pub const ENTRY_TYPE_FILE: u8 = 1;
pub const ENTRY_TYPE_DIRECTORY: u8 = 2;

pub const ACCESS_MODE_BLIND: u8 = 0;
pub const ACCESS_MODE_READ: u8 = 1;
pub const ACCESS_MODE_WRITE: u8 = 2;

pub struct RepositoryHolder {
    pub(super) repository: Repository,
    registration: Registration,
}

/// Creates a new repository and set access to it based on the following table:
///
/// local_read_password  |  local_write_password  |  token access  |  result
/// ---------------------+------------------------+----------------+------------------------------
/// None or any          |  None or any           |  blind         |  blind replica
/// None                 |  None or any           |  read          |  read without password
/// read_pwd             |  None or any           |  read          |  read with read_pwd as password
/// None                 |  None                  |  write         |  read and write without password
/// any                  |  None                  |  write         |  read (only!) with password
/// None                 |  any                   |  write         |  read without password, require password for writing
/// any                  |  any                   |  write         |  read with password, write with (same or different) password
pub(crate) async fn create(
    state: &ServerState,
    store: String,
    local_read_password: Option<String>,
    local_write_password: Option<String>,
    share_token: Option<String>,
) -> Result<Handle<RepositoryHolder>> {
    let store = Utf8PathBuf::from(store);
    let local_read_password = local_read_password.as_deref().map(Password::new);
    let local_write_password = local_write_password.as_deref().map(Password::new);

    let access_secrets = if let Some(share_token) = share_token {
        let share_token: ShareToken = share_token.parse()?;
        share_token.into_secrets()
    } else {
        AccessSecrets::random_write()
    };

    let span = state.repo_span(&store);

    async {
        let device_id = device_id::get_or_create(&state.config).await?;

        let db = RepositoryDb::create(store.into_std_path_buf()).await?;

        let local_read_key = if let Some(local_read_password) = local_read_password {
            Some(db.password_to_key(local_read_password).await?)
        } else {
            None
        };

        let local_write_key = if let Some(local_write_password) = local_write_password {
            Some(db.password_to_key(local_write_password).await?)
        } else {
            None
        };

        let access = Access::new(local_read_key, local_write_key, access_secrets);
        let repository = Repository::create(db, device_id, access).await?;

        let registration = state.network.handle().register(repository.store().clone());

        // TODO: consider leaving the decision to enable DHT, PEX to the app.
        registration.enable_dht();
        registration.enable_pex();

        let holder = RepositoryHolder {
            repository,
            registration,
        };

        let handle = state.repositories.insert(holder);

        Ok(handle)
    }
    .instrument(span)
    .await
}

/// Opens an existing repository.
pub(crate) async fn open(
    state: &ServerState,
    store: String,
    local_password: Option<String>,
) -> Result<Handle<RepositoryHolder>> {
    let store = Utf8PathBuf::from(store);
    let local_password = local_password.as_deref().map(Password::new);

    let span = state.repo_span(&store);

    async {
        let device_id = device_id::get_or_create(&state.config).await?;

        let repository = Repository::open(
            store.into_std_path_buf(),
            device_id,
            local_password.map(LocalSecret::Password),
        )
        .await?;

        let registration = state.network.handle().register(repository.store().clone());

        // TODO: consider leaving the decision to enable DHT, PEX to the app.
        registration.enable_dht();
        registration.enable_pex();

        let holder = RepositoryHolder {
            repository,
            registration,
        };

        let handle = state.repositories.insert(holder);

        Ok(handle)
    }
    .instrument(span)
    .await
}

/// Closes a repository.
pub(crate) async fn close(state: &ServerState, handle: Handle<RepositoryHolder>) -> Result<()> {
    let holder = state.repositories.remove(handle);

    if let Some(holder) = holder {
        holder.repository.close().await
    } else {
        Ok(())
    }
}

/// If `share_token` is null, the function will try with the currently active access secrets in the
/// repository. Note that passing `share_token` explicitly (as opposed to implicitly using the
/// currently active secrets) may be used to increase access permissions.
///
/// Attempting to change the secret without enough permissions will fail with PermissionDenied
/// error.
///
/// If `local_read_password` is null, the repository will become readable without a password.
/// To remove the read (and write) permission use the `repository_remove_read_access`
/// function.
pub(crate) async fn set_read_access(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
    local_read_password: Option<String>,
    share_token: Option<String>,
) -> Result<()> {
    let holder = state.repositories.get(handle);

    let access_secrets = if let Some(share_token) = share_token {
        let share_token: ShareToken = share_token.parse()?;
        Some(share_token.into_secrets())
    } else {
        // Repository shall attempt to use the one it's currently using.
        None
    };

    let local_read_secret = local_read_password
        .as_deref()
        .map(Password::new)
        .map(LocalSecret::Password);

    holder
        .repository
        .set_read_access(local_read_secret.as_ref(), access_secrets.as_ref())
        .await
}

/// If `share_token` is null, the function will try with the currently active access secrets in the
/// repository. Note that passing `share_token` explicitly (as opposed to implicitly using the
/// currently active secrets) may be used to increase access permissions.
///
/// Attempting to change the secret without enough permissions will fail with PermissionDenied
/// error.
///
/// If `local_new_rw_password` is null, the repository will become read and writable without a
/// password.  To remove the read and write access use the
/// `repository_remove_read_and_write_access` function.
///
/// The `local_old_rw_password` is optional (may be a null pointer), if it is set the previously
/// used "writer ID" shall be used, otherwise a new one shall be generated. Note that it is
/// preferred to keep the writer ID as it was, this reduces the number of writers in Version
/// Vectors for every entry in the repository (files and directories) and thus reduces traffic and
/// CPU usage when calculating causal relationships.
pub(crate) async fn set_read_and_write_access(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
    local_old_rw_password: Option<String>,
    local_new_rw_password: Option<String>,
    share_token: Option<String>,
) -> Result<()> {
    let holder = state.repositories.get(handle);

    let access_secrets = if let Some(share_token) = share_token {
        let share_token: ShareToken = share_token.parse()?;
        Some(share_token.into_secrets())
    } else {
        // Repository shall attempt to use the one it's currently using.
        None
    };

    let local_old_rw_secret = local_old_rw_password
        .as_deref()
        .map(Password::new)
        .map(LocalSecret::Password);

    let local_new_rw_secret = local_new_rw_password
        .as_deref()
        .map(Password::new)
        .map(LocalSecret::Password);

    holder
        .repository
        .set_read_and_write_access(
            local_old_rw_secret.as_ref(),
            local_new_rw_secret.as_ref(),
            access_secrets.as_ref(),
        )
        .await?;

    Ok(())
}

/// Note that after removing read key the user may still read the repository if they previously had
/// write key set up.
pub(crate) async fn remove_read_key(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
) -> Result<()> {
    let holder = state.repositories.get(handle);
    holder.repository.remove_read_key().await
}

/// Note that removing the write key will leave read key intact.
pub(crate) async fn remove_write_key(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
) -> Result<()> {
    let holder = state.repositories.get(handle);
    holder.repository.remove_write_key().await
}

/// Returns true if the repository requires a local password to be opened for reading.
pub(crate) async fn requires_local_password_for_reading(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
) -> Result<bool> {
    let holder = state.repositories.get(handle);
    holder
        .repository
        .requires_local_password_for_reading()
        .await
}

/// Returns true if the repository requires a local password to be opened for writing.
pub(crate) async fn requires_local_password_for_writing(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
) -> Result<bool> {
    let holder = state.repositories.get(handle);
    holder
        .repository
        .requires_local_password_for_writing()
        .await
}

/// Return the info-hash of the repository formatted as hex string. This can be used as a globally
/// unique, non-secret identifier of the repository.
/// User is responsible for deallocating the returned string.
pub(crate) fn info_hash(state: &ServerState, handle: Handle<RepositoryHolder>) -> String {
    let holder = state.repositories.get(handle);
    let info_hash = network::repository_info_hash(holder.repository.secrets().id());

    hex::encode(info_hash)
}

/// Returns an ID that is randomly generated once per repository. Can be used to store local user
/// data per repository (e.g. passwords behind biometric storage).
pub(crate) async fn database_id(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
) -> Result<Vec<u8>> {
    let holder = state.repositories.get(handle);
    Ok(holder.repository.database_id().await?.as_ref().to_vec())
}

/// Returns the type of repository entry (file, directory, ...).
/// If the entry doesn't exists, returns `ENTRY_TYPE_INVALID`, not an error.
pub(crate) async fn entry_type(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
    path: String,
) -> Result<u8> {
    let holder = state.repositories.get(handle);
    let path = Utf8PathBuf::from(path);

    match holder.repository.lookup_type(path).await {
        Ok(entry_type) => Ok(entry_type_to_num(entry_type)),
        Err(Error::EntryNotFound) => Ok(ENTRY_TYPE_INVALID),
        Err(error) => Err(error),
    }
}

/// Move/rename entry from src to dst.
pub(crate) async fn move_entry(
    state: &ServerState,
    handle: Handle<RepositoryHolder>,
    src: String,
    dst: String,
) -> Result<()> {
    let holder = state.repositories.get(handle);
    let src = Utf8PathBuf::from(src);
    let dst = Utf8PathBuf::from(dst);

    let (src_dir, src_name) = path::decompose(&src).ok_or(Error::EntryNotFound)?;
    let (dst_dir, dst_name) = path::decompose(&dst).ok_or(Error::EntryNotFound)?;

    holder
        .repository
        .move_entry(src_dir, src_name, dst_dir, dst_name)
        .await
}

/// Subscribe to change notifications from the repository.
pub(crate) fn subscribe(
    server_state: &ServerState,
    client_state: &ClientState,
    repository_handle: Handle<RepositoryHolder>,
) -> Result<SubscriptionHandle> {
    let holder = server_state.repositories.get(repository_handle);
    let (subscription_id_tx, subscription_id_rx) = oneshot::channel();

    let mut notification_rx = holder.repository.subscribe();
    let notification_tx = client_state.notification_tx.clone();

    let subscription_task = scoped_task::spawn(async move {
        // unwrap is OK because we send the handle after this spawn.
        let subscription_id = subscription_id_rx.await.unwrap();

        loop {
            match notification_rx.recv().await {
                // Only `BlockReceived` events cause user-observable changes
                Ok(Event {
                    payload: Payload::BlockReceived { .. },
                    ..
                }) => (),
                Ok(Event {
                    payload: Payload::BranchChanged(_) | Payload::FileClosed,
                    ..
                }) => continue,
                Err(RecvError::Lagged(_)) => (),
                Err(RecvError::Closed) => break,
            }

            notification_tx
                .send((subscription_id, Notification::Repository))
                .await
                .ok();
        }
    });
    let subscription_handle = server_state.tasks.insert(subscription_task);

    // unwrap OK because we immediately receive in the task spawned above.
    subscription_id_tx.send(subscription_handle.id()).unwrap();

    Ok(subscription_handle)
}

#[no_mangle]
pub unsafe extern "C" fn repository_is_dht_enabled(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
) -> bool {
    session
        .get()
        .state
        .repositories
        .get(handle)
        .registration
        .is_dht_enabled()
}

#[no_mangle]
pub unsafe extern "C" fn repository_enable_dht(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
) {
    let session = session.get();
    let holder = session.state.repositories.get(handle);

    // HACK: the `enable_dht` call isn't async so spawning it should not be necessary. However,
    // calling it directly (even with entered runtime context) sometimes causes crash in the app
    // (SIGSEGV / stack corruption) for some reason. The spawn seems to fix it.
    let task = session
        .runtime()
        .spawn(async move { holder.registration.enable_dht() });

    // HACK: wait until the task completes so that this function is actually sync.
    session.runtime().block_on(task).ok();
}

#[no_mangle]
pub unsafe extern "C" fn repository_disable_dht(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
) {
    session
        .get()
        .state
        .repositories
        .get(handle)
        .registration
        .disable_dht()
}

#[no_mangle]
pub unsafe extern "C" fn repository_is_pex_enabled(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
) -> bool {
    session
        .get()
        .state
        .repositories
        .get(handle)
        .registration
        .is_pex_enabled()
}

#[no_mangle]
pub unsafe extern "C" fn repository_enable_pex(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
) {
    session
        .get()
        .state
        .repositories
        .get(handle)
        .registration
        .enable_pex()
}

#[no_mangle]
pub unsafe extern "C" fn repository_disable_pex(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
) {
    session
        .get()
        .state
        .repositories
        .get(handle)
        .registration
        .disable_pex()
}

/// The `password` parameter is optional, if `null` the current access level of the opened
/// repository is used. If provided, the highest access level that the password can unlock is used.
#[no_mangle]
pub unsafe extern "C" fn repository_create_share_token(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
    password: *const c_char,
    access_mode: u8,
    name: *const c_char,
    port: Port<Result<String>>,
) {
    session.get().with(port, |ctx| {
        let holder = ctx.state().repositories.get(handle);
        let access_mode = access_mode_from_num(access_mode)?;
        let name = utils::ptr_to_str(name)?.to_owned();
        let password = utils::ptr_to_maybe_str(password)?;
        let password = password.map(Password::new);

        ctx.spawn(async move {
            let access_secrets = if let Some(password) = password {
                Cow::Owned(
                    holder
                        .repository
                        .unlock_secrets(LocalSecret::Password(password))
                        .await?,
                )
            } else {
                Cow::Borrowed(holder.repository.secrets())
            };

            let share_token =
                ShareToken::from(access_secrets.with_mode(access_mode)).with_name(name);

            Ok(share_token.to_string())
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn repository_access_mode(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
) -> u8 {
    let holder = session.get().state.repositories.get(handle);
    access_mode_to_num(holder.repository.access_mode())
}

/// Returns the syncing progress as a float in the 0.0 - 1.0 range.
#[no_mangle]
pub unsafe extern "C" fn repository_sync_progress(
    session: SessionHandle,
    handle: Handle<RepositoryHolder>,
    port: Port<Result<Vec<u8>>>,
) {
    session.get().with(port, |ctx| {
        let holder = ctx.state().repositories.get(handle);

        ctx.spawn(async move {
            let progress = holder.repository.sync_progress().await?;
            // unwrap is OK because serialization into a vector has no reason to fail
            Ok(rmp_serde::to_vec(&progress).unwrap())
        })
    })
}

/// Returns the access mode of the given share token.
#[no_mangle]
pub unsafe extern "C" fn share_token_mode(token: *const c_char) -> u8 {
    #![allow(clippy::question_mark)] // false positive

    let token = if let Ok(token) = utils::ptr_to_str(token) {
        token
    } else {
        return ACCESS_MODE_BLIND;
    };

    let token: ShareToken = if let Ok(token) = token.parse() {
        token
    } else {
        return ACCESS_MODE_BLIND;
    };

    access_mode_to_num(token.access_mode())
}

/// Returns the info-hash of the repository corresponding to the share token formatted as hex
/// string.
/// User is responsible for deallocating the returned string.
#[no_mangle]
pub unsafe extern "C" fn share_token_info_hash(token: *const c_char) -> *const c_char {
    let token = if let Ok(token) = utils::ptr_to_str(token) {
        token
    } else {
        return ptr::null();
    };

    let token: ShareToken = if let Ok(token) = token.parse() {
        token
    } else {
        return ptr::null();
    };

    utils::str_to_ptr(&hex::encode(
        network::repository_info_hash(token.id()).as_ref(),
    ))
}

/// IMPORTANT: the caller is responsible for deallocating the returned pointer unless it is `null`.
#[no_mangle]
pub unsafe extern "C" fn share_token_suggested_name(token: *const c_char) -> *const c_char {
    let token = if let Ok(token) = utils::ptr_to_str(token) {
        token
    } else {
        return ptr::null();
    };

    let token: ShareToken = if let Ok(token) = token.parse() {
        token
    } else {
        return ptr::null();
    };

    utils::str_to_ptr(token.suggested_name().as_ref())
}

/// Take the input string, decide whether it's a valid OuiSync token and normalize it (remove white
/// space, unnecessary slashes,...).
/// IMPORTANT: the caller is responsible for deallocating the returned buffer unless it is `null`.
#[no_mangle]
pub unsafe extern "C" fn share_token_normalize(token: *const c_char) -> *const c_char {
    #![allow(clippy::question_mark)] // false positive

    let token = if let Ok(token) = utils::ptr_to_str(token) {
        token
    } else {
        return ptr::null();
    };

    let token: ShareToken = if let Ok(token) = ShareToken::from_str(token) {
        token
    } else {
        return ptr::null();
    };

    utils::str_to_ptr(&token.to_string())
}

/// IMPORTANT: the caller is responsible for deallocating the returned buffer unless it is `null`.
#[no_mangle]
pub unsafe extern "C" fn share_token_encode(token: *const c_char) -> Bytes {
    #![allow(clippy::question_mark)] // false positive

    let token = if let Ok(token) = utils::ptr_to_str(token) {
        token
    } else {
        return Bytes::NULL;
    };

    let token: ShareToken = if let Ok(token) = token.parse() {
        token
    } else {
        return Bytes::NULL;
    };

    let mut buffer = Vec::new();
    token.encode(&mut buffer);

    Bytes::from_vec(buffer)
}

/// IMPORTANT: the caller is responsible for deallocating the returned pointer unless it is `null`.
#[no_mangle]
pub unsafe extern "C" fn share_token_decode(bytes: *const u8, len: u64) -> *const c_char {
    let len = if let Ok(len) = len.try_into() {
        len
    } else {
        return ptr::null();
    };

    let slice = slice::from_raw_parts(bytes, len);

    let token = if let Ok(token) = ShareToken::decode(slice) {
        token
    } else {
        return ptr::null();
    };

    utils::str_to_ptr(token.to_string().as_ref())
}

pub(super) fn entry_type_to_num(entry_type: EntryType) -> u8 {
    match entry_type {
        EntryType::File => ENTRY_TYPE_FILE,
        EntryType::Directory => ENTRY_TYPE_DIRECTORY,
    }
}

fn access_mode_from_num(num: u8) -> Result<AccessMode, Error> {
    // Note: we could've used `AccessMode::try_from` instead but then we would need a separate
    // check (ideally a compile-time one) that the `ACCESS_MODE_*` constants match the
    // corresponding `AccessMode` variants.

    match num {
        ACCESS_MODE_BLIND => Ok(AccessMode::Blind),
        ACCESS_MODE_READ => Ok(AccessMode::Read),
        ACCESS_MODE_WRITE => Ok(AccessMode::Write),
        _ => Err(Error::MalformedData),
    }
}

fn access_mode_to_num(mode: AccessMode) -> u8 {
    match mode {
        AccessMode::Blind => ACCESS_MODE_BLIND,
        AccessMode::Read => ACCESS_MODE_READ,
        AccessMode::Write => ACCESS_MODE_WRITE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_mode_constants() {
        for mode in [AccessMode::Blind, AccessMode::Read, AccessMode::Write] {
            assert_eq!(
                access_mode_from_num(access_mode_to_num(mode)).unwrap(),
                mode
            );
        }
    }
}
