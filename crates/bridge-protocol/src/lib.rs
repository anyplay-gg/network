//! Frames between the AnyPS5 network bridge and the AnyPlay net service.
//!
//! The wire format is specified in `spec/bridge-protocol.md`. This crate is its reference; the
//! bridge in AnyPS5 implements the same file in C++.

use std::fmt;
use std::net::Ipv4Addr;

mod payload;

pub use payload::{Reader, Writer};

/// The protocol version this crate speaks.
pub const VERSION: u16 = 0;

/// The largest payload a frame may carry.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// The size of the key that opens a game traffic stream.
pub const STREAM_KEY_LEN: usize = 16;

const HEADER: usize = 12;

/// What a frame's payload is. Unknown kinds are kept, so either side can be newer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Kind(pub u16);

impl Kind {
    pub const HELLO: Kind = Kind(0x0001);
    pub const WELCOME: Kind = Kind(0x0002);
    pub const REJECT: Kind = Kind(0x0003);
    pub const UNSUPPORTED: Kind = Kind(0x8000);
    pub const STATE: Kind = Kind(0x0101);
    pub const FEATURE: Kind = Kind(0x0102);
    pub const WEB_REQUEST: Kind = Kind(0x0201);
    pub const PUSH_FILTER: Kind = Kind(0x0202);
    pub const PUSH_FILTER_GONE: Kind = Kind(0x0203);
    pub const PUSH_EVENT: Kind = Kind(0x0204);
    pub const ACTIVATE: Kind = Kind(0x0301);
    pub const DEACTIVATE: Kind = Kind(0x0302);
    pub const PEER: Kind = Kind(0x0303);
    pub const PEER_INFO: Kind = Kind(0x0304);
    pub const BIND: Kind = Kind(0x0305);
    pub const UNBIND: Kind = Kind(0x0306);
    pub const INTENT: Kind = Kind(0x0401);
    pub const INVITATION_DIALOG: Kind = Kind(0x0402);

    /// The kind of the reply to a request of this kind.
    pub const fn reply(self) -> Kind {
        Kind(self.0 | 0x8000)
    }

    /// True for the kind of a reply.
    pub const fn is_reply(self) -> bool {
        self.0 & 0x8000 != 0
    }
}

/// One message on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    /// 0 for an event; otherwise the number that pairs a request with its reply.
    pub request: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// A frame announced or carried more than [`MAX_PAYLOAD`] bytes. The connection must be closed.
    TooLarge(usize),
    /// A payload ended before all of its fields.
    Truncated,
    /// A string or list is longer than its count field can say.
    TooLong,
    /// A string is not UTF-8.
    NotText,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooLarge(len) => write!(f, "frame payload of {len} bytes exceeds {MAX_PAYLOAD}"),
            Error::Truncated => f.write_str("payload ended before all of its fields"),
            Error::TooLong => f.write_str("a string or list is too long for its count"),
            Error::NotText => f.write_str("a string is not UTF-8"),
        }
    }
}

impl std::error::Error for Error {}

impl Frame {
    /// An event carrying `message`.
    pub fn event(message: &Message) -> Result<Self, Error> {
        Self::numbered(message, 0)
    }

    /// Request number `request` carrying `message`, or its reply when `message` is a reply.
    pub fn numbered(message: &Message, request: u32) -> Result<Self, Error> {
        Ok(Self {
            kind: message.kind(),
            request,
            payload: message.payload()?,
        })
    }

    /// The reply to this frame saying its kind is not known.
    pub fn unsupported(&self) -> Self {
        Self {
            kind: Kind::UNSUPPORTED,
            request: self.request,
            payload: Vec::new(),
        }
    }

    /// Appends the frame's bytes to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let len = self.payload.len();
        if len > MAX_PAYLOAD {
            return Err(Error::TooLarge(len));
        }
        out.reserve(HEADER + len);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&self.kind.0.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.request.to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(())
    }

    /// The frame's message, or `None` for a kind this version does not know.
    pub fn message(&self) -> Result<Option<Message>, Error> {
        Message::parse(self.kind, &self.payload)
    }
}

