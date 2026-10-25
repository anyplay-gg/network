//! The desktop portal's ScreenCast on Linux: the player picks the game's window in the system's
//! own dialog, and the portal hands back a PipeWire stream of that window and nothing else.
//!
//! The portal has no way to name a window for the player, so the processes the launcher gives
//! cannot preselect it (spec/screen-share.md: the player confirms the window each time). Nothing
//! is kept between streams: no restore token is asked for. A screen picked instead of a window is
//! refused, since some pickers offer screens whatever is asked for.
//!
//! The portal speaks D-Bus, which `ashpd` (MIT) does on a tokio runtime; the worker itself is
//! not async, so the runtime lives here, one thread, for as long as the stream does. Dropping
//! [`PortalWindow`] closes the portal session, which ends the PipeWire stream.

use std::os::fd::OwnedFd;

use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
};
use ashpd::desktop::{PersistMode, Session};

/// A window the player picked: the PipeWire remote to read it from, and the stream's node.
pub struct PortalWindow {
    /// The PipeWire remote of the portal's session (only this stream is visible on it); taken by
    /// whoever connects to it.
    pub fd: Option<OwnedFd>,
    /// The stream's PipeWire node.
    pub node_id: u32,
    /// The window's size when it was picked, if the portal says.
    pub size: Option<(i32, i32)>,
    session: Option<Session<Screencast>>,
    runtime: tokio::runtime::Runtime,
}

impl std::fmt::Debug for PortalWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalWindow")
            .field("node_id", &self.node_id)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Asks the player for the window to share. Blocks while the portal's dialog is open; an error
/// when there is no portal, the player cancels, or the portal gives no window.
pub fn pick_window() -> Result<PortalWindow, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("the portal's runtime could not start: {e}"))?;
    let (session, fd, node_id, size, kind) = runtime
        .block_on(open())
        .map_err(|e| format!("the screen cast portal refused: {e}"))?;
    // Some pickers offer screens whatever was asked for (Hyprland's opens on its Screen tab).
    // Only a window is shared: a screen would show the desktop and everything on it.
    if kind != Some(SourceType::Window) {
        let _ = runtime.block_on(session.close());
        return Err(format!(
            "the player picked {} instead of a window; only the game's window is shared",
            match kind {
                Some(SourceType::Monitor) => "a screen",
                Some(SourceType::Virtual) => "a virtual screen",
                _ => "a source of unknown kind",
            }
        ));
    }
    Ok(PortalWindow {
        fd: Some(fd),
        node_id,
        size,
        session: Some(session),
        runtime,
    })
}

/// What the portal gave: its session, the PipeWire remote, the stream's node, size and kind.
type Opened = (
    Session<Screencast>,
    OwnedFd,
    u32,
    Option<(i32, i32)>,
    Option<SourceType>,
);

async fn open() -> ashpd::Result<Opened> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    // The cursor stays out of the picture, as on Windows; a portal that cannot hide it draws it.
    let cursor = proxy
        .available_cursor_modes()
        .await
        .ok()
        .filter(|modes| modes.contains(CursorMode::Hidden))
        .map(|_| CursorMode::Hidden);
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Window))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let streams = proxy
        .start(&session, None, StartCastOptions::default())
        .await?
        .response()?;
    let Some(stream) = streams.streams().first() else {
        return Err(ashpd::Error::NoResponse);
    };
    let (node_id, size, kind) = (
        stream.pipe_wire_node_id(),
        stream.size(),
        stream.source_type(),
    );
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    Ok((session, fd, node_id, size, kind))
}

impl Drop for PortalWindow {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = self.runtime.block_on(session.close());
        }
    }
}
