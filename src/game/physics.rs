use crate::game::{Ball, GameSettings};
use nalgebra::{Point2, Vector2};

#[derive(Debug, Copy, Clone)]
pub struct Circle<V> {
    pub radius: f32,
    pub center: Point2<f32>,
    pub velocity: V,
    /// `true` indicates collision with the inside edge, `false` with
    /// the outside edge.
    pub inner: bool,
}

impl<V> Circle<V> {
    /// A circle which detects collisions with it's inner edge.
    pub fn inner(radius: f32, center: Point2<f32>, velocity: V) -> Circle<V> {
        Circle {
            radius,
            center,
            velocity,
            inner: true,
        }
    }

    /// A circle which detects collisions with it's outer edge.
    pub fn outer(radius: f32, center: Point2<f32>, velocity: V) -> Circle<V> {
        Circle {
            radius,
            center,
            velocity,
            inner: false,
        }
    }

    /// Returns the orientation of the normals as a scalar.
    ///
    /// `1` if outside, `-1` if inside.
    pub fn orientation(&self) -> f32 {
        if self.inner {
            -1.0
        } else {
            1.0
        }
    }
}

/// Returns the physics circle corresponding to the boundary.
pub fn bounds(settings: &GameSettings) -> Circle<Static> {
    Circle::inner(settings.bounds_radius, Point2::origin(), Static)
}

/// Returns the physics circle corresponding to a given cursor
/// position.
pub fn cursor(cursor: Point2<f32>, settings: &GameSettings) -> Circle<Static> {
    Circle::outer(settings.cursor_radius, cursor, Static)
}

pub fn ball(ball: Ball, settings: &GameSettings) -> Circle<Vector2<f32>> {
    Circle::outer(settings.ball_radius, ball.position, ball.velocity)
}

impl From<Circle<Vector2<f32>>> for Ball {
    fn from(circle: Circle<Vector2<f32>>) -> Ball {
        Ball {
            position: circle.center,
            velocity: circle.velocity,
        }
    }
}

/// Returns the distance between centers required for a
/// collision between two circles.
pub fn collision_distance<V1, V2>(a: &Circle<V1>, b: &Circle<V2>) -> f32 {
    a.radius * b.orientation() + b.radius * a.orientation()
}

/// Returns the normal of a collision between two circles, pointing
/// towards `a`.
pub fn collision_normal<V1, V2>(
    a: &Circle<V1>,
    b: &Circle<V2>,
) -> Vector2<f32> {
    (a.center - b.center).normalize() * a.orientation() * b.orientation()
}

/// Indicates velocity a circle which is fixed in place.
#[derive(Debug, Copy, Clone)]
pub struct Static;

pub trait Velocity: Sized + Copy {
    /// Returns the quantity of the circle's velocity.
    ///
    /// For a static circle, this is always 0.
    fn get(self) -> Vector2<f32>;

    /// Calculates the new velocity of two circles after elastic
    /// collision.
    ///
    /// Similar to [`resolve_penetration`], at least one of the circles must
    /// be non-static.
    fn elastic_collision(a: &mut Circle<Vector2<f32>>, b: &mut Circle<Self>);

    /// Offsets the position of two circles along the collision normal
    /// so that they are just touching.
    fn offset_collision(a: &mut Circle<Vector2<f32>>, b: &mut Circle<Self>);
}

impl Velocity for Vector2<f32> {
    fn get(self) -> Vector2<f32> {
        self
    }

    fn elastic_collision(
        a: &mut Circle<Vector2<f32>>,
        b: &mut Circle<Vector2<f32>>,
    ) {
        // The 2d case is equivalent to the 1d case when projected onto
        // the normal. Also, the normal doesn't actually have to be
        // normalized, since any length change just scales the whole
        // system, and gets reversed when projecting back.
        let normal = collision_normal(a, b);
        let velocity_a_n = a.velocity.dot(&normal);
        let velocity_b_n = b.velocity.dot(&normal);
        // In the 1d case with two dynamic circles, velocities are
        // simply exchanged, since mass is assumed to be equal.
        let accel_a_n = velocity_b_n - velocity_a_n;
        let accel_b_n = velocity_a_n - velocity_b_n;

        a.velocity += accel_a_n * normal;
        b.velocity += accel_b_n * normal;
    }

