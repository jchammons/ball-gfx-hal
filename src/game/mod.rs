use crate::graphics::Circle;
use arrayvec::ArrayVec;
use nalgebra::{Point2, Vector2};
use num_traits::Zero;
use palette::LinSrgb;
use serde::{Deserialize, Serialize};
use std::iter;
use std::ops::Deref;

pub mod client;
pub mod input;
pub mod server;
pub mod snapshot;

pub use self::input::*;
pub use self::snapshot::*;

pub const BALL_RADIUS: f32 = 0.15;
pub const CURSOR_RADIUS: f32 = 0.05;
const SPRING_CONSTANT: f32 = 8.0;
const BALL_START_DISTANCE: f32 = 0.3;
pub type PlayerId = u16;

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
    pub cursor: Point2<f32>,
    pub ball: Ball,
}

/// A trait for any system of querying player state.
pub trait GetPlayer {
    type State: Deref<Target = PlayerState>;
    type StaticState: Deref<Target = StaticPlayerState>;

    fn state(self) -> Self::State;

    fn static_state(self) -> Self::StaticState;

    /// Generates a set of circles to draw this player.
    fn draw(self, scale: f32) -> ArrayVec<[Circle; 2]>
    where
        Self: Sized + Copy,
    {
        let state = self.state();
        let color = self.static_state().color;
        let cursor = Circle {
            center: state.cursor * scale,
            radius: CURSOR_RADIUS * scale,
            color,
        };
        let ball = Circle {
            center: state.ball.position * scale,
            radius: BALL_RADIUS * scale,
            color,
        };
        ArrayVec::from([cursor, ball])
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
    /// Steps the ball forward in time using a provided cursor
    /// location.
    pub fn tick(&mut self, dt: f32, cursor: Point2<f32>) {
        let displacement = self.position - cursor;
        self.velocity -= SPRING_CONSTANT * displacement * dt;
        self.position += self.velocity * dt;
    }
}

impl PlayerState {
    /// Creates a new player state given a cursor position.
    ///
    /// The player's ball is placed a set distance further away from
    /// the origin than the cursor.
    pub fn new(cursor: Point2<f32>) -> PlayerState {
        PlayerState {
            cursor,
            ball: Ball {
                position: cursor +
                    (cursor - Point2::origin()).normalize() *
                        BALL_START_DISTANCE,
                velocity: Vector2::zero(),
            },
        }
    }

    /// Steps the player forward in time.
    pub fn tick(&mut self, dt: f32) {
        self.ball.tick(dt, self.cursor);
    }
}
