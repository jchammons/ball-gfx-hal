use crate::double_buffer::DoubleBuffer;
use atomic::{Atomic, Ordering};
use cgmath::{Point2, Vector2};
use palette::{LabHue, Lch, LinSrgb};
use parking_lot::{Mutex, MutexGuard};
use rand::{thread_rng, Rng};
use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::{Add, Mul, Sub};
use std::time::Instant;

const SPRING_CONSTANT: f32 = 5.0;

pub type PlayerId = u16;

pub trait Interpolate {
    type Output;

    fn lerp(self, other: Self, alpha: f32) -> Self::Output;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Player {
    pub position: Point2<f32>,
    pub position_ball: Point2<f32>,
    pub velocity_ball: Vector2<f32>,
    pub color: LinSrgb,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub position: Point2<f32>,
    pub position_ball: Point2<f32>,
    pub velocity_ball: Vector2<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerClient {
    pub color: LinSrgb,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub players: HashMap<PlayerId, PlayerSnapshot>,
}

/// Be careful! This contains a mutex guard.
pub struct InterpolatedSnapshot<'a> {
    alpha: f32,
    snapshots: MutexGuard<'a, DoubleBuffer<(Snapshot, Instant)>>,
    client_player: PlayerId,
    client_snapshot: PlayerSnapshot,
}

#[derive(Debug)]
pub struct Input {
    position: Atomic<Point2<f32>>,
}

#[derive(Debug)]
pub struct GameClient {
    pub players: Mutex<HashMap<PlayerId, PlayerClient>>,
    pub snapshots: Mutex<DoubleBuffer<(Snapshot, Instant)>>,
    pub input: Input,
    pub position_ball: Atomic<Point2<f32>>,
    pub velocity_ball: Atomic<Vector2<f32>>,
    pub client_player: PlayerId,
}

#[derive(Debug)]
pub struct GameServer {
    pub players: HashMap<PlayerId, Player>,
    next_id: PlayerId,
}

// Complex bounds here in order to handle Point2, which doesn't allow
// addition, but allows subtraction into Vector2 and adding Vector2 to
// Point2. Hopefully the compiler can optimize this all out in the
// end.
impl<T> Interpolate for T
where
    T: Clone + Sub<T> + Add<<<T as Sub>::Output as Mul<f32>>::Output, Output = T>,
    <T as Sub<T>>::Output: Mul<f32>,
{
    type Output = T;

    fn lerp(self, other: T, alpha: f32) -> T {
        self.clone() + (other - self.clone()) * alpha
    }
}

impl Interpolate for &PlayerSnapshot {
    type Output = PlayerSnapshot;

    fn lerp(self, other: &PlayerSnapshot, alpha: f32) -> PlayerSnapshot {
        PlayerSnapshot {
            position: self.position.lerp(other.position, alpha),
            position_ball: self.position_ball.lerp(other.position_ball, alpha),
            velocity_ball: self.velocity_ball.lerp(other.velocity_ball, alpha),
        }
    }
}

impl Default for Input {
    fn default() -> Input {
        Input {
            position: Atomic::new(Point2::new(0.0, 0.0)),
        }
    }
}

impl Input {
    pub fn set_position(&self, position: Point2<f32>) {
        self.position.store(position, Ordering::Release)
    }

    pub fn position(&self) -> Point2<f32> {
        self.position.load(Ordering::Acquire)
    }
}

impl Player {
    pub fn with_random_color() -> Player {
        let mut rng = thread_rng();
        let hue = LabHue::from_degrees(rng.gen_range(0.0, 360.0));
        Player {
            position: Point2::new(0.0, 0.0),
            position_ball: Point2::new(0.0, 0.0),
            velocity_ball: Vector2::new(0.0, 0.0),
            color: Lch::new(75.0, 80.0, hue).into(),
        }
    }

    pub fn as_client(&self) -> PlayerClient {
        PlayerClient { color: self.color }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            position: self.position,
            position_ball: self.position_ball,
            velocity_ball: self.velocity_ball,
        }
    }
}

impl<'a> From<InterpolatedSnapshot<'a>> for Snapshot {
    fn from(other: InterpolatedSnapshot<'a>) -> Snapshot {
        Snapshot {
            players: other.players().collect(),
        }
    }
}