/// Collects bytes as they arrive and cuts them into frames.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    start: usize,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` until enough bytes have arrived.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        let buf = &self.buf[self.start..];
        if buf.len() < HEADER {
            self.compact();
            return Ok(None);
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(Error::TooLarge(len));
        }
        if buf.len() < HEADER + len {
            self.compact();
            return Ok(None);
        }
        let kind = Kind(u16::from_le_bytes([buf[4], buf[5]]));
        let request = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let payload = buf[HEADER..HEADER + len].to_vec();
        self.start += HEADER + len;
        Ok(Some(Frame {
            kind,
            request,
            payload,
        }))
    }

    /// Drops the bytes of frames already handed out, once they are worth moving.
    fn compact(&mut self) {
        if self.start > 4096 && self.start * 2 > self.buf.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }
}

/// Every message of version 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Welcome(Welcome),
    Reject(Reject),
    Unsupported,
    State(State),
    Feature { user_id: i32, feature: u32 },
    WebRequest(WebRequest),
    WebReply(WebReply),
    PushFilter(PushFilter),
    PushFilterGone { user_id: i32, filter: u32 },
    PushEvent(PushEvent),
    Activate(Activate),
    Activated { result: ActivateResult },
    Deactivate { user_id: i32, session: String },
    Peer(Peer),
    PeerInfo { address: Ipv4Addr },
    PeerInfoReply(PeerInfoReply),
    Bind(Bind),
    Bound { result: BindResult },
    Unbind { vport: u16, transport: Transport },
    Intent(Intent),
    InvitationDialog(InvitationDialog),
    InvitationDialogReply { result: DialogResult, sent: u16 },
}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::Hello(_) => Kind::HELLO,
            Message::Welcome(_) => Kind::WELCOME,
            Message::Reject(_) => Kind::REJECT,
            Message::Unsupported => Kind::UNSUPPORTED,
            Message::State(_) => Kind::STATE,
            Message::Feature { .. } => Kind::FEATURE,
            Message::WebRequest(_) => Kind::WEB_REQUEST,
            Message::WebReply(_) => Kind::WEB_REQUEST.reply(),
            Message::PushFilter(_) => Kind::PUSH_FILTER,
            Message::PushFilterGone { .. } => Kind::PUSH_FILTER_GONE,
            Message::PushEvent(_) => Kind::PUSH_EVENT,
            Message::Activate(_) => Kind::ACTIVATE,
            Message::Activated { .. } => Kind::ACTIVATE.reply(),
            Message::Deactivate { .. } => Kind::DEACTIVATE,
            Message::Peer(_) => Kind::PEER,
            Message::PeerInfo { .. } => Kind::PEER_INFO,
            Message::PeerInfoReply(_) => Kind::PEER_INFO.reply(),
            Message::Bind(_) => Kind::BIND,
            Message::Bound { .. } => Kind::BIND.reply(),
            Message::Unbind { .. } => Kind::UNBIND,
            Message::Intent(_) => Kind::INTENT,
            Message::InvitationDialog(_) => Kind::INVITATION_DIALOG,
            Message::InvitationDialogReply { .. } => Kind::INVITATION_DIALOG.reply(),
        }
    }

    /// The payload bytes of this message.
    pub fn payload(&self) -> Result<Vec<u8>, Error> {
        let mut w = Writer::new();
        match self {
            Message::Hello(m) => {
                w.u16(m.version);
                w.raw(&m.token);
            }
            Message::Welcome(m) => {
                w.u16(m.version);
                w.u16(m.udp_port);
                w.u16(m.tcp_port);
                w.raw(&m.stream_key);
            }
            Message::Reject(m) => w.raw(m.reason.as_bytes()),
            Message::Unsupported => {}
            Message::State(m) => {
                w.u8(m.signed_in as u8);
                w.u64(m.account_id);
                w.str(&m.handle)?;
                w.u8(m.nat_type);
                w.addr(m.address);
            }
            Message::Feature { user_id, feature } => {
                w.i32(*user_id);
                w.u32(*feature);
            }
            Message::WebRequest(m) => {
                w.i32(m.user_id);
                w.str(&m.api_group)?;
                w.str(&m.path)?;
                w.str(&m.method)?;
                w.pairs(&m.headers)?;
                w.bytes(&m.body)?;
            }
            Message::WebReply(m) => {
                w.u16(m.status);
                w.pairs(&m.headers)?;
                w.bytes(&m.body)?;
            }
            Message::PushFilter(m) => {
                w.i32(m.user_id);
                w.u32(m.filter);
                w.strs(&m.data_types)?;
            }
            Message::PushFilterGone { user_id, filter } => {
                w.i32(*user_id);
                w.u32(*filter);
            }
            Message::PushEvent(m) => {
                w.i32(m.user_id);
                w.u32(m.filter);
                w.str(&m.data_type)?;
                w.u64(m.from);
                w.u64(m.to);
                w.bytes(&m.data)?;
                w.pairs(&m.extended)?;
            }
            Message::Activate(m) => {
                w.i32(m.user_id);
                w.str(&m.session)?;
                w.u8(m.session_kind as u8);
            }
            Message::Activated { result } => w.u8(*result as u8),
            Message::Deactivate { user_id, session } => {
                w.i32(*user_id);
                w.str(session)?;
            }
            Message::Peer(m) => {
                w.str(&m.session)?;
                w.u64(m.account_id);
                w.str(&m.handle)?;
                w.addr(m.address);
                w.u8(m.peer_state as u8);
            }
            Message::PeerInfo { address } => w.addr(*address),
            Message::PeerInfoReply(m) => {
                w.u8(m.known as u8);
                w.u32(m.rtt_us);
                w.u8(m.relayed as u8);
            }
            Message::Bind(m) => {
                w.u16(m.vport);
                w.u8(m.transport as u8);
                w.u16(m.loopback_port);
            }
            Message::Bound { result } => w.u8(*result as u8),
            Message::Unbind { vport, transport } => {
                w.u16(*vport);
                w.u8(*transport as u8);
            }
            Message::Intent(m) => {
                w.i32(m.user_id);
                w.str(&m.intent_type)?;
                w.pairs(&m.properties)?;
            }
            Message::InvitationDialog(m) => {
                w.i32(m.user_id);
                w.str(&m.session)?;
                w.u16(m.max);
            }
            Message::InvitationDialogReply { result, sent } => {
                w.u8(*result as u8);
                w.u16(*sent);
            }
        }
        Ok(w.into_inner())
    }

    /// Reads the payload of a frame of `kind`; `None` for a kind this version does not know.
    pub fn parse(kind: Kind, payload: &[u8]) -> Result<Option<Message>, Error> {
        let mut r = Reader::new(payload);
        let message = match kind {
            Kind::HELLO => Message::Hello(Hello {
                version: r.u16()?,
                token: r.rest().to_vec(),
            }),
            Kind::WELCOME => {
                let version = r.u16()?;
                Message::Welcome(Welcome {
                    version,
                    udp_port: r.u16()?,
                    tcp_port: r.u16()?,
                    stream_key: r.array()?,
                })
            }
            Kind::REJECT => Message::Reject(Reject {
                reason: String::from_utf8_lossy(r.rest()).into_owned(),
            }),
            Kind::UNSUPPORTED => Message::Unsupported,
            Kind::STATE => Message::State(State {
                signed_in: r.u8()? != 0,
                account_id: r.u64()?,
                handle: r.str()?,
                nat_type: r.u8()?,
                address: r.addr()?,
            }),
            Kind::FEATURE => Message::Feature {
                user_id: r.i32()?,
                feature: r.u32()?,
            },
            Kind::WEB_REQUEST => Message::WebRequest(WebRequest {
                user_id: r.i32()?,
                api_group: r.str()?,
                path: r.str()?,
                method: r.str()?,
                headers: r.pairs()?,
                body: r.bytes()?.to_vec(),
            }),
            k if k == Kind::WEB_REQUEST.reply() => Message::WebReply(WebReply {
                status: r.u16()?,
                headers: r.pairs()?,
                body: r.bytes()?.to_vec(),
            }),
            Kind::PUSH_FILTER => Message::PushFilter(PushFilter {
                user_id: r.i32()?,
                filter: r.u32()?,
                data_types: r.strs()?,
            }),
            Kind::PUSH_FILTER_GONE => Message::PushFilterGone {
                user_id: r.i32()?,
                filter: r.u32()?,
            },
            Kind::PUSH_EVENT => Message::PushEvent(PushEvent {
                user_id: r.i32()?,
                filter: r.u32()?,
                data_type: r.str()?,
                from: r.u64()?,
                to: r.u64()?,
                data: r.bytes()?.to_vec(),
                extended: r.pairs()?,
            }),
            Kind::ACTIVATE => Message::Activate(Activate {
                user_id: r.i32()?,
                session: r.str()?,
                session_kind: SessionKind::from(r.u8()?),
            }),
            k if k == Kind::ACTIVATE.reply() => Message::Activated {
                result: ActivateResult::from(r.u8()?),
            },
            Kind::DEACTIVATE => Message::Deactivate {
                user_id: r.i32()?,
                session: r.str()?,
            },
            Kind::PEER => Message::Peer(Peer {
                session: r.str()?,
                account_id: r.u64()?,
                handle: r.str()?,
                address: r.addr()?,
                peer_state: PeerState::from(r.u8()?),
            }),
            Kind::PEER_INFO => Message::PeerInfo { address: r.addr()? },
            k if k == Kind::PEER_INFO.reply() => Message::PeerInfoReply(PeerInfoReply {
                known: r.u8()? != 0,
                rtt_us: r.u32()?,
                relayed: r.u8()? != 0,
            }),
            Kind::BIND => Message::Bind(Bind {
                vport: r.u16()?,
                transport: Transport::from(r.u8()?),
                loopback_port: r.u16()?,
            }),
            k if k == Kind::BIND.reply() => Message::Bound {
                result: BindResult::from(r.u8()?),
            },
            Kind::UNBIND => Message::Unbind {
                vport: r.u16()?,
                transport: Transport::from(r.u8()?),
            },
            Kind::INTENT => Message::Intent(Intent {
                user_id: r.i32()?,
                intent_type: r.str()?,
                properties: r.pairs()?,
            }),
            Kind::INVITATION_DIALOG => Message::InvitationDialog(InvitationDialog {
                user_id: r.i32()?,
                session: r.str()?,
                max: r.u16()?,
            }),
            k if k == Kind::INVITATION_DIALOG.reply() => Message::InvitationDialogReply {
                result: DialogResult::from(r.u8()?),
                sent: r.u16()?,
            },
            _ => return Ok(None),
        };
        Ok(Some(message))
    }
}

