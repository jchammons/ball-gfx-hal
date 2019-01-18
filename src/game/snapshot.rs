use crate::game::{Ball, PlayerId, PlayerState};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::{Add, Mul, Sub};

pub trait Interpolate {
    type Output;

    fn interpolate(self, other: Self, alpha: f32) -> Self::Output;
}

// Linear interpolation for any normed vector space or metric space.
impl<T> Interpolate for T
where
    T: Clone
        + Sub<T>
        + Add<<<T as Sub>::Output as Mul<f32>>::Output, Output = T>,
    <T as Sub<T>>::Output: Mul<f32>,
{
    type Output = T;

    fn interpolate(self, other: T, alpha: f32) -> T {
        self.clone() + (other - self.clone()) * alpha
    }
}

impl<'a> Interpolate for &'a PlayerState {
    type Output = PlayerState;

    fn interpolate(self, other: &'a PlayerState, alpha: f32) -> PlayerState {
        PlayerState {
            cursor: self.cursor.interpolate(other.cursor, alpha),
            ball: self.ball.interpolate(&other.ball, alpha),
        }
    }
}

impl<'a> Interpolate for &'a Ball {
    type Output = Ball;

    fn interpolate(self, other: &'a Ball, alpha: f32) -> Ball {
        // TODO: hermite interpolation
        Ball {
            position: self.position.interpolate(other.position, alpha),
            velocity: self.velocity.interpolate(other.velocity, alpha),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub players: HashMap<PlayerId, PlayerState>,
}

#[derive(Copy, Clone, Debug)]
pub struct InterpolatedSnapshot<'a> {
    alpha: f32,
    old: &'a Snapshot,
    new: &'a Snapshot,
}

pub trait SnapshotView<'a> {
    /// Hopefully existential types will be a thing soon so using
    /// `Box` here won't be needed.
    fn players(
        self,
    ) -> Box<dyn Iterator<Item = (PlayerId, Cow<'a, PlayerState>)> + 'a>;

    /// Gets a player by id.
    fn get(&self, id: PlayerId) -> Option<Cow<'a, PlayerState>>;
}

impl<'a> SnapshotView<'a> for &'a Snapshot {
    fn players(
        self,
    ) -> Box<dyn Iterator<Item = (PlayerId, Cow<'a, PlayerState>)> + 'a> {
        Box::new(
            self.players.iter().map(|(&id, state)| (id, Cow::Borrowed(state))),
        )
    }

    fn get(&self, id: PlayerId) -> Option<Cow<'a, PlayerState>> {
        self.players.get(&id).map(Cow::Borrowed)
    }
}

impl<'a, S: SnapshotView<'a>> From<S> for Snapshot {
    fn from(snapshot: S) -> Snapshot {
        Snapshot {
            players: snapshot
                .players()
                .map(|(id, player)| (id, *player))
                .collect(),
        }
    }
}

impl<'a> InterpolatedSnapshot<'a> {
    pub fn new(
        alpha: f32,
        old: &'a Snapshot,
        new: &'a Snapshot,
    ) -> InterpolatedSnapshot<'a> {
        InterpolatedSnapshot {
            alpha,
            old,
            new,
        }
    }

    fn interpolate(
        &self,
        new: &'a PlayerState,
        old: Option<&'a PlayerState>,
    ) -> Cow<'a, PlayerState> {
        match old {
            // If the old snapshot contains this player, interpolate.
            Some(old) => Cow::Owned(old.interpolate(new, self.alpha)),
            // Otherwise just use only the new snapshots.
            None => Cow::Borrowed(new),
        }
    }
}

impl<'a> SnapshotView<'a> for InterpolatedSnapshot<'a> {
    fn players(
        self,
    ) -> Box<dyn Iterator<Item = (PlayerId, Cow<'a, PlayerState>)> + 'a> {
        Box::new(self.new.players.iter().map(move |(&id, new)| {
            (id, self.interpolate(new, self.old.players.get(&id)))
        }))
    }

    fn get(&self, id: PlayerId) -> Option<Cow<'a, PlayerState>> {
        self.new
            .players
            .get(&id)
            .map(|new| self.interpolate(new, self.old.players.get(&id)))
    }
}
