//! Dart FFI (foreign function interface) for the ouisync library.

#[macro_use]
mod utils;
#[cfg(not(feature = "dart"))]
mod c;
#[cfg(feature = "dart")]
mod dart;
mod directory;
mod error;
mod file;
mod handler;
mod network;
mod protocol;
mod registry;
mod repository;
mod sender;
mod session;
mod share_token;
mod state;
mod state_monitor;
mod transport;

use crate::session::{SessionCreateResult, SessionHandle};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use std::{ffi::CString, os::raw::c_char, slice};

#[cfg(feature = "dart")]
use {
    crate::{
        dart::{Port, PortSender},
        error::Error,
        file::FileHolder,
        registry::Handle,
        sender::Sender,
    },
    std::os::raw::{c_int, c_void},
};

#[cfg(not(feature = "dart"))]
use crate::c::{Callback, CallbackSender};

/// Creates a ouisync session
///
/// # Safety
///
/// - `configs_path` and `log_path` must be pointers to nul-terminated utf-8 encoded strings.
/// - `context` must be a valid pointer to a value that outlives the `Session` and that is safe
///   to be sent to other threads or null.
/// - `callback` must be a valid function pointer which does not leak the passed `msg_ptr`.
#[cfg(not(feature = "dart"))]
#[no_mangle]
pub unsafe extern "C" fn session_create(
    configs_path: *const c_char,
    log_path: *const c_char,
    context: *mut (),
    callback: Callback,
) -> SessionCreateResult {
    let sender = CallbackSender::new(context, callback);
    session::create(configs_path, log_path, sender)
}

/// Creates a ouisync session
///
/// # Safety
///
/// - `configs_path` and `log_path` must be pointers to nul-terminated utf-8 encoded strings.
/// - `post_c_object_fn` must be a pointer to the dart's `NativeApi.postCObject` function cast to
///   `Pointer<void>` (the casting is necessary to work around limitations of the binding
///   generators)
#[cfg(feature = "dart")]
#[no_mangle]
pub unsafe extern "C" fn session_create(
    configs_path: *const c_char,
    log_path: *const c_char,
    post_c_object_fn: *const c_void,
    port: Port,
) -> SessionCreateResult {
    let sender = PortSender::new(std::mem::transmute(post_c_object_fn), port);
    session::create(configs_path, log_path, sender)
}

/// Closes the ouisync session.
///
/// # Safety
///
/// `session` must be a valid session handle.
#[no_mangle]
pub unsafe extern "C" fn session_close(session: SessionHandle) {
    session.release();
}

/// # Safety
///
/// `session` must be a valid session handle, `sender` must be a valid client sender handle,
/// `payload_ptr` must be a pointer to a byte buffer whose length is at least `payload_len` bytes.
///
#[no_mangle]
pub unsafe extern "C" fn session_channel_send(
    session: SessionHandle,
    payload_ptr: *mut u8,
    payload_len: u64,
) {
    let payload = slice::from_raw_parts(payload_ptr, payload_len as usize);
    let payload = payload.into();

    session.get().client_sender.send(payload).ok();
}

/// Shutdowns the network and closes the session. This is equivalent to doing it in two steps
/// (`network_shutdown` then `session_close`), but in flutter when the engine is being detached
/// from Android runtime then async wait for `network_shutdown` never completes (or does so
/// randomly), and thus `session_close` is never invoked. My guess is that because the dart engine
/// is being detached we can't do any async await on the dart side anymore, and thus need to do it
/// here.
///
/// # Safety
///
/// `session` must be a valid session handle.
#[no_mangle]
pub unsafe extern "C" fn session_shutdown_network_and_close(session: SessionHandle) {
    session.release().shutdown_network_and_close();
}

