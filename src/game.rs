use crate::double_buffer::DoubleBuffer;
use atomic::{Atomic, Ordering};
use cgmath::Point2;
use int_hash::IntHashMap;
use palette::LinSrgb;
use parking_lot::{Mutex, MutexGuard};
use rand::{thread_rng, Rng};
use serde_derive::{Deserialize, Serialize};

use std::ops::{Add, Mul, Sub};
use std::time::Instant;

pub type PlayerId = u16;

pub trait Interpolate {
    type Output;

    fn lerp(self, other: Self, alpha: f32) -> Self::Output;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Player {
    pub position: Point2<f32>,
    pub color: LinSrgb,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub position: Point2<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerClient {
    pub color: LinSrgb,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub players: IntHashMap<PlayerId, PlayerSnapshot>,
}

/// Be careful! This contains a mutex guard.
pub struct InterpolatedSnapshot<'a> {
    alpha: f32,
    snapshots: MutexGuard<'a, DoubleBuffer<(Snapshot, Instant)>>,
}

#[derive(Debug)]
pub struct Input {
    position: Atomic<Point2<f32>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    AddPlayer { id: PlayerId, player: PlayerClient },
    RemovePlayer(PlayerId),
    Snapshot(Snapshot),
}

#[derive(Debug)]
pub struct GameClient {
    pub players: Mutex<IntHashMap<PlayerId, PlayerClient>>,
    pub snapshots: Mutex<DoubleBuffer<(Snapshot, Instant)>>,
    pub input: Input,
}

#[derive(Debug)]
pub struct GameServer {
    pub players: IntHashMap<PlayerId, Player>,
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
    pub fn new() -> Player {
        let mut rng = thread_rng();
        Player {
            position: Point2::new(0.0, 0.0),
            color: LinSrgb::new(rng.gen(), rng.gen(), rng.gen()),
        }
    }

    pub fn as_client(&self) -> PlayerClient {
        PlayerClient { color: self.color }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            position: self.position,
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
            .map(
                move |(&id, new)| match self.snapshots.get_old().0.players.get(&id) {
                    Some(old) => (id, old.lerp(new, self.alpha)),
                    None => (id, new.clone()),
                },
            )
    }

    pub fn player(&self, id: PlayerId) -> Option<PlayerSnapshot> {
        self.snapshots.get().0.players.get(&id).map(|new| {
            match self.snapshots.get_old().0.players.get(&id) {
                Some(old) => old.lerp(new, self.alpha),
                None => new.clone(),
            }
        })
    }
}

impl GameClient {
    pub fn tick(&mut self) {
        /*for event in self.events.try_iter() {
            match event {
                Event::AddPlayer { id, player } => self.players.insert(id, player),
                Event::RemovePlayer(id) => self.players.remove(id),
                Event::Snapshot(snapshot) => {
                    if snapshot.time > self.snapshots.old().time {
                        self.snapshots.insert(snapshot);
                    }
                }
            }
        }*/
    }

    pub fn update_position(&self, position: Point2<f32>) {
        self.input.set_position(position);
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
        }
    }
}

impl GameServer {
    pub fn new() -> GameServer {
        GameServer {
            players: IntHashMap::default(),
            next_id: 0,
        }
    }

    /*pub fn tick(&mut self) {
        for (player, input) in inputs.iter_mut() {
            self.players.get_mut(input).unwrap().input(input);
            input.clear();
        }
    }*/

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
        self.players.insert(id, Player::new());
        id
    }
}