/// The bridge's first frame: the version it speaks and the secret of this game start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    pub token: Vec<u8>,
}

/// The net service's acceptance: the version both sides use from now on, and where game traffic
/// goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub version: u16,
    /// The loopback UDP port for game datagrams, 0 when the net service carries none.
    pub udp_port: u16,
    /// The loopback TCP port for game streams, 0 when the net service carries none.
    pub tcp_port: u16,
    /// The key every game stream connection starts with.
    pub stream_key: [u8; STREAM_KEY_LEN],
}

/// The net service's refusal. The reason is for the log; the connection closes after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reject {
    pub reason: String,
}

/// Sign-in, identity and NAT, as the bridge answers the game from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub signed_in: bool,
    pub account_id: u64,
    pub handle: String,
    /// 1, 2 or 3 as the platform counts NAT types; 0 while unknown.
    pub nat_type: u8,
    /// The player's own virtual address; unspecified while it has none.
    pub address: Ipv4Addr,
}

impl State {
    /// The state of a player who is not signed in.
    pub fn signed_out() -> Self {
        Self {
            signed_in: false,
            account_id: 0,
            handle: String::new(),
            nat_type: 0,
            address: Ipv4Addr::UNSPECIFIED,
        }
    }
}

/// A web API request exactly as the game built it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebRequest {
    pub user_id: i32,
    pub api_group: String,
    pub path: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The answer to a web request. Status 0 means the network does not offer this API.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl WebReply {
    /// The reply for an API the network does not offer.
    pub fn not_offered() -> Self {
        Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushFilter {
    pub user_id: i32,
    pub filter: u32,
    pub data_types: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushEvent {
    pub user_id: i32,
    pub filter: u32,
    pub data_type: String,
    pub from: u64,
    pub to: u64,
    pub data: Vec<u8>,
    pub extended: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activate {
    pub user_id: i32,
    pub session: String,
    pub session_kind: SessionKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub session: String,
    pub account_id: u64,
    pub handle: String,
    pub address: Ipv4Addr,
    pub peer_state: PeerState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerInfoReply {
    pub known: bool,
    pub rtt_us: u32,
    pub relayed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bind {
    pub vport: u16,
    pub transport: Transport,
    pub loopback_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub user_id: i32,
    pub intent_type: String,
    pub properties: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvitationDialog {
    pub user_id: i32,
    pub session: String,
    pub max: u16,
}

macro_rules! byte_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $value:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum $name {
            $($variant = $value,)+
            /// A value this version does not know.
            Unknown = 255,
        }

        impl From<u8> for $name {
            fn from(value: u8) -> Self {
                match value {
                    $($value => $name::$variant,)+
                    _ => $name::Unknown,
                }
            }
        }
    };
}

byte_enum!(SessionKind { Player = 1, Game = 2 });
byte_enum!(ActivateResult { Done = 0, UnknownSession = 1, SignedOut = 2 });
byte_enum!(PeerState { Connecting = 1, Connected = 2, Disconnected = 3, Failed = 4 });
byte_enum!(Transport { Datagram = 1, Stream = 2 });
byte_enum!(BindResult { Bound = 0, InUse = 1 });
byte_enum!(DialogResult { Sent = 0, Cancelled = 1 });

/// The header in front of every game datagram and at the start of every game stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrafficHeader {
    /// The peer's virtual address: the destination on the way out, the sender on the way in.
    pub address: Ipv4Addr,
    /// The receiving virtual port.
    pub vport: u16,
    /// The sending virtual port.
    pub source_vport: u16,
}

impl TrafficHeader {
    pub const LEN: usize = 8;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        out[..4].copy_from_slice(&u32::from(self.address).to_le_bytes());
        out[4..6].copy_from_slice(&self.vport.to_le_bytes());
        out[6..].copy_from_slice(&self.source_vport.to_le_bytes());
        out
    }

    /// The header and the game's bytes after it, or `None` for a datagram shorter than a header.
    pub fn split(datagram: &[u8]) -> Option<(TrafficHeader, &[u8])> {
        let (head, rest) = datagram.split_at_checked(Self::LEN)?;
        let header = TrafficHeader {
            address: Ipv4Addr::from(u32::from_le_bytes([head[0], head[1], head[2], head[3]])),
            vport: u16::from_le_bytes([head[4], head[5]]),
            source_vport: u16::from_le_bytes([head[6], head[7]]),
        };
        Some((header, rest))
    }
}

#[cfg(test)]
mod tests;