/// Copy the file contents into the provided raw file descriptor.
///
/// This function takes ownership of the file descriptor and closes it when it finishes. If the
/// caller needs to access the descriptor afterwards (or while the function is running), he/she
/// needs to `dup` it before passing it into this function.
///
/// # Safety
///
/// - `session` must be a valid session handle
/// - `handle` must be a valid file holder handle
/// - `fd` must be a valid and open file descriptor
/// - `post_c_object_fn` must be a pointer to the dart's `NativeApi.postCObject` function
/// - `port` must be a valid dart native port
#[cfg(all(unix, feature = "dart"))]
#[no_mangle]
pub unsafe extern "C" fn file_copy_to_raw_fd(
    session: SessionHandle,
    handle: Handle<FileHolder>,
    fd: c_int,
    post_c_object_fn: *const c_void,
    port: Port,
) {
    use bytes::Bytes;
    use std::{io::SeekFrom, os::fd::FromRawFd};
    use tokio::fs;

    let session = session.get();
    let sender = PortSender::new(std::mem::transmute(post_c_object_fn), port);

    let src = session.state.files.get(handle);
    let mut dst = fs::File::from_raw_fd(fd);

    session.runtime.spawn(async move {
        let mut src = src.file.lock().await;
        src.seek(SeekFrom::Start(0));
        let result = src.copy_to_writer(&mut dst).await;

        match result {
            Ok(()) => sender.send(Bytes::new()),
            Err(error) => sender.send(encode_error(&error.into())),
        }
    });
}

/// Always returns `OperationNotSupported` error. Defined to avoid lookup errors on non-unix
/// platforms. Do not use.
///
/// # Safety
///
/// - `post_c_object_fn` must be a pointer to the dart's `NativeApi.postCObject` function
/// - `port` must be a valid dart native port.
/// - `session`, `handle` and `fd` are not actually used and so have no safety requirements.
#[cfg(all(not(unix), feature = "dart"))]
#[no_mangle]
pub unsafe extern "C" fn file_copy_to_raw_fd(
    _session: SessionHandle,
    _handle: Handle<FileHolder>,
    _fd: c_int,
    post_c_object_fn: *const c_void,
    port: Port,
) {
    let sender = PortSender::new(std::mem::transmute(post_c_object_fn), port);
    sender.send(encode_error(
        &ouisync_lib::Error::OperationNotSupported.into(),
    ))
}

#[cfg(feature = "dart")]
fn encode_error(error: &Error) -> bytes::Bytes {
    use bytes::{BufMut, BytesMut};

    let mut buffer = BytesMut::new();
    buffer.put_u16(error.code as u16);
    buffer.put_slice(error.message.as_bytes());
    buffer.freeze()
}

/// Deallocate string that has been allocated on the rust side
///
/// # Safety
///
/// `ptr` must be a pointer obtained from a call to `CString::into_raw`.
#[no_mangle]
pub unsafe extern "C" fn free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }

    let _ = CString::from_raw(ptr);
}

/// Print log message
///
/// # Safety
///
/// `message_ptr` must be a pointer to a nul-terminated utf-8 encoded string
#[no_mangle]
pub unsafe extern "C" fn log_print(
    level: u8,
    scope_ptr: *const c_char,
    message_ptr: *const c_char,
) {
    let scope = match utils::ptr_to_str(scope_ptr) {
        Ok(scope) => scope,
        Err(error) => {
            tracing::error!(?error, "invalid log scope string");
            return;
        }
    };

    let _enter = tracing::info_span!("app", scope).entered();

    let message = match utils::ptr_to_str(message_ptr) {
        Ok(message) => message,
        Err(error) => {
            tracing::error!(?error, "invalid log message string");
            return;
        }
    };

    match level.try_into() {
        Ok(LogLevel::Error) => tracing::error!("{}", message),
        Ok(LogLevel::Warn) => tracing::warn!("{}", message),
        Ok(LogLevel::Info) => tracing::info!("{}", message),
        Ok(LogLevel::Debug) => tracing::debug!("{}", message),
        Ok(LogLevel::Trace) => tracing::trace!("{}", message),
        Err(_) => tracing::error!(level, "invalid log level"),
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}
