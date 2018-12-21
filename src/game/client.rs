use crate::double_buffer::DoubleBuffer;
use crate::game::{
    step_dt, GetPlayer, Input, InputBuffer, InterpolatedSnapshot, PlayerId, PlayerState, Snapshot,
    StaticPlayerState,
};
use log::{debug, warn};
use nalgebra::Point2;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::iter;
use std::time::Instant;

#[derive(Debug)]
pub struct Player<'a> {
    static_state: &'a StaticPlayerState,
    state: PlayerState,
}

/// Constructs an iterator over all complete players, including
/// snapshot interpolation and predicted position for the client's
/// player.
pub struct Players<'a, 'b: 'a> {
    players: &'b HashMap<PlayerId, StaticPlayerState>,
    snapshot: InterpolatedSnapshot<'a>,
    predicted: (PlayerId, PlayerState),
}

pub struct Game {
    players: Mutex<HashMap<PlayerId, StaticPlayerState>>,
    snapshots: Mutex<DoubleBuffer<(Snapshot, Instant)>>,
    pub input_buffer: Mutex<InputBuffer>,
    /// Player id for this client.
    player_id: PlayerId,
    /// Predicted state of this client.
    predicted: Mutex<PlayerState>,
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

impl<'a, 'b: 'a> Players<'a, 'b> {
    /// This doesn't use the `IntoIterator` trait, since it needs impl
    /// Trait.
    ///
    /// If existential types ever get implemented, this can we swapped
    /// out.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(self) -> impl Iterator<Item = (PlayerId, Player<'b>)> + 'a {
        let Players {
            players,
            predicted,
            snapshot,
        } = self;

        let predicted = (
            predicted.0,
            Player {
                static_state: &players[&predicted.0],
                state: predicted.1,
            },
        );

        snapshot
            .players()
            .filter_map(move |(id, state)| {
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
            .chain(iter::once(predicted))
    }
}

impl Game {
    pub fn new(
        players: HashMap<PlayerId, StaticPlayerState>,
        snapshot: Snapshot,
        player_id: PlayerId,
        cursor: Point2<f32>,
    ) -> Game {
        Game {
            players: Mutex::new(players),
            snapshots: Mutex::new(DoubleBuffer::new((snapshot, Instant::now()))),
            input_buffer: Mutex::new(InputBuffer::new(Input { cursor })),
            player_id,
            predicted: Mutex::new(PlayerState::new(cursor)),
        }
    }

    /// Steps client prediction forward in time.
    pub fn tick(&self, dt: f32) {
        let mut predicted = self.predicted.lock();
        for dt in step_dt(dt, 1.0 / 60.0) {
            predicted.tick(dt);
        }
    }

    /// Updates the cursor position for this client player.
    pub fn update_cursor(&self, cursor: Point2<f32>) {
        self.predicted.lock().cursor = cursor;
        self.input_buffer
            .lock()
            .store_input(Input { cursor }, Instant::now());
    }

    /// Adds a new joined player.
    pub fn add_player(&self, id: PlayerId, static_state: StaticPlayerState) {
        self.players.lock().insert(id, static_state);
    }

    /// Removes a player.
    pub fn remove_player(&self, id: PlayerId) {
        if self.players.lock().remove(&id).is_none() {
            warn!("attempting to remove player that was never added ({})", id);
        }
    }

    /// Handles a snapshot received from the server.
    ///
    /// `input_delay` is the time in seconds since the last received
    /// input used to generate the snapshot.
    ///
    /// If `new` is `true`, the snapshot is the most recent. Otherwise
    /// it is old.
    pub fn insert_snapshot(&self, mut snapshot: Snapshot, mut input_delay: f32, new: bool) {
        // Remove the client's player from the snapshot, and reconcile
        // predicted state.
        if let Some(player) = snapshot.players.remove(&self.player_id) {
            if new {
                let input_buffer = self.input_buffer.lock();
                // Update the predicted big ball position/velocity.
                let mut ball = player.ball;
                let mut cursor = player.cursor;
                // Fast-forward through all unacknowledged inputs.
                for (input, dt) in input_buffer.inputs() {
                    let dt_adjusted = (dt - input_delay).max(0.0);
                    // Only take input delay into account for the
                    // first input.
                    input_delay -= dt.min(input_delay);

                    debug!("replaying input: {:?} {}", input, dt_adjusted);
                    for dt in step_dt(dt_adjusted, 1.0 / 60.0) {
                        ball.tick(dt, cursor);
                    }
                    cursor = input.cursor;
                }
                let dt = (input_buffer.delay(Instant::now()) - input_delay).max(0.0);
                for dt in step_dt(dt, 1.0 / 60.0) {
                    ball.tick(dt, cursor);
                }
                let mut predicted = self.predicted.lock();
                predicted.ball = ball;
            }
        }

        let mut snapshots = self.snapshots.lock();
        snapshots.insert((snapshot, Instant::now()));
        if new {
            snapshots.swap();
        }
    }

    /// Interpolates the two most recent snapshots and processes the
    /// resulting set of players.
    ///
    /// Note that mutexes will be locked for the duration of
    /// `process`, so don't block or anything.
    pub fn players<O, F: FnOnce(Players) -> O>(&self, time: Instant, process: F) -> O {
        let snapshots = self.snapshots.lock();
        let &(ref old, old_time) = snapshots.get_old();
        let &(ref new, new_time) = snapshots.get();
        let alpha = if old_time == new_time || time < new_time {
            // First set of snapshots, or snapshot received after
            // frame started, so just use purely old.

            // TODO: this is *definitely* causing jittering
            0.0
        } else {
            debug_assert!(old_time < new_time);
            let span = new_time.duration_since(old_time).as_float_secs();

            time.duration_since(new_time).as_float_secs() / span
        } as f32;

        let snapshot = InterpolatedSnapshot::new(alpha, old, new);
        let players = self.players.lock();
        let players = Players {
            players: &*players,
            snapshot,
            predicted: (self.player_id, *self.predicted.lock()),
        };
        process(players)
    }
}
