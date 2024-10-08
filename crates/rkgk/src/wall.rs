use std::{
    error::Error,
    fmt,
    str::FromStr,
    sync::{
        atomic::{self, AtomicU32},
        Arc, Weak,
    },
};

use dashmap::DashMap;
use haku::render::tiny_skia::Pixmap;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tracing::info;

use crate::{id, login::UserId, schema::Vec2, serialization::DeserializeFromStr};

pub mod auto_save;
pub mod broker;
pub mod chunk_images;
pub mod chunk_iterator;
pub mod database;

pub use broker::Broker;
pub use database::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WallId([u8; 32]);

impl WallId {
    pub fn new(rng: &mut dyn RngCore) -> Self {
        let mut bytes = [0; 32];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }
}

impl fmt::Display for WallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        id::serialize(f, "wall_", &self.0)
    }
}

impl FromStr for WallId {
    type Err = InvalidWallId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        id::deserialize(s, "wall_")
            .map(WallId)
            .map_err(|_| InvalidWallId)
    }
}

impl Serialize for WallId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for WallId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(DeserializeFromStr::new("wall ID"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct SessionId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidWallId;

impl fmt::Display for InvalidWallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid wall ID")
    }
}

impl Error for InvalidWallId {}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ChunkPosition {
    pub x: i32,
    pub y: i32,
}

impl fmt::Debug for ChunkPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

impl ChunkPosition {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

pub struct Chunk {
    pub pixmap: Pixmap,
}

impl Chunk {
    pub fn new(size: u32) -> Self {
        Self {
            pixmap: Pixmap::new(size, size).unwrap(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Settings {
    pub max_chunks: usize,
    pub max_sessions: usize,
    pub paint_area: u32,
    pub chunk_size: u32,
}

impl Settings {
    pub fn chunk_at_1d(&self, x: f32) -> i32 {
        f32::floor(x / self.chunk_size as f32) as i32
    }

    pub fn chunk_at_1d_ceil(&self, x: f32) -> i32 {
        f32::ceil(x / self.chunk_size as f32) as i32
    }

    pub fn chunk_at(&self, position: Vec2) -> ChunkPosition {
        ChunkPosition::new(self.chunk_at_1d(position.x), self.chunk_at_1d(position.y))
    }

    pub fn chunk_at_ceil(&self, position: Vec2) -> ChunkPosition {
        ChunkPosition::new(
            self.chunk_at_1d_ceil(position.x),
            self.chunk_at_1d_ceil(position.y),
        )
    }
}

pub struct Wall {
    settings: Settings,

    chunks: DashMap<ChunkPosition, Arc<Mutex<Chunk>>>,

    sessions: DashMap<SessionId, Session>,
    session_id_counter: AtomicU32,

    event_sender: broadcast::Sender<Event>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInit {
    // Provide a brush upon initialization, so that the user always has a valid brush set.
    pub brush: String,
}

pub struct Session {
    pub user_id: UserId,
    pub cursor: Option<Vec2>,
    pub brush: String,
}

pub struct SessionHandle {
    pub wall: Weak<Wall>,
    pub event_receiver: broadcast::Receiver<Event>,
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub session_id: SessionId,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EventKind {
    Join { nickname: String, init: UserInit },
    Leave,

    Cursor { position: Vec2 },

    SetBrush { brush: String },
    Plot { points: Vec<Vec2> },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Online {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub cursor: Option<Vec2>,
    pub brush: String,
}

impl Wall {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            chunks: DashMap::new(),
            sessions: DashMap::new(),
            session_id_counter: AtomicU32::new(0),
            event_sender: broadcast::channel(16).0,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn has_chunk(&self, at: ChunkPosition) -> bool {
        self.chunks.contains_key(&at)
    }

    pub fn get_chunk(&self, at: ChunkPosition) -> Option<Arc<Mutex<Chunk>>> {
        self.chunks.get(&at).map(|chunk| Arc::clone(&chunk))
    }

    pub fn get_or_create_chunk(&self, at: ChunkPosition) -> Arc<Mutex<Chunk>> {
        Arc::clone(&self.chunks.entry(at).or_insert_with(|| {
            info!(?at, "chunk created");
            Arc::new(Mutex::new(Chunk::new(self.settings.chunk_size)))
        }))
    }

    pub fn join(self: &Arc<Self>, session: Session) -> Result<SessionHandle, JoinError> {
        let session_id = SessionId(
            self.session_id_counter
                .fetch_add(1, atomic::Ordering::Relaxed),
        );

        self.sessions.insert(session_id, session);

        Ok(SessionHandle {
            wall: Arc::downgrade(self),
            event_receiver: self.event_sender.subscribe(),
            session_id,
        })
    }

    pub fn online(&self) -> Vec<Online> {
        self.sessions
            .iter()
            .map(|r| Online {
                session_id: *r.key(),
                user_id: r.user_id,
                cursor: r.value().cursor,
                brush: r.value().brush.clone(),
            })
            .collect()
    }

    pub fn event(&self, event: Event) {
        if let Some(mut session) = self.sessions.get_mut(&event.session_id) {
            match &event.kind {
                // Join and Leave are events that only get broadcasted through the wall such that
                // all users get them. We don't need to react to them in any way.
                EventKind::Join { .. } | EventKind::Leave => (),

                EventKind::Cursor { position } => {
                    session.cursor = Some(*position);
                }

                // Drawing events are handled by the owner session's thread to make drawing as
                // parallel as possible.
                EventKind::SetBrush { .. } | EventKind::Plot { .. } => (),
            }
        }

        _ = self.event_sender.send(event);
    }
}

impl Session {
    pub fn new(user_id: UserId, user_init: UserInit) -> Self {
        Self {
            user_id,
            cursor: None,
            brush: user_init.brush,
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        if let Some(wall) = self.wall.upgrade() {
            wall.sessions.remove(&self.session_id);
            wall.event(Event {
                session_id: self.session_id,
                kind: EventKind::Leave,
            });
            // After the session is removed, the wall will be garbage collected later.
        }
    }
}

pub enum JoinError {
    TooManyCurrentSessions,
    IdsExhausted,
}
