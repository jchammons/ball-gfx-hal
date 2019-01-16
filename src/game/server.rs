use crate::game::{
    step_dt,
    GetPlayer,
    PlayerId,
    PlayerState,
    Snapshot,
    StaticPlayerState,
};
use nalgebra::Point2;
use ord_subset::OrdSubsetIterExt;
use palette::{LabHue, Lch};
use rand::{thread_rng, Rng};
use std::collections::HashMap;

/// Number of hue candidates to generate for each existing player
/// sample.
const HUE_CANDIDATES_PER_SAMPLE: usize = 8;

/// Gets the distance between two hue values, specified from 0 to 1.
fn hue_distance(a: f32, b: f32) -> f32 {
    let dist = (a - b).abs();
    if dist > 0.5 {
        // Wrap around the outside of the circle.
        1.0 - dist
    } else {
        dist
    }
}

#[derive(Clone, Debug)]
pub struct Player {
    pub state: PlayerState,
    pub static_state: StaticPlayerState,
    hue: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Game {
    players: HashMap<PlayerId, Player>,
    next_id: PlayerId,
}

impl<'a> GetPlayer for &'a Player {
    type State = &'a PlayerState;
    type StaticState = &'a StaticPlayerState;

    fn state(self) -> &'a PlayerState {
        &self.state
    }

    fn static_state(self) -> &'a StaticPlayerState {
        &self.static_state
    }
}

impl Game {
    /// Returns an iterator over the players.
    pub fn players(&self) -> impl Iterator<Item = (PlayerId, &Player)> {
        self.players.iter().map(|(&id, player)| (id, player))
    }

    /// Gets a mutable reference to a player by id.
    pub fn player_mut(&mut self, id: PlayerId) -> Option<&mut Player> {
        self.players.get_mut(&id)
    }

    /// Generates a snapshot of the current game state.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            players: self
                .players
                .iter()
                .map(|(&id, player)| (id, player.state))
                .collect(),
        }
    }

    /// Steps the whole game world forward in time.
    pub fn tick(&mut self, dt: f32) {
        for dt in step_dt(dt, 1.0 / 60.0) {
            for player in self.players.values_mut() {
                player.state.tick(dt);
            }
        }

        // TODO: collisions
    }

    /// Adds a new player and returns the id and state of the added
    /// player.
    pub fn add_player(&mut self, cursor: Point2<f32>) -> (PlayerId, &Player) {
        let mut rng = thread_rng();
        let id = self.next_id;
        self.next_id += 1;

        // Generate a new player with random color.
        let hue = if self.players.is_empty() {
            // There weren't any existing players, so just use uniform
            // RNG.
            rng.gen()
        } else {
            // Otherwise use Mitchell's best-candidate algorithm for
            // picking the hue. The main disadvantage of this is that
            // it's O(n^2) wrt the number of players. I don't think
            // there are ever enough players for this to matter
            // though.
            let num_samples = self.players.len() * HUE_CANDIDATES_PER_SAMPLE;
            let samples = (0..num_samples).map(|_| rng.gen::<f32>());
            // Unwrap is okay because we already know self.players and
            // samples are both not empty.
            samples
                .ord_subset_max_by_key(|&hue| {
                    self.players
                        .values()
                        .map(|player| player.hue)
                        .ord_subset_min_by_key(|&player_hue| {
                            hue_distance(hue, player_hue)
                        })
                        .unwrap()
                })
                .unwrap()
        };
        let lab_hue = LabHue::from_degrees(hue * 360.0);
        let player = Player {
            state: PlayerState::new(cursor),
            static_state: StaticPlayerState {
                color: Lch::new(75.0, 80.0, lab_hue).into(),
            },
            hue,
        };

        debug_assert!(!self.players.contains_key(&id));
        let player = self.players.entry(id).or_insert(player);
        (id, player)
    }

    /// Removes the player with a given id.
    pub fn remove_player(&mut self, id: PlayerId) {
        self.players.remove(&id);
    }
}
