use crate::game::{
    physics::{self, check_collision, resolve_collision},
    step_dt,
    Ball,
    Event,
    GameSettings,
    GetPlayer,
    PlayerId,
    PlayerState,
    RoundState,
    Snapshot,
    StaticPlayerState,
};
use log::info;
use nalgebra::Point2;
use ord_subset::OrdSubsetIterExt;
use palette::{LabHue, Lch};
use rand::{thread_rng, Rng};
use smallvec::SmallVec;
use std::collections::HashMap;

/// Number of hue candidates to generate for each existing player
/// sample.
const HUE_CANDIDATES_PER_SAMPLE: usize = 4;

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

#[test]
fn hue_distance_lt_half() {
    assert_eq!(hue_distance(0.2, 0.5), 0.3);
    assert_eq!(hue_distance(0.5, 0.2), 0.3);
}

#[test]
fn hue_distance_gt_half() {
    assert_eq!(hue_distance(0.2, 0.9), 0.3);
    assert_eq!(hue_distance(0.9, 0.2), 0.3);
}

#[derive(Clone, Debug)]
pub struct Player {
    pub state: PlayerState,
    pub static_state: StaticPlayerState,
    hue: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Game {
    pub players: HashMap<PlayerId, Player>,
    pub round: RoundState,
    pub round_duration: f32,
    pub settings: GameSettings,
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

    /// Sets the location of a player's cursor.
    ///
    /// If this is before the round is started, the starting position
    /// of that player's ball is also updated.
    ///
    /// Returns `false` if there is no player corresponding to the id.
    pub fn set_player_cursor(
        &mut self,
        id: PlayerId,
        cursor: Point2<f32>,
    ) -> bool {
        let player = match self.players.get_mut(&id) {
            Some(player) => player,
            None => return false,
        };

        if self.round.running() {
            player.state.set_cursor(cursor);
        } else {
            // In any non-round states, always set players alive.
            player.state.cursor = Some(cursor);
        }

        if !self.round.running() {
            player.state.ball = Ball::starting(cursor, &self.settings);
        }
        true
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

    fn switch_round(&mut self, round: RoundState) {
        self.round = round;
        self.round_duration = 0.0;
    }

    /// Steps the whole game world forward in time.
    pub fn tick(&mut self, dt: f32) -> impl Iterator<Item = Event> {
        self.round_duration += dt;
        let transition = match self.round {
            RoundState::Lobby => None,
            RoundState::Waiting => {
                if self.round_duration > 3.0 {
                    Some(RoundState::Round)
                } else {
                    None
                }
            },
            RoundState::Round => None,
            RoundState::RoundEnd => {
                if self.round_duration > 2.0 {
                    // If a player is still alive, they win.
                    let winner = self
                        .players
                        .iter()
                        .filter(|(_, player)| player.state.alive())
                        .next()
                        .map(|(&id, _)| id);
                    Some(RoundState::Winner(winner))
                } else {
                    None
                }
            },
            RoundState::Winner(_) => {
                if self.round_duration > 1.0 {
                    Some(RoundState::Waiting)
                } else {
                    None
                }
            },
        };
        if let Some(round) = transition {
            self.switch_round(round);
        }

        if !self.round.running() {
            // No simulation happens if not running.
            return transition.map(Event::RoundState).into_iter();
        }

        // To avoid borrow issues.
        let settings = &self.settings;

        for dt in step_dt(dt, 1.0 / 60.0) {
            // Calculate individual ball spring physics.
            for player in self.players.values_mut() {
                player.state.tick(dt, settings);
            }

            // Check for collisions between balls.
            let mut collisions = SmallVec::<[_; 2]>::new();
            for (&id_a, a) in self.players.iter() {
                for (&id_b, b) in self.players.iter() {
                    // This ensures every unordered pair only gets checked
                    // once.
                    if id_a < id_b {
                        let mut circle_a =
                            physics::ball(a.state.ball, settings);
                        let mut circle_b =
                            physics::ball(b.state.ball, settings);
                        if resolve_collision(&mut circle_a, &mut circle_b) {
                            collisions.push((id_a, circle_a));
                            collisions.push((id_b, circle_b));
                        }
                    }
                }
            }

            // Process collisions updates.
            for (id, circle) in collisions.into_iter() {
                self.players.get_mut(&id).unwrap().state.ball = circle.into();
            }

            // Check for collisions with walls.
            for (&id, player) in self.players.iter_mut() {
                let alive = player.state.alive();
                let mut circle = physics::ball(player.state.ball, settings);
                if resolve_collision(
                    &mut circle,
                    &mut physics::bounds(settings),
                ) {
                    player.state.ball = circle.into();
                    if alive {
                        info!("{} killed {}", id, id);
                        player.state.cursor = None;
                    }
                }
            }

            let mut deaths = SmallVec::<[_; 1]>::new();

            // Check for collisions with cursor.
            for (&id, player) in self.players.iter() {
                if let Some(cursor) = player.state.cursor {
                    let circle_cursor = physics::cursor(cursor, settings);
                    for (&id_ball, player_ball) in self.players.iter() {
                        if id == id_ball && !settings.kill_own_cursor {
                            // Don't let players kill themselves
                            // unless that setting is on.
                            continue;
                        }

                        let circle_ball =
                            physics::ball(player_ball.state.ball, settings);
                        if check_collision(&circle_cursor, &circle_ball) {
                            info!("{} killed {}", id_ball, id);
                            deaths.push(id);
                        }
                    }
                }
            }

            // Process collisions with cursor.
            for id in deaths.into_iter() {
                self.players.get_mut(&id).unwrap().state.cursor = None;
            }
        }

        if let RoundState::Round = self.round {
            // Start the round ending if there are one or less players
            // still alive.
            let num_alive = self
                .players
                .values()
                .filter(|player| player.state.alive())
                .count();
            if num_alive <= 1 {
                self.switch_round(RoundState::RoundEnd);
                return Some(Event::RoundState(self.round)).into_iter();
            }
        }
        transition.map(Event::RoundState).into_iter()
    }

    /// Adds a new player and returns the id of the added.
    pub fn add_player(
        &mut self,
        cursor: Point2<f32>,
    ) -> (PlayerId, impl Iterator<Item = Event>) {
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
                        .map(|player| hue_distance(hue, player.hue))
                        .ord_subset_min()
                        .unwrap()
                })
                .unwrap()
        };
        info!("selected hue {}", hue);
        let lab_hue = LabHue::from_degrees(hue * 360.0);
        let static_state = StaticPlayerState {
            color: Lch::new(75.0, 80.0, lab_hue).into(),
        };
        let player = Player {
            state: PlayerState::new(cursor, &self.settings),
            static_state: static_state.clone(),
            hue,
        };

        debug_assert!(!self.players.contains_key(&id));
        self.players.insert(id, player);

        let mut events = SmallVec::<[_; 2]>::new();
        events.push(Event::NewPlayer {
            id,
            static_state,
        });
        // Queue a waiting round if there are two or more players.
        if let RoundState::Lobby = self.round {
            if self.players.len() >= 2 {
                self.switch_round(RoundState::Waiting);
                events.push(Event::RoundState(self.round));
            }
        }

        (id, events.into_iter())
    }

    /// Removes the player with a given id.
    pub fn remove_player(
        &mut self,
        id: PlayerId,
    ) -> impl Iterator<Item = Event> {
        self.players.remove(&id);

        let mut events = SmallVec::<[_; 2]>::new();
        events.push(Event::RemovePlayer(id));
        // If there are less than two players left, stop the round.
        if self.players.len() < 2 {
            self.switch_round(RoundState::Lobby);
            events.push(Event::RoundState(self.round))
        }

        events.into_iter()
    }
}
