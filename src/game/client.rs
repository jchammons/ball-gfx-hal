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
use crossbeam::channel::{self, Receiver, Sender};
use log::{info, warn};
use nalgebra::Point2;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

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
    players: HashMap<PlayerId, StaticPlayerState>,
    snapshots: VecDeque<(Snapshot, Instant)>,
    round: RoundState,
    cursor: Arc<Mutex<Point2<f32>>>,
    events: Receiver<Event>,
    /// Player id for this client.
    player_id: PlayerId,
}

pub struct GameHandle {
    events: Sender<Event>,
    cursor: Arc<Mutex<Point2<f32>>>,
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

impl GameHandle {
    pub fn event(&self, event: Event) {
        self.events.send(event).unwrap();
    }

    pub fn latest_input(&self) -> Input {
        Input {
            cursor: *self.cursor.lock(),
        }
    }
}

impl Game {
    pub fn new(
        players: HashMap<PlayerId, StaticPlayerState>,
        snapshot: Snapshot,
        player_id: PlayerId,
        cursor: Point2<f32>,
    ) -> (Game, GameHandle) {
        let mut snapshots = VecDeque::new();
        snapshots.push_back((snapshot, Instant::now()));
        let (events_tx, events_rx) = channel::bounded(16);
        let cursor = Arc::new(Mutex::new(cursor));
        let game = Game {
            players,
            snapshots,
            cursor: cursor.clone(),
            events: events_rx,
            round: RoundState::default(),
            player_id,
        };
        let handle = GameHandle {
            cursor,
            events: events_tx,
        };
        (game, handle)
    }

    /// Handles events from the server.
    pub fn handle_events(&mut self) {
        for event in self.events.try_iter() {
            match event {
                Event::RoundState(round) => {
                    info!("transitioning to round state {:?}", round);
                    self.round;
                },
                Event::NewPlayer {
                    id,
                    static_state,
                } => {
                    info!("new player {}", id);
                    self.players.insert(id, static_state);
                },
                Event::RemovePlayer(id) => {
                    info!("removing player {}", id);
                    if self.players.remove(&id).is_none() {
                        warn!(
                            "attempting to remove player that was never added \
                             ({})",
                            id
                        );
                    }
                },
                Event::Snapshot(snapshot) => {
                    self.snapshots.push_back((snapshot, Instant::now()));
                },
            }
        }
    }

    /// Steps client prediction forward in time.
    pub fn tick(&mut self, dt: f32) {
        self.handle_events();
        self.round.tick(dt);
    }

    /// Updates the cursor position for this client player.
    pub fn update_cursor(&self, cursor: Point2<f32>) {
        *self.cursor.lock() = cursor;
    }

    /// Returns the set of players corresponding to the most recent
    /// snapshot.
    pub fn latest_players(&self) -> Players<&Snapshot> {
        let (snapshot, _) = &self.snapshots[self.snapshots.len() - 1];
        Players {
            players: &self.players,
            snapshot,
            predicted: None,
        }
    }

    /// Interpolates snapshots with delay and returns the resulting
    /// set of player states.
    pub fn interpolated_players(
        &mut self,
        time: Instant,
        cursor: Point2<f32>,
        delay: f32,
    ) -> Players<InterpolatedSnapshot> {
        let delayed_time = time - SNAPSHOT_RATE.mul_f64(delay.into());

        // Get rid of old snapshots.
        while self.snapshots.len() > 1 && delayed_time > self.snapshots[1].1 {
            // Yay for short circuiting &&
            self.snapshots.pop_front();
        }

        let (ref old, old_time) = self.snapshots[0];
        let snapshot = match self.snapshots.get(1) {
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

        Players {
            players: &self.players,
            snapshot,
            predicted: Some((self.player_id, predicted)),
        }
    }
}
