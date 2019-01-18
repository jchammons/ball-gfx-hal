use crate::graphics::Circle;
use nalgebra::{self, Point2, Vector2};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};
use palette::LinSrgb;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::iter;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod client;
pub mod input;
pub mod server;
pub mod snapshot;

pub use self::input::*;
pub use self::snapshot::*;

pub const BALL_RADIUS: f32 = 0.15;
pub const CURSOR_RADIUS: f32 = 0.05;
pub const ROUND_WAITING_TIME: f32 = 3.0;
const SPRING_CONSTANT: f32 = 8.0;
const BALL_START_DISTANCE: f32 = 0.3;
const BALL_START_SPEED: f32 = 1.0;
pub type PlayerId = u16;

/// Different stages of a round.
#[derive(
    Debug,
    Copy,
    Clone,
    Eq,
    PartialEq,
    FromPrimitive,
    ToPrimitive,
    Serialize,
    Deserialize,
)]
pub enum RoundState {
    Waiting,
    Started,
}

/// A small wrapper around [RoundState] with thread safe mutability.
#[derive(Debug)]
pub struct AtomicRoundState(AtomicUsize);

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

/// A trait for any system of querying player state.
pub trait GetPlayer {
    type State: Deref<Target = PlayerState>;
    type StaticState: Deref<Target = StaticPlayerState>;

    fn state(self) -> Self::State;

    fn static_state(self) -> Self::StaticState;

    /// Generates a set of circles to draw this player.
    fn draw(self, scale: f32) -> SmallVec<[Circle; 2]>
    where
        Self: Sized + Copy,
    {
        let state = self.state();
        let color = self.static_state().color;
        let mut circles = SmallVec::new();
        // Ball
        circles.push(Circle {
            center: state.ball.position * scale,
            radius: BALL_RADIUS * scale,
            color,
        });
        if let Some(cursor) = state.cursor {
            // Cursor, if alive
            circles.push(Circle {
                center: cursor * scale,
                radius: CURSOR_RADIUS * scale,
                color,
            });
        }
        circles
    }
}

impl Default for RoundState {
    fn default() -> RoundState {
        RoundState::Waiting
    }
}

impl Default for AtomicRoundState {
    fn default() -> AtomicRoundState {
        AtomicRoundState::from(RoundState::Waiting)
    }
}

impl From<RoundState> for AtomicRoundState {
    fn from(round: RoundState) -> AtomicRoundState {
        AtomicRoundState(AtomicUsize::new(round.to_usize().unwrap()))
    }
}

impl AtomicRoundState {
    pub fn load(&self) -> RoundState {
        RoundState::from_usize(self.0.load(Ordering::Relaxed)).unwrap()
    }

    pub fn store(&self, round: RoundState) {
        self.0.store(round.to_usize().unwrap(), Ordering::Relaxed);
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

impl Ball {
    /// Gets the starting state of the ball for a given starting
    /// cursor position.
    pub fn starting(cursor: Point2<f32>) -> Ball {
        let cursor_dir = (cursor - Point2::origin()).normalize();
        let mut position = cursor + cursor_dir * BALL_START_DISTANCE;
        if nalgebra::distance(&position, &Point2::origin()) > 1.0 - BALL_RADIUS
        {
            position.coords.normalize_mut();
            position *= 1.0 - BALL_RADIUS;
        }
        Ball {
            position,
            velocity: cursor_dir * BALL_START_SPEED,
        }
    }

    /// Steps the ball forward in time using a provided cursor
    /// location.
    pub fn tick(&mut self, dt: f32, cursor: Option<Point2<f32>>) {
        if let Some(cursor) = cursor {
            let displacement = self.position - cursor;
            self.velocity -= SPRING_CONSTANT * displacement * dt;
        }
        self.position += self.velocity * dt;
    }
}

/// Clamps a cursor position within bounds.
pub fn clamp_cursor(cursor: Point2<f32>) -> Point2<f32> {
    let dist_sq = (cursor - Point2::origin()).norm_squared();
    if dist_sq > (1.0 - CURSOR_RADIUS) {
        (1.0 - CURSOR_RADIUS) * cursor / dist_sq.sqrt()
    } else {
        cursor
    }
}

impl PlayerState {
    /// Creates a new player state given a cursor position.
    ///
    /// The player's ball is placed a set distance further away from
    /// the origin than the cursor.
    pub fn new(cursor: Point2<f32>) -> PlayerState {
        PlayerState {
            cursor: Some(cursor),
            ball: Ball::starting(cursor),
        }
    }

    /// Sets the cursor position, if the player is still alive.
    pub fn set_cursor(&mut self, cursor: Point2<f32>) {
        if let Some(ref mut alive_cursor) = self.cursor {
            *alive_cursor = cursor;
        }
    }

    /// Steps the player forward in time.
    pub fn tick(&mut self, dt: f32) {
        self.ball.tick(dt, self.cursor);
    }

    /// Returns whether the player is still alive.
    pub fn alive(&self) -> bool {
        self.cursor.is_some()
    }
}
