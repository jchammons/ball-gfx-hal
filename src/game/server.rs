use crate::game::{
    clamp_cursor,
    step_dt,
    GetPlayer,
    PlayerId,
    PlayerState,
    Snapshot,
    StaticPlayerState,
    BALL_RADIUS,
};
use log::{debug, trace};
use nalgebra::Point2;
use ord_subset::OrdSubsetIterExt;
use palette::{LabHue, Lch};
use rand::{thread_rng, Rng};
use smallvec::SmallVec;
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
            // Calculate individual ball spring physics.
            for player in self.players.values_mut() {
                player.state.tick(dt);
            }

            // Check for collisions between balls.
            let mut collisions = SmallVec::<[_; 2]>::new();
            for (&id_a, a) in self.players.iter() {
                let a = &a.state.ball;
                for (&id_b, b) in self.players.iter() {
                    let b = &b.state.ball;

                    // This ensures every unordered pair only gets checked
                    // once.
                    if id_a < id_b {
                        let a_to_b = b.position - a.position;
                        let dist_sq = a_to_b.norm_squared();
                        trace!(
                            "distance from {} to {}: {}",
                            id_a,
                            id_b,
                            dist_sq.sqrt()
                        );
                        if dist_sq < 4.0 * BALL_RADIUS * BALL_RADIUS {
                            let penetration_dist =
                                2.0 * BALL_RADIUS - dist_sq.sqrt();
                            debug!(
                                "collision between {} and {} ({})",
                                id_a, id_b, penetration_dist
                            );
                            let vel = a_to_b *
                                (b.velocity - a.velocity).dot(&a_to_b) /
                                dist_sq;
                            // Normalize using previously computed distance.
                            collisions.push((
                                id_a,
                                id_b,
                                penetration_dist,
                                a_to_b / dist_sq.sqrt(),
                                vel,
                            ));
                        }
                    }
                }
            }

            // Process collisions
            for (id_a, id_b, penetration, a_to_b, vel) in collisions.into_iter()
            {
                let bounce = |player: &mut Player, sign| {
                    let position = &mut player.state.ball.position;
                    let velocity = &mut player.state.ball.velocity;

                    // Get penetration along the velocity vector.
                    let penetration_vel =
                        0.5 * velocity.dot(&a_to_b).abs() * penetration;
                    // Move player out of collision (* 0.5 because there are two
                    // balls).
                    *position -= velocity.normalize() * penetration_vel;
                    // Update velocity
                    *velocity -= sign * vel;
                    // Get penetration along the new velocity vector.
                    let penetration_vel =
                        0.5 * velocity.dot(&a_to_b).abs() * penetration;
                    // Repeat remaining movement in the new direction.
                    *position += velocity.normalize() * penetration_vel;
                };
                bounce(self.players.get_mut(&id_a).unwrap(), -1.0);
                bounce(self.players.get_mut(&id_b).unwrap(), 1.0);
            }

            // Check for collisions with walls.
            for (&id, player) in self.players.iter_mut() {
                let position = &mut player.state.ball.position;
                let velocity = &mut player.state.ball.velocity;

                // Determine if player's ball intersects the boundary.
                let distance_sq = (*position - Point2::origin()).norm_squared();
                if distance_sq > (1.0 - BALL_RADIUS) * (1.0 - BALL_RADIUS) {
                    let distance = distance_sq.sqrt();
                    let normal = -(*position - Point2::origin()) / distance;
                    let penetration = distance - (1.0 - BALL_RADIUS) + 0.001;
                    debug!(
                        "collision between {} and boundary circle ({})",
                        id, penetration
                    );
                    // Get penetration along the velocity vector.
                    let penetration_vel =
                        velocity.dot(&normal).abs() * penetration;
                    // Move player out of collision.
                    *position += normal * penetration;
                    // Update velocity, reflecting across the normal.
                    *velocity += 2.0 * normal * velocity.dot(&normal).abs();
                    // Repeat remaining movement in the new direction.
                    *position += velocity.normalize() * penetration_vel;
                }
            }
        }
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
            state: PlayerState::new(clamp_cursor(cursor)),
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
