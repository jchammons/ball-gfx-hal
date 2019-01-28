use crate::graphics::Circle;
use enum_kinds::EnumKind;
use nalgebra::{self, Point2, Vector2};
use palette::LinSrgb;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::iter;
use std::ops::Deref;

pub mod client;
pub mod physics;
pub mod server;
pub mod snapshot;

pub use self::snapshot::*;

pub type PlayerId = u16;

/// Finite state machine for the round state.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, EnumKind)]
#[enum_kind(RoundStateKind)]
pub enum RoundState {
    /// Less than two players, so nothing happens.
    Lobby,
    /// More than two players, waiting for a round to start.
    Waiting(f32),
    /// More than one player left alive.
    Round,
    /// One or less players alive, waiting for the round to end.
    PostRound(f32),
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameSettings {
    pub ball_radius: f32,
    pub cursor_radius: f32,
    pub spring_constant: f32,
    pub ball_start_distance: f32,
    pub ball_start_speed: f32,
    pub bounds_radius: f32,
}

impl Default for GameSettings {
    fn default() -> GameSettings {
        GameSettings {
            ball_radius: 0.15,
            cursor_radius: 0.05,
            spring_constant: 8.0,
            ball_start_distance: 0.3,
            ball_start_speed: 1.0,
            bounds_radius: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    RoundState(RoundState),
    Settings(GameSettings),
    NewPlayer {
        id: PlayerId,
        static_state: StaticPlayerState,
    },
    RemovePlayer(PlayerId),
    Snapshot(Snapshot),
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    pub cursor: Point2<f32>,
}

/// Dynamic state for the large ball.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct Ball {
    pub position: Point2<f32>,
    pub velocity: Vector2<f32>,
}

/// Static player state that is unlikely to change between frames.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaticPlayerState {
    pub color: LinSrgb,
}

/// Dynamic player state that is likely to change between frames.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct PlayerState {
    /// Position of the cursor, if the player is still alive.
    pub cursor: Option<Point2<f32>>,
    pub ball: Ball,
}

impl Ball {
    /// Gets the starting state of the ball for a given starting
    /// cursor position.
    pub fn starting(cursor: Point2<f32>, settings: &GameSettings) -> Ball {
        let cursor_dir = (cursor - Point2::origin()).normalize();
        let mut position = cursor + cursor_dir * settings.ball_start_distance;
        let max_dist = settings.bounds_radius - settings.ball_radius;
        if nalgebra::distance_squared(&position, &Point2::origin()) >
            max_dist * max_dist
        {
            position.coords.normalize_mut();
            position *= max_dist;
        }
        Ball {
            position,
            velocity: cursor_dir * settings.ball_start_speed,
        }
    }

    /// Steps the ball forward in time using a provided cursor
    /// location.
    pub fn tick(
        &mut self,
        dt: f32,
        cursor: Option<Point2<f32>>,
        settings: &GameSettings,
    ) {
        if let Some(cursor) = cursor {
            let displacement = self.position - cursor;
            self.velocity -= settings.spring_constant * displacement * dt;
        }
        self.position += self.velocity * dt;
    }
}

/// Clamps a cursor position within bounds.
pub fn clamp_cursor(
    cursor: Point2<f32>,
    settings: &GameSettings,
) -> Point2<f32> {
    let dist_sq = (cursor - Point2::origin()).norm_squared();
    let max_dist = settings.bounds_radius - settings.cursor_radius;
    if dist_sq > max_dist * max_dist {
        max_dist * cursor / dist_sq.sqrt()
    } else {
        cursor
    }
}

impl PlayerState {
    /// Creates a new player state given a cursor position.
    ///
    /// The player's ball is placed a set distance further away from
    /// the origin than the cursor.
    pub fn new(cursor: Point2<f32>, settings: &GameSettings) -> PlayerState {
        PlayerState {
            cursor: Some(cursor),
            ball: Ball::starting(cursor, settings),
        }
    }

    /// Sets the cursor position, if the player is still alive.
    pub fn set_cursor(&mut self, cursor: Point2<f32>) {
        if let Some(ref mut alive_cursor) = self.cursor {
            *alive_cursor = cursor;
        }
    }

    /// Steps the player forward in time.
    pub fn tick(&mut self, dt: f32, settings: &GameSettings) {
        self.ball.tick(dt, self.cursor, settings);
    }

    /// Returns whether the player is still alive.
    pub fn alive(&self) -> bool {
        self.cursor.is_some()
    }
}

/// A trait for any system of querying player state.
pub trait GetPlayer {
    type State: Deref<Target = PlayerState>;
    type StaticState: Deref<Target = StaticPlayerState>;

    fn state(self) -> Self::State;

    fn static_state(self) -> Self::StaticState;

    /// Generates a set of circles to draw this player.
    fn draw(self, scale: f32, settings: &GameSettings) -> SmallVec<[Circle; 2]>
    where
        Self: Sized + Copy,
    {
        let state = self.state();
        let color = self.static_state().color;
        let mut circles = SmallVec::new();
        // Ball
        circles.push(Circle {
            center: state.ball.position * scale,
            radius: settings.ball_radius * scale,
            color,
        });
        if let Some(cursor) = state.cursor {
            // Cursor, if alive
            circles.push(Circle {
                center: cursor * scale,
                radius: settings.cursor_radius * scale,
                color,
            });
        }
        circles
    }
}

impl Default for RoundState {
    fn default() -> RoundState {
        RoundState::Lobby
    }
}

impl RoundState {
    /// Waiting state, for the default initial time.
    pub fn waiting() -> RoundState {
        RoundState::Waiting(3.0) // 3 seconds
    }

    /// Post-round state, for the default initial time.
    pub fn post_round() -> RoundState {
        RoundState::PostRound(3.0) // 3 seconds
    }

    /// Whether a round is actually running in this state.
    pub fn running(self) -> bool {
        match self {
            RoundState::Round | RoundState::PostRound(_) => true,
            _ => false,
        }
    }

    /// Tick any timers on this state, returning a possible transition.
    ///
    /// The reason that this doesn't just transition itself is that
    /// sometimes you want to ignore the transition. For example, the
    /// client should just wait for a message from the server.
    pub fn tick(&mut self, dt: f32) -> Option<RoundState> {
        match self {
            RoundState::Waiting(ref mut time) => {
                *time -= dt;
                if *time < 0.0 {
                    return Some(RoundState::Round);
                }
            },
            RoundState::PostRound(ref mut time) => {
                *time -= dt;
                if *time < 0.0 {
                    return Some(RoundState::waiting());
                }
            },
            _ => (),
        }
        None
    }
}

/// Steps over a given time interval in chunks of at most a specified duration.
///
/// This is useful for things that will be stable at small timesteps,
/// but break down over larger timesteps. In order to step a large
/// timestep, it needs to be broken up and stepped in sequence.
pub fn step_dt(dt: f32, max: f32) -> impl Iterator<Item = f32> {
    let times = (dt / max) as usize;
    iter::repeat(max).take(times).chain(iter::once(dt % max))
}
