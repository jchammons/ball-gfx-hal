use crate::game::{
    Event,
    GameSettings,
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
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub players: HashMap<PlayerId, StaticPlayerState>,
    pub last_round: Option<RoundState>,
    pub round: RoundState,
    pub round_duration: f32,
    snapshots: VecDeque<(Snapshot, Instant)>,
    settings: GameSettings,
    settings_handle: Arc<SettingsHandle>,
    cursor: Arc<Mutex<Point2<f32>>>,
    events: Receiver<Event>,
    /// Player id for this client.
    player_id: PlayerId,
}

pub struct SettingsHandle {
    dirty: AtomicBool,
    settings: Mutex<GameSettings>,
}

pub struct GameHandle {
    events: Sender<Event>,
    cursor: Arc<Mutex<Point2<f32>>>,
    pub settings: Arc<SettingsHandle>,
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
        let _ = self.events.send(event);
    }

    pub fn latest_input(&self) -> Input {
        Input {
            cursor: *self.cursor.lock(),
        }
    }
}

impl SettingsHandle {
    pub fn dirty(&self) -> Option<GameSettings> {
        if self.dirty.load(Ordering::SeqCst) {
            self.dirty.store(false, Ordering::SeqCst);
            let settings = self.settings.lock();
            Some(*settings)
        } else {
            None
        }
    }

    pub fn settings(&self) -> GameSettings {
        let settings = self.settings.lock();
        *settings
    }
}

impl Game {
    pub fn new(
        players: HashMap<PlayerId, StaticPlayerState>,
        snapshot: Snapshot,
        round: RoundState,
        round_duration: f32,
        settings: GameSettings,
        player_id: PlayerId,
        cursor: Point2<f32>,
    ) -> (Game, GameHandle) {
        let mut snapshots = VecDeque::new();
        snapshots.push_back((snapshot, Instant::now()));
        let (events_tx, events_rx) = channel::bounded(16);
        let cursor = Arc::new(Mutex::new(cursor));
        let settings_handle = Arc::new(SettingsHandle {
            dirty: AtomicBool::new(false),
            settings: Mutex::new(settings),
        });
        let game = Game {
            players,
            snapshots,
            cursor: cursor.clone(),
            events: events_rx,
            round,
            last_round: None,
            round_duration,
            player_id,
            settings,
            settings_handle: Arc::clone(&settings_handle),
        };
        let handle = GameHandle {
            cursor,
            events: events_tx,
            settings: settings_handle,
        };
        (game, handle)
    }

    pub fn settings(&self) -> &GameSettings {
        &self.settings
    }

    /// Modifies the game settings and flags the change to be sent to
    /// the server.
    pub fn set_settings(&mut self, settings: GameSettings) {
        self.settings = settings;
        let mut shared = self.settings_handle.settings.lock();
        *shared = settings;
        drop(shared);
        self.settings_handle.dirty.store(true, Ordering::SeqCst);
    }

    /// Handles events from the server.
    pub fn handle_events(&mut self) {
        for event in self.events.try_iter() {
            match event {
                Event::RoundState(round) => {
                    info!("transitioning to round state {:?}", round);
                    self.last_round = Some(self.round);
                    self.round_duration = 0.0;
                    self.round = round;
                },
                Event::Settings(settings) => {
                    self.settings = settings;
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
        self.round_duration += dt;
        self.handle_events();
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

    /// Removes any old snapshots that are no longer needed for
    /// interpolation.
    pub fn clean_old_snapshots(&mut self, time: Instant, delay: f32) {
        let delayed_time = time - SNAPSHOT_RATE.mul_f64(delay.into());
        while self.snapshots.len() > 1 && delayed_time > self.snapshots[1].1 {
            // Yay for short circuiting &&
            self.snapshots.pop_front();
        }
    }

    /// Interpolates snapshots with delay and returns the resulting
    /// set of player states.
    pub fn interpolated_players(
        &self,
        time: Instant,
        cursor: Point2<f32>,
        delay: f32,
    ) -> Players<InterpolatedSnapshot> {
        let delayed_time = time - SNAPSHOT_RATE.mul_f64(delay.into());

        let (ref old, old_time) = self.snapshots[0];
        let snapshot = match self.snapshots.get(1) {
            Some(&(ref new, new_time)) => {
                let alpha = if delayed_time > old_time {
                    let span = new_time.duration_since(old_time);
                    delayed_time.duration_since(old_time).div_duration_f32(span)
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
