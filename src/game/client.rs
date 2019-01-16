use crate::game::{
    step_dt,
    GetPlayer,
    Input,
    InputBuffer,
    InterpolatedSnapshot,
    PlayerId,
    PlayerState,
    Snapshot,
    SnapshotView,
    StaticPlayerState,
};
use crate::networking::server;
use log::warn;
use nalgebra::Point2;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Player<'a> {
    static_state: &'a StaticPlayerState,
    state: PlayerState,
}

/// Constructs an iterator over all complete players with a given
/// snapshot view.
pub struct Players<'a, S> {
    players: &'a HashMap<PlayerId, StaticPlayerState>,
    snapshot: S,
    // predicted: (PlayerId, PlayerState),
}

pub struct Game {
    players: Mutex<HashMap<PlayerId, StaticPlayerState>>,
    snapshots: Mutex<VecDeque<(Snapshot, Instant)>>,
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

impl<'a: 'b, 'b, S: SnapshotView<'b>> Players<'a, S> {
    /// This doesn't use the `IntoIterator` trait, since it needs impl
    /// Trait.
    ///
    /// If existential types ever get implemented, this can we swapped
    /// out.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(
        self,
    ) -> impl Iterator<Item = (PlayerId, Player<'a>)> + 'b {
        let Players {
            players,
            // predicted,
            snapshot,
        } = self;

        // let predicted = (
        // predicted.0,
        // Player {
        // static_state: &players[&predicted.0],
        // state: predicted.1,
        // },
        // );

        snapshot.players().filter_map(move |(id, state)| {
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
        //.chain(iter::once(predicted))
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
            input_buffer: Mutex::new(InputBuffer::new(Input {
                cursor,
            })),
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
        self.input_buffer.lock().store_input(
            Input {
                cursor,
            },
            Instant::now(),
        );
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
    pub fn insert_snapshot(
        &self,
        snapshot: Snapshot,
        _input_delay: f32,
    ) {
        // TODO!
        
        // Remove the client's player from the snapshot, and reconcile
        // predicted state.
        /*if let Some(player) = snapshot.players.get(&self.player_id) {
            let input_buffer = self.input_buffer.lock();
            // Update the predicted big ball position/velocity.
            let mut ball = player.ball;
            let mut cursor = player.cursor;
            /*
            // Fast-forward through all unacknowledged inputs. // 
            for (input, dt) in input_buffer.inputs() { // 
            // Only take input delay into account for the // 
            // first input. // 
            let dt = (dt - input_delay).max(0.0); // 
            input_delay -= dt; // 
            // 
            debug!("replaying input: {:?} {}", input, dt); // 
            for dt in step_dt(dt, 1.0 / 60.0) { // 
            ball.tick(dt, cursor); // 
        } // 
            cursor = input.cursor; // 
        } // 
            let dt = // 
            (input_buffer.delay(Instant::now()) - input_delay).max(0.0); // 
            for dt in step_dt(dt, 1.0 / 60.0) { // 
            ball.tick(dt, cursor); // 
        } */
            // Since the protocol only sends over the latest
            // input, that's the only thing we need to predict.
            /*let dt: f32 = input_buffer.inputs().map(|(_, dt)| dt).sum();
            if let Some((input, _)) = input_buffer.inputs().last() {
                //let dt = (dt - input_delay).max(0.0);
                //let dt = input_delay;
                cursor = input.cursor;
                // TODO possibly just use calculus here instead
                for dt in step_dt(dt, 1.0 / 60.0) {
                    ball.tick(dt, cursor);
                }
            }*/
            let mut predicted = self.predicted.lock();
            predicted.ball = ball;
        }*/

        let mut snapshots = self.snapshots.lock();
        snapshots.push_back((snapshot, Instant::now()));
    }

    /// Processes the set of players corresponding to the most recent snapshot.
    ///
    /// Note that mutexes will be locked for the duration of
    /// `process`, so don't block or anything.
    pub fn latest_players<O, F: FnOnce(Players<&Snapshot>) -> O>(
        &self,
        process: F,
    ) -> O {
        let mut snapshots = self.snapshots.lock();
        // Why is there no .last() for VecDeque?
        let (snapshot, _) = &snapshots[snapshots.len() - 1];
        let players = self.players.lock();
        let players = Players {
            players: &*players,
            snapshot,
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
        delay: f32,
        process: F,
    ) -> O {
        let mut snapshots = self.snapshots.lock();
        let delayed_time = time -
            Duration::from_float_secs((delay * server::SNAPSHOT_RATE) as f64);

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

        let players = self.players.lock();
        let players = Players {
            players: &*players,
            snapshot,
            // predicted: (self.player_id, *self.predicted.lock()),
        };
        process(players)
    }
}
