//! The real [`Social`]: Discord's Social SDK, loaded at run time.
//!
//! The library is opened by absolute path, with symbols looked up through bindings that bindgen
//! made from the SDK's header while this crate was built (nothing of it is in the repository).
//! A library that lacks a symbol is refused when it is loaded.
//!
//! One thread owns the SDK client. It handles requests from the [`Sdk`] handle and runs the
//! SDK's callbacks every few milliseconds; a callback does nothing but post a message back to the
//! same thread, which then reads what it needs and tells the caller through the event channel.

#![allow(unsafe_code)]

mod ffi {
    #![allow(
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        dead_code,
        unused_imports,
        unsafe_op_in_unsafe_fn,
        clippy::all
    )]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

use std::ffi::c_void;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use crate::{Authorization, Error, Event, SCOPES, Snapshot, Social, State, User, scrub};

/// How often the SDK's callbacks run when nothing else wakes the thread.
const PUMP: Duration = Duration::from_millis(10);

enum Msg {
    Authorize,
    AbortAuthorize,
    Connect(String),
    Disconnect,
    Stop,
    // From callbacks
    Status(i64, i64, i32),
    Authorized(Result<(String, String), String>),
    TokenUpdated(Result<(), String>),
    Recount,
}

/// The real SDK. See the module.
pub struct Sdk {
    messages: Sender<Msg>,
    shared: Arc<Mutex<Snapshot>>,
    events: UnboundedSender<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Sdk {
    /// Loads the library at `library` (an absolute path) and starts the SDK for `application_id`.
    /// Fails when the file is missing, does not load, or lacks a function this crate calls.
    pub fn load(
        library: &Path,
        application_id: u64,
        events: UnboundedSender<Event>,
    ) -> Result<Self, Error> {
        if !library.is_absolute() {
            return Err(Error::Sdk(
                "the library path must be absolute; it is never searched for".into(),
            ));
        }
        if !library.is_file() {
            return Err(Error::Sdk(format!("no library at {}", library.display())));
        }
        let shared = Arc::new(Mutex::new(Snapshot::new(State::SignedOut)));
        let (messages, inbox) = mpsc::channel();
        let (ready_tx, ready) = mpsc::sync_channel::<Result<String, String>>(1);
        let thread = {
            let (library, shared, events, own) = (
                library.to_owned(),
                shared.clone(),
                events.clone(),
                messages.clone(),
            );
            std::thread::Builder::new()
                .name("discord-sdk".into())
                .spawn(move || {
                    run(
                        &library,
                        application_id,
                        inbox,
                        own,
                        shared,
                        events,
                        ready_tx,
                    )
                })
                .map_err(|e| Error::Sdk(e.to_string()))?
        };
        match ready.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(version)) => {
                tracing::info!("Discord Social SDK {version} loaded");
                Ok(Self {
                    messages,
                    shared,
                    events,
                    thread: Some(thread),
                })
            }
            Ok(Err(why)) => {
                let _ = thread.join();
                Err(Error::Sdk(why))
            }
            Err(_) => Err(Error::Sdk("the library did not start in time".into())),
        }
    }

    fn set(&self, snapshot: Snapshot) {
        set(&self.shared, &self.events, snapshot);
    }

    fn state(&self) -> State {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).state
    }
}

fn set(shared: &Mutex<Snapshot>, events: &UnboundedSender<Event>, snapshot: Snapshot) {
    let mut current = shared.lock().unwrap_or_else(|e| e.into_inner());
    if *current != snapshot {
        *current = snapshot.clone();
        let _ = events.send(Event::Changed(snapshot));
    }
}