    fn offset_collision(
        a: &mut Circle<Vector2<f32>>,
        b: &mut Circle<Vector2<f32>>,
    ) {
        let normal = collision_normal(a, b);
        let distance = collision_distance(a, b);
        let midpoint = 0.5 * (a.center + b.center.coords);
        a.center = midpoint + 0.5 * distance * normal;
        b.center = midpoint - 0.5 * distance * normal;
    }
}

impl Velocity for Static {
    fn get(self) -> Vector2<f32> {
        Vector2::new(0.0, 0.0)
    }

    fn elastic_collision(a: &mut Circle<Vector2<f32>>, b: &mut Circle<Static>) {
        let normal = (a.center - b.center).normalize() *
            a.orientation() *
            b.orientation();
        let velocity_n = a.velocity.dot(&normal);
        // In the 1d case with one static circle, the dynamic circle
        // velocity inverts.
        let accel_n = -2.0 * velocity_n;

        a.velocity += accel_n * normal;
    }

    fn offset_collision(a: &mut Circle<Vector2<f32>>, b: &mut Circle<Static>) {
        let normal = collision_normal(a, b);
        let distance = collision_distance(a, b);
        a.center = b.center + distance * normal;
    }
}

/// Fully resolves a potential collision between two circles.
///
/// Returns whether or there was a collision.
pub fn resolve_collision<V: Velocity + std::fmt::Debug>(
    a: &mut Circle<Vector2<f32>>,
    b: &mut Circle<V>,
) -> bool {
    let collision = check_collision(a, b);
    if collision {
        // Step backward along velocity vectors until the circles are
        // no longer colliding.
        let t = match resolve_penetration(a, b) {
            Some(t) => t,
            None => return true,
        };
        // Determine new velocities after collision.
        Velocity::elastic_collision(a, b);
        // Redo the backward movement with the new velocities.
        a.center -= t * a.velocity.get();
        b.center -= t * b.velocity.get();
    }
    collision
}

/// Check for collision between two circles.
pub fn check_collision<V1, V2>(a: &Circle<V1>, b: &Circle<V2>) -> bool {
    // This one is very straightforward:
    //
    // |a.center - b.center| < collision_distance
    //
    // This equation can be checked directly, but squaring both sides
    // avoids an expensive square root.
    let distance_sq = nalgebra::distance_squared(&a.center, &b.center);
    let distance = collision_distance(a, b);
    if a.inner || b.inner {
        distance_sq > distance * distance
    } else {
        distance_sq < distance * distance
    }
}

/// Moves circles out of collision along their velocity vectors.
///
/// If there is no scalar multiple of the velocity vectors that will move
/// them out of collision, it just gives up and moves along the collision
/// normal.
///
/// Returns `Some(t)` with the multiple of the velocities, or `None` if it
/// offset along normal.
///
/// At least one of the circles has to be non-static. This is expressed in
/// the type system by requiring the first argument to have `Vector2<f32>`
/// as it's velocity field.
pub fn resolve_penetration<V: Velocity + std::fmt::Debug>(
    a: &mut Circle<Vector2<f32>>,
    b: &mut Circle<V>,
) -> Option<f32> {
    // This problem can be described by the equation:
    //
    // |(a.center + t a.velocity) - (b.center + t b.velocity)| =
    // collision_distance
    //
    // Rearranging:
    //
    // |(a.center - b.center) + t (a.velocity - b.velocity)| =
    // collision_distance
    //
    // Squaring both sides and distributing the dot products gives a
    // quadratic equation for t.

    let center = a.center - b.center;
    let velocity = a.velocity - b.velocity.get();
    let orientation = a.orientation() * b.orientation();
    let distance = collision_distance(a, b);
    let co_a = velocity.dot(&velocity);
    let co_b = 2.0 * center.dot(&velocity);
    let co_c = center.dot(&center) - distance * distance;

    let discriminant = co_b * co_b - 4.0 * co_a * co_c;
    if discriminant < 0.0 {
        // Give up and move along the normal.
        Velocity::offset_collision(a, b);
        return None;
    }
    // Multiply by orientation so that it picks the furthest back
    // point in outer/outer, and the furthest forward point in
    // outer/inner.
    let t = (-co_b - orientation * discriminant.sqrt()) / (2.0 * co_a);
    if t.abs() > 0.05 {
        // There's probably something wrong...
        Velocity::offset_collision(a, b);
        return None;
    }

    a.center += t * a.velocity.get();
    b.center += t * b.velocity.get();
    Some(t)
}
