//! The three-component vector arithmetic the tessellator needs, kept here so
//! no other module repeats it.

/// A vector or a point in world coordinates, in metres.
pub type Vec3 = [f64; 3];

#[must_use]
pub fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

#[must_use]
pub fn subtract(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[must_use]
pub fn scale(a: Vec3, factor: f64) -> Vec3 {
    [a[0] * factor, a[1] * factor, a[2] * factor]
}

#[must_use]
pub fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0].mul_add(b[0], a[1].mul_add(b[1], a[2] * b[2]))
}

#[must_use]
pub fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1].mul_add(b[2], -(a[2] * b[1])),
        a[2].mul_add(b[0], -(a[0] * b[2])),
        a[0].mul_add(b[1], -(a[1] * b[0])),
    ]
}

#[must_use]
pub fn length(a: Vec3) -> f64 {
    dot(a, a).sqrt()
}

#[must_use]
pub fn distance(a: Vec3, b: Vec3) -> f64 {
    length(subtract(a, b))
}

/// `a` scaled to unit length, or `None` when it is too short to have a
/// direction. A zero-length axis is a defect in the source, not a direction of
/// zero, so it is reported rather than replaced.
#[must_use]
pub fn normalize(a: Vec3) -> Option<Vec3> {
    let magnitude = length(a);
    if magnitude <= 1e-12 {
        return None;
    }
    Some(scale(a, 1.0 / magnitude))
}

/// `mix(a, b, 0)` is `a` and `mix(a, b, 1)` is `b`.
#[must_use]
pub fn mix(a: Vec3, b: Vec3, ratio: f64) -> Vec3 {
    [
        (b[0] - a[0]).mul_add(ratio, a[0]),
        (b[1] - a[1]).mul_add(ratio, a[1]),
        (b[2] - a[2]).mul_add(ratio, a[2]),
    ]
}