impl Social for Sdk {
    fn snapshot(&self) -> Snapshot {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn authorize(&mut self) -> Result<(), Error> {
        if self.state() != State::SignedOut {
            return Err(Error::Busy);
        }
        self.set(Snapshot::new(State::Connecting));
        self.messages
            .send(Msg::Authorize)
            .map_err(|_| Error::Unavailable)
    }

    fn abort_authorize(&mut self) {
        let _ = self.messages.send(Msg::AbortAuthorize);
    }

    fn connect(&mut self, access_token: &str) -> Result<(), Error> {
        if self.state() == State::Connected {
            return Err(Error::Busy);
        }
        self.set(Snapshot::new(State::Connecting));
        self.messages
            .send(Msg::Connect(access_token.to_owned()))
            .map_err(|_| Error::Unavailable)
    }

    fn disconnect(&mut self) {
        let _ = self.messages.send(Msg::Disconnect);
        self.set(Snapshot::new(State::SignedOut));
    }
}

impl Drop for Sdk {
    fn drop(&mut self) {
        let _ = self.messages.send(Msg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// The thread

struct Ctx {
    messages: Sender<Msg>,
}

fn context(messages: &Sender<Msg>) -> *mut c_void {
    Box::into_raw(Box::new(Ctx {
        messages: messages.clone(),
    }))
    .cast()
}

unsafe extern "C" fn free_context(pointer: *mut c_void) {
    if !pointer.is_null() {
        drop(unsafe { Box::from_raw(pointer.cast::<Ctx>()) });
    }
}

fn post(user_data: *mut c_void, msg: Msg) {
    if let Some(ctx) = unsafe { user_data.cast::<Ctx>().as_ref() } {
        let _ = ctx.messages.send(msg);
    }
}

fn text(s: &ffi::Discord_String) -> String {
    if s.ptr.is_null() || s.size == 0 {
        return String::new();
    }
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(s.ptr, s.size) }).into_owned()
}

fn borrowed(s: &str) -> ffi::Discord_String {
    ffi::Discord_String {
        ptr: s.as_ptr().cast_mut(),
        size: s.len(),
    }
}

/// A string the SDK allocated: copied, then freed.
fn owned(sdk: &ffi::Sdk, s: ffi::Discord_String) -> String {
    let copy = text(&s);
    if !s.ptr.is_null() {
        unsafe { sdk.Discord_Free(s.ptr.cast()) };
    }
    copy
}

fn empty_string() -> ffi::Discord_String {
    ffi::Discord_String {
        ptr: std::ptr::null_mut(),
        size: 0,
    }
}

/// What a failed result says, without anything secret.
fn failure_of(sdk: &ffi::Sdk, result: *mut ffi::Discord_ClientResult) -> String {
    let mut message = empty_string();
    unsafe { sdk.Discord_ClientResult_Error(result, &mut message) };
    let code = unsafe { sdk.Discord_ClientResult_ErrorCode(result) };
    format!("{} (code {code})", scrub(&owned(sdk, message)))
}

// The callbacks run inside `Discord_RunCallbacks` on the SDK thread. The library is reached
// through a global for them, since a C callback has no other way to; it is set before the first
// call and lives as long as the thread.
static LIBRARY: std::sync::atomic::AtomicPtr<ffi::Sdk> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

fn library<'a>() -> Option<&'a ffi::Sdk> {
    unsafe { LIBRARY.load(std::sync::atomic::Ordering::Acquire).as_ref() }
}

unsafe extern "C" fn on_status(
    status: ffi::Discord_Client_Status,
    error: ffi::Discord_Client_Error,
    detail: i32,
    user_data: *mut c_void,
) {
    post(user_data, Msg::Status(status as i64, error as i64, detail));
}

unsafe extern "C" fn on_authorized(
    result: *mut ffi::Discord_ClientResult,
    code: ffi::Discord_String,
    redirect: ffi::Discord_String,
    user_data: *mut c_void,
) {
    let Some(sdk) = library() else { return };
    let outcome = if unsafe { sdk.Discord_ClientResult_Successful(result) } {
        Ok((text(&code), text(&redirect)))
    } else {
        Err(failure_of(sdk, result))
    };
    post(user_data, Msg::Authorized(outcome));
}

unsafe extern "C" fn on_token_updated(
    result: *mut ffi::Discord_ClientResult,
    user_data: *mut c_void,
) {
    let Some(sdk) = library() else { return };
    let outcome = if unsafe { sdk.Discord_ClientResult_Successful(result) } {
        Ok(())
    } else {
        Err(failure_of(sdk, result))
    };
    post(user_data, Msg::TokenUpdated(outcome));
}

unsafe extern "C" fn on_relationship(_user: u64, _discord: bool, user_data: *mut c_void) {
    post(user_data, Msg::Recount);
}

unsafe extern "C" fn on_groups(_user: u64, user_data: *mut c_void) {
    post(user_data, Msg::Recount);
}

unsafe extern "C" fn on_log(
    message: ffi::Discord_String,
    severity: ffi::Discord_LoggingSeverity,
    _user_data: *mut c_void,
) {
    let line = scrub(&text(&message));
    if severity as i64 >= ffi::Discord_LoggingSeverity_Error as i64 {
        tracing::warn!(target: "discord_sdk", "{line}");
    } else {
        tracing::debug!(target: "discord_sdk", "{line}");
    }
}

fn run(
    path: &Path,
    application_id: u64,
    inbox: Receiver<Msg>,
    own: Sender<Msg>,
    shared: Arc<Mutex<Snapshot>>,
    events: UnboundedSender<Event>,
    ready: mpsc::SyncSender<Result<String, String>>,
) {
    let sdk = match unsafe { ffi::Sdk::new(path) } {
        Ok(sdk) => Box::new(sdk),
        Err(error) => {
            let _ = ready.send(Err(format!("{}: {error}", path.display())));
            return;
        }
    };
    let sdk = Box::into_raw(sdk);
    LIBRARY.store(sdk, std::sync::atomic::Ordering::Release);
    let lib = unsafe { &*sdk };

    let mut client = ffi::Discord_Client {
        opaque: std::ptr::null_mut(),
    };
    unsafe {
        lib.Discord_Client_Init(&mut client);
        lib.Discord_Client_SetApplicationId(&mut client, application_id);
        lib.Discord_Client_AddLogCallback(
            &mut client,
            Some(on_log),
            Some(free_context),
            context(&own),
            ffi::Discord_LoggingSeverity_Warning as _,
        );
        lib.Discord_Client_SetStatusChangedCallback(
            &mut client,
            Some(on_status),
            Some(free_context),
            context(&own),
        );
        lib.Discord_Client_SetRelationshipCreatedCallback(
            &mut client,
            Some(on_relationship),
            Some(free_context),
            context(&own),
        );
        lib.Discord_Client_SetRelationshipDeletedCallback(
            &mut client,
            Some(on_relationship),
            Some(free_context),
            context(&own),
        );
        lib.Discord_Client_SetRelationshipGroupsUpdatedCallback(
            &mut client,
            Some(on_groups),
            Some(free_context),
            context(&own),
        );
    }
    let version = unsafe {
        format!(
            "{}.{}.{}",
            lib.Discord_Client_GetVersionMajor(),
            lib.Discord_Client_GetVersionMinor(),
            lib.Discord_Client_GetVersionPatch()
        )
    };
    let _ = ready.send(Ok(version));

    let mut verifier: Option<String> = None;
    // True from `connect` until the connection ends, so late callbacks of an ended one are ignored.
    let mut active = false;
    loop {
        match inbox.recv_timeout(PUMP) {
            Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(msg) => handle(
                lib,
                &mut client,
                msg,
                application_id,
                &own,
                &shared,
                &events,
                &mut verifier,
                &mut active,
            ),
        }
        unsafe { lib.Discord_RunCallbacks() };
    }
    unsafe {
        lib.Discord_Client_AbortAuthorize(&mut client);
        lib.Discord_Client_Disconnect(&mut client);
        lib.Discord_RunCallbacks();
        lib.Discord_Client_Drop(&mut client);
        lib.Discord_RunCallbacks();
    }
    LIBRARY.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
    drop(unsafe { Box::from_raw(sdk) });
}

#[allow(clippy::too_many_arguments)]
fn handle(
    lib: &ffi::Sdk,
    client: &mut ffi::Discord_Client,
    msg: Msg,
    application_id: u64,
    own: &Sender<Msg>,
    shared: &Mutex<Snapshot>,
    events: &UnboundedSender<Event>,
    verifier: &mut Option<String>,
    active: &mut bool,
) {
    let signed_out = || Snapshot::new(State::SignedOut);
    match msg {
        Msg::Authorize => match start_authorization(lib, client, own, application_id) {
            Ok(v) => *verifier = Some(v),
            Err(why) => {
                let _ = events.send(Event::AuthorizationFailed(why));
                set(shared, events, signed_out());
            }
        },
        Msg::AbortAuthorize => {
            unsafe { lib.Discord_Client_AbortAuthorize(client) };
            *verifier = None;
            if !*active {
                set(shared, events, signed_out());
            }
        }
        Msg::Authorized(Ok((code, redirect_uri))) => {
            // No verifier means the authorisation was aborted meanwhile.
            if let Some(verifier) = verifier.take() {
                let _ = events.send(Event::Authorized(Authorization {
                    code,
                    verifier,
                    redirect_uri,
                }));
            }
        }
        Msg::Authorized(Err(why)) => {
            *verifier = None;
            let _ = events.send(Event::AuthorizationFailed(why));
            set(shared, events, signed_out());
        }
        Msg::Connect(token) => {
            *active = true;
            unsafe {
                lib.Discord_Client_UpdateToken(
                    client,
                    ffi::Discord_AuthorizationTokenType_Bearer as _,
                    borrowed(&token),
                    Some(on_token_updated),
                    Some(free_context),
                    context(own),
                );
            }
        }
        Msg::TokenUpdated(Ok(())) => {
            if *active {
                unsafe { lib.Discord_Client_Connect(client) };
            }
        }
        Msg::TokenUpdated(Err(why)) => {
            *active = false;
            let _ = events.send(Event::Failed(why));
            set(shared, events, signed_out());
        }
        Msg::Disconnect => {
            *active = false;
            *verifier = None;
            unsafe {
                lib.Discord_Client_AbortAuthorize(client);
                lib.Discord_Client_Disconnect(client);
            }
        }
        Msg::Status(status, error, detail) => {
            if !*active {
                return;
            }
            let status_is = |known| status == i64::from(known);
            if status_is(ffi::Discord_Client_Status_Ready) {
                let snapshot = read_ready(lib, client);
                set(shared, events, snapshot);
            } else if status_is(ffi::Discord_Client_Status_Disconnected) {
                *active = false;
                if error != i64::from(ffi::Discord_Client_Error_None) {
                    let _ = events.send(Event::Failed(format!(
                        "the connection failed (error {error}, detail {detail})"
                    )));
                }
                set(shared, events, signed_out());
            } else {
                // Connecting, connected but not ready, waiting, reconnecting.
                let current = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if current.state != State::Connected {
                    set(shared, events, Snapshot::new(State::Connecting));
                }
            }
        }
        Msg::Recount => {
            let current = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if *active && current.state == State::Connected {
                set(shared, events, read_ready(lib, client));
            }
        }
        Msg::Stop => {}
    }
}

/// Makes a PKCE verifier and challenge and starts the authorisation. Returns the verifier.
fn start_authorization(
    lib: &ffi::Sdk,
    client: &mut ffi::Discord_Client,
    own: &Sender<Msg>,
    application_id: u64,
) -> Result<String, String> {
    unsafe {
        let mut code_verifier = ffi::Discord_AuthorizationCodeVerifier {
            opaque: std::ptr::null_mut(),
        };
        lib.Discord_Client_CreateAuthorizationCodeVerifier(client, &mut code_verifier);
        let mut challenge = ffi::Discord_AuthorizationCodeChallenge {
            opaque: std::ptr::null_mut(),
        };
        lib.Discord_AuthorizationCodeVerifier_Challenge(&mut code_verifier, &mut challenge);
        let mut secret = empty_string();
        lib.Discord_AuthorizationCodeVerifier_Verifier(&mut code_verifier, &mut secret);
        let verifier = owned(lib, secret);

        let mut args = ffi::Discord_AuthorizationArgs {
            opaque: std::ptr::null_mut(),
        };
        lib.Discord_AuthorizationArgs_Init(&mut args);
        lib.Discord_AuthorizationArgs_SetClientId(&mut args, application_id);
        lib.Discord_AuthorizationArgs_SetScopes(&mut args, borrowed(SCOPES));
        lib.Discord_AuthorizationArgs_SetCodeChallenge(&mut args, &mut challenge);
        lib.Discord_Client_Authorize(
            client,
            &mut args,
            Some(on_authorized),
            Some(free_context),
            context(own),
        );
        lib.Discord_AuthorizationArgs_Drop(&mut args);
        lib.Discord_AuthorizationCodeChallenge_Drop(&mut challenge);
        lib.Discord_AuthorizationCodeVerifier_Drop(&mut code_verifier);
        if verifier.is_empty() {
            return Err("the SDK made no PKCE verifier".into());
        }
        Ok(verifier)
    }
}

/// The player and the number of Discord friends, once the SDK is ready.
fn read_ready(lib: &ffi::Sdk, client: &mut ffi::Discord_Client) -> Snapshot {
    unsafe {
        let mut handle = ffi::Discord_UserHandle {
            opaque: std::ptr::null_mut(),
        };
        let user = if lib.Discord_Client_GetCurrentUserV2(client, &mut handle) {
            let id = lib.Discord_UserHandle_Id(&mut handle);
            let mut name = empty_string();
            lib.Discord_UserHandle_DisplayName(&mut handle, &mut name);
            let name = owned(lib, name);
            lib.Discord_UserHandle_Drop(&mut handle);
            Some(User {
                id: id.to_string(),
                name,
            })
        } else {
            None
        };
        let mut span = ffi::Discord_RelationshipHandleSpan {
            ptr: std::ptr::null_mut(),
            size: 0,
        };
        lib.Discord_Client_GetRelationships(client, &mut span);
        let mut friends = 0u32;
        for i in 0..span.size {
            let handle = span.ptr.add(i);
            if lib.Discord_RelationshipHandle_DiscordRelationshipType(handle)
                == ffi::Discord_RelationshipType_Friend
            {
                friends += 1;
            }
            lib.Discord_RelationshipHandle_Drop(handle);
        }
        if !span.ptr.is_null() {
            lib.Discord_Free(span.ptr.cast());
        }
        Snapshot {
            state: State::Connected,
            user,
            friends: Some(friends),
        }
    }
}
