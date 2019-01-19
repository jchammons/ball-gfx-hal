use crate::game::{
    Event,
    GetPlayer,
    Input,
    InterpolatedSnapshot,
    PlayerId,
    PlayerState,
    RoundState,
    Snapshot,
    SnapshotView,
    StaticPlayerState,
};
use crate::networking::SNAPSHOT_RATE;
use log::{info, warn};
use nalgebra::Point2;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Player<'a> {
    static_state: &'a StaticPlayerState,
    state: Cow<'a, PlayerState>,
}

/// Constructs an iterator over all complete players with a given
/// snapshot view.
pub struct Players<'a, S> {
    players: &'a HashMap<PlayerId, StaticPlayerState>,
    snapshot: S,
    predicted: Option<(PlayerId, PlayerState)>,
}

pub struct Game {
    players: Mutex<HashMap<PlayerId, StaticPlayerState>>,
    snapshots: Mutex<VecDeque<(Snapshot, Instant)>>,
    round: Mutex<RoundState>,
    cursor: Mutex<Point2<f32>>,
    /// Player id for this client.
    player_id: PlayerId,
}

impl<'a, 'b> GetPlayer for &'b Player<'a> {
    type State = &'b PlayerState;
    type StaticState = &'a StaticPlayerState;

    fn state(self) -> &'b PlayerState {
        &self.state
    }

    fn static_state(self) -> &'a StaticPlayerState {
        &self.static_state
    }
}

impl<'a, S: SnapshotView<'a>> Players<'a, S> {
    /// This doesn't use the `IntoIterator` trait, since it needs impl
    /// Trait.
    ///
    /// If existential types ever get implemented, this can we swapped
    /// out.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(
        self,
    ) -> impl Iterator<Item = (PlayerId, Player<'a>)> + 'a {
        let Players {
            players,
            predicted,
            snapshot,
        } = self;

        let predicted_id = predicted.map(|(id, _)| id);
        let predicted = predicted.map(|(id, state)| {
            (
                id,
                Player {
                    static_state: &players[&id],
                    state: Cow::Owned(state),
                },
            )
        });

        snapshot
            .players()
            .filter_map(move |(id, state)| {
                if predicted_id
                    .map(|predicted_id| id == predicted_id)
                    .unwrap_or(false)
                {
                    return None;
                }
                // Only handle players who also have static state
                // stored. If the static state isn't there, the player
                // must have already been removed, but we haven't received
                // the new snapshot.
                players.get(&id).map(|static_state| {
                    (
                        id,
                        Player {
                            static_state,
                            state,
                        },
                    )
                })
            })
            .chain(predicted)
    }
}

impl Game {
    pub fn new(
        players: HashMap<PlayerId, StaticPlayerState>,
        snapshot: Snapshot,
        player_id: PlayerId,
        cursor: Point2<f32>,
    ) -> Game {
        let mut snapshots = VecDeque::new();
        snapshots.push_back((snapshot, Instant::now()));
        Game {
            players: Mutex::new(players),
            snapshots: Mutex::new(snapshots),
            cursor: Mutex::new(cursor),
            round: Mutex::new(RoundState::default()),
            player_id,
            // predicted: Mutex::new(PlayerState::new(clamp_cursor(cursor))),
        }
    }

    /// Handles an event from the server.
    pub fn handle_event(&self, event: Event) {
        match event {
            Event::RoundState(round) => {
                info!("transitioning to round state {:?}", round);
                *self.round.lock() = round
            },
            Event::NewPlayer {
                id,
                static_state,
            } => {
                info!("new player {}", id);
                self.players.lock().insert(id, static_state);
            },
            Event::RemovePlayer(id) => {
                info!("removing player {}", id);
                if self.players.lock().remove(&id).is_none() {
                    warn!(
                        "attempting to remove player that was never added ({})",
                        id
                    );
                }
            },
            Event::Snapshot(snapshot) => {
                let mut snapshots = self.snapshots.lock();
                snapshots.push_back((snapshot, Instant::now()));
            },
        }
    }

    /// Steps client prediction forward in time.
    pub fn tick(&self, dt: f32) {
        self.round.lock().tick(dt);
    }

    /// Updates the cursor position for this client player.
    pub fn update_cursor(&self, cursor: Point2<f32>) {
        *self.cursor.lock() = cursor;
    }

    /// Gets the most recent input to send to the server.
    pub fn latest_input(&self) -> Input {
        Input {
            cursor: *self.cursor.lock(),
        }
    }

    /// Processes the set of players corresponding to the most recent snapshot.
    ///
    /// Note that mutexes will be locked for the duration of
    /// `process`, so don't block or anything.
    pub fn latest_players<O, F: FnOnce(Players<&Snapshot>) -> O>(
        &self,
        process: F,
    ) -> O {
        let snapshots = self.snapshots.lock();
        // Why is there no .last() for VecDeque?
        let (snapshot, _) = &snapshots[snapshots.len() - 1];
        let players = self.players.lock();
        let players = Players {
            players: &*players,
            snapshot,
            predicted: None,
        };
        process(players)
    }

    /// Interpolates snapshots with delay and processes the resulting
    /// set of players.
    ///
    /// Note that mutexes will be locked for the duration of
    /// `process`, so don't block or anything.
    pub fn interpolated_players<
        O,
        F: FnOnce(Players<InterpolatedSnapshot>) -> O,
    >(
        &self,
        time: Instant,
        cursor: Point2<f32>,
        delay: f32,
        process: F,
    ) -> O {
        let mut snapshots = self.snapshots.lock();
        let delayed_time =
            time - Duration::from_float_secs(f64::from(delay * SNAPSHOT_RATE));

        // Get rid of old snapshots.
        while snapshots.len() > 1 && delayed_time > snapshots[1].1 {
            // Yay for short circuiting &&
            snapshots.pop_front();
        }

        let (ref old, old_time) = snapshots[0];
        let snapshot = match snapshots.get(1) {
            Some(&(ref new, new_time)) => {
                let alpha = if delayed_time > old_time {
                    let span = new_time.duration_since(old_time);
                    delayed_time.duration_since(old_time).div_duration(span)
                } else {
                    0.0
                };
                // If delayed_time is newer than both of the
                // snapshots, one of them would have been removed
                // earlier, so alpha should always be [0, 1].
                debug_assert!(alpha < 1.0);
                InterpolatedSnapshot::new(alpha as f32, old, new)
            },
            None => InterpolatedSnapshot::new(0.0, old, old),
        };

        let mut predicted = *snapshot.get(self.player_id).unwrap();
        predicted.set_cursor(cursor);

        let players = self.players.lock();
        let players = Players {
            players: &*players,
            snapshot,
            predicted: Some((self.player_id, predicted)),//*self.predicted.lock()),
        };
        process(players)
    }
}