impl<'a> InterpolatedSnapshot<'a> {
    pub fn players<'b: 'a>(&'b self) -> impl Iterator<Item = (PlayerId, PlayerSnapshot)> + 'b {
        self.snapshots
            .get()
            .0
            .players
            .iter()
            .map(move |(&id, new)| {
                if id == self.client_player {
                    (id, self.client_snapshot.clone())
                } else {
                    match self.snapshots.get_old().0.players.get(&id) {
                        Some(old) => (id, old.lerp(new, self.alpha)),
                        None => (id, new.clone()),
                    }
                }
            })
    }

    pub fn player(&self, id: PlayerId) -> Option<PlayerSnapshot> {
        if id == self.client_player {
            return Some(self.client_snapshot.clone());
        }
        self.snapshots.get().0.players.get(&id).map(|new| {
            match self.snapshots.get_old().0.players.get(&id) {
                Some(old) => old.lerp(new, self.alpha),
                None => new.clone(),
            }
        })
    }
}

impl GameClient {
    pub fn new(
        players: HashMap<PlayerId, PlayerClient>,
        snapshot: Snapshot,
        id: PlayerId,
    ) -> GameClient {
        GameClient {
            players: Mutex::new(players),
            snapshots: Mutex::new(DoubleBuffer::new((snapshot, Instant::now()))),
            input: Input::default(),
            position_ball: Atomic::new(Point2::new(0.0, 0.0)),
            velocity_ball: Atomic::new(Vector2::new(0.0, 0.0)),
            client_player: id,
        }
    }

    pub fn tick(&self, dt: f32) {
        let position = self.position_ball.load(Ordering::Acquire);
        let velocity = self.velocity_ball.load(Ordering::Acquire);
        let displacement = self.input.position() - position;
        let velocity = velocity - SPRING_CONSTANT * displacement * dt;
        let position = position + velocity * dt;
        self.position_ball.store(position, Ordering::Release);
        self.velocity_ball.store(velocity, Ordering::Release);
    }

    pub fn update_position(&self, position: Point2<f32>) {
        self.input.set_position(position);
    }

    /// Handles a snapshot received from the server.
    ///
    /// If `new` is `true`, the snapshot is the most recent. Otherwise
    /// it is old.
    pub fn insert_snapshot(&self, snapshot: Snapshot, new: bool) {
        let mut snapshots = self.snapshots.lock();
        if new {
            // Update the predicted big ball position/velocity.
            let player = &snapshot.players[&self.client_player];
            self.position_ball
                .store(player.position_ball, Ordering::Release);
            self.velocity_ball
                .store(player.velocity_ball, Ordering::Release);
        }
        snapshots.insert((snapshot, Instant::now()));
        if new {
            snapshots.swap();
        }
    }

    /// This locks the snapshot mutex and returns a lazy struct that
    /// interpolates between the old and new snapshots.
    ///
    /// Keep in mind that the mutex remains locked while this struct
    /// is alive. To store the results long-term, convert the
    /// `InterpolatedSnapshot` instance into `Snapshot` with
    /// `.into()`. Doing so will allocate a new hash map, but unlock
    /// the mutex.
    pub fn interpolate_snapshot(&self, time: Instant) -> InterpolatedSnapshot {
        let snapshots = self.snapshots.lock();
        let new_time = snapshots.get().1;
        let old_time = snapshots.get_old().1;
        let alpha = if old_time == new_time || time < new_time {
            // First set of snapshots, or snapshot received after
            // frame started, so just use purely old.

            // TODO: this is *definitely* causing jittering
            0.0
        } else {
            debug_assert!(old_time < new_time);
            let span = new_time.duration_since(old_time).as_float_secs();

            time.duration_since(new_time).as_float_secs() / span
        };

        InterpolatedSnapshot {
            alpha: alpha as f32,
            snapshots,
            client_player: self.client_player,
            client_snapshot: PlayerSnapshot {
                position: self.input.position(),
                position_ball: self.position_ball.load(Ordering::Acquire),
                velocity_ball: self.velocity_ball.load(Ordering::Acquire),
            },
        }
    }
}

impl Default for GameServer {
    fn default() -> GameServer {
        GameServer {
            players: HashMap::default(),
            next_id: 0,
        }
    }
}

impl GameServer {
    pub fn tick(&mut self, dt: f32) {
        for player in self.players.values_mut() {
            let displacement = player.position_ball - player.position;
            player.velocity_ball -= SPRING_CONSTANT * displacement * dt;
            player.position_ball += player.velocity_ball * dt;
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            players: self
                .players
                .iter()
                .map(|(&id, player)| (id, player.snapshot()))
                .collect(),
        }
    }

    pub fn add_player(&mut self) -> PlayerId {
        let id = self.next_id;
        self.next_id += 1;
        self.players.insert(id, Player::with_random_color());
        id
    }

    pub fn remove_player(&mut self, id: PlayerId) {
        self.players.remove(&id);
    }
}
