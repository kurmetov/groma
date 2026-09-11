//! Each surface a `bim-core` face can stand on, reduced to one interface: a
//! two-parameter domain, a map from it into world coordinates, and the inverse
//! of that map.
//!
//! The two parameters are always lengths on the surface rather than raw
//! angles, so a tolerance in metres means the same thing in both of them and
//! the triangulator can treat the domain as a plane. Nothing here approximates
//! the surface; the approximation happens once, in [`crate::refine`], against
//! `to_xyz`.

use bim_core::{BimBrepProfile, BimBrepSurface, BimPoint3};

use crate::vector::{Vec3, add, cross, dot, length, normalize, scale, subtract};

/// The unit every coordinate in a tessellated model is stated in. A geometry
/// carrying anything else is refused rather than converted on a guess.
pub const METRES: &str = "autodesk.unit.unit:meters-1.0.0";

/// A point in a surface's own two parameters.
pub type Point2 = [f64; 2];

/// The curve a surface of revolution turns, in that surface's frame.
#[derive(Clone, Copy, Debug)]
pub enum Profile {
    /// `origin + parameter * direction`, `direction` a unit vector, so the
    /// parameter is a length.
    Line { origin: Vec3, direction: Vec3 },
    /// `center + radius * (cos(a) * x + sin(a) * y)` where `a` is the
    /// parameter divided by `radius`, so the parameter is an arc length.
    Arc {
        center: Vec3,
        x: Vec3,
        y: Vec3,
        radius: f64,
    },
}

impl Profile {
    /// The profile point at `parameter`, in the surface's frame.
    #[must_use]
    fn at(self, parameter: f64) -> Vec3 {
        match self {
            Self::Line { origin, direction } => add(origin, scale(direction, parameter)),
            Self::Arc {
                center,
                x,
                y,
                radius,
            } => {
                let angle = parameter / radius;
                add(
                    center,
                    add(
                        scale(x, radius * angle.cos()),
                        scale(y, radius * angle.sin()),
                    ),
                )
            }
        }
    }

    /// A parameter span wide enough to contain any point the source trims to.
    /// A line is unbounded, so the span is taken from the frame's own scale; an
    /// arc closes, so its span is one full turn.
    fn span(self) -> (f64, f64) {
        match self {
            Self::Line { origin, .. } => {
                let reach = length(origin).mul_add(4.0, 1000.0);
                (-reach, reach)
            }
            Self::Arc { radius, .. } => (0.0, std::f64::consts::TAU * radius),
        }
    }
}

/// A trimmed surface, in world coordinates.
#[derive(Clone, Copy, Debug)]
pub enum Surface {
    Plane {
        origin: Vec3,
        x: Vec3,
        y: Vec3,
        normal: Vec3,
    },
    Cylinder {
        center: Vec3,
        x: Vec3,
        y: Vec3,
        z: Vec3,
        radius: f64,
    },
    Revolution {
        center: Vec3,
        x: Vec3,
        y: Vec3,
        z: Vec3,
        profile: Profile,
        /// The radius the angular parameter is scaled by, so that both
        /// parameters are lengths.
        reference_radius: f64,
    },
}

impl Surface {
    /// Read a `bim-core` surface, or refuse it: a frame whose axes do not
    /// define a direction, or coordinates in an unestablished unit, is a
    /// defect rather than a shape to guess at.
    #[must_use]
    pub fn from_bim(surface: &BimBrepSurface) -> Option<Self> {
        match surface {
            BimBrepSurface::Plane {
                origin,
                x_axis,
                y_axis,
            } => {
                let x = normalize(*x_axis)?;
                let y = normalize(*y_axis)?;
                let normal = normalize(cross(x, y))?;
                Some(Self::Plane {
                    origin: point(origin)?,
                    x,
                    y,
                    normal,
                })
            }
            BimBrepSurface::Cylinder {
                center,
                x_axis,
                y_axis,
                z_axis,
                radius,
            } => {
                let radius = metres(
                    radius.value,
                    radius.unit.as_ref().map(|unit| unit.id.as_str()),
                )?;
                if radius <= 0.0 {
                    return None;
                }
                Some(Self::Cylinder {
                    center: point(center)?,
                    x: normalize(*x_axis)?,
                    y: normalize(*y_axis)?,
                    z: normalize(*z_axis)?,
                    radius,
                })
            }
            BimBrepSurface::Revolution {
                center,
                x_axis,
                y_axis,
                z_axis,
                profile,
            } => {
                let profile = read_profile(profile)?;
                let center = point(center)?;
                let radial = |parameter: f64| {
                    let at = profile.at(parameter);
                    at[0].hypot(at[1])
                };
                let (low, high) = profile.span();
                let reference_radius = ((radial(low) + radial(high) + radial(0.0)) / 3.0).max(1e-3);
                Some(Self::Revolution {
                    center,
                    x: normalize(*x_axis)?,
                    y: normalize(*y_axis)?,
                    z: normalize(*z_axis)?,
                    profile,
                    reference_radius,
                })
            }
            BimBrepSurface::Ruled { .. } => None,
        }
    }

    /// The world point at `uv`.
    #[must_use]
    pub fn to_xyz(self, uv: Point2) -> Vec3 {
        match self {
            Self::Plane { origin, x, y, .. } => add(origin, add(scale(x, uv[0]), scale(y, uv[1]))),
            Self::Cylinder {
                center,
                x,
                y,
                z,
                radius,
            } => {
                let angle = uv[0] / radius;
                add(
                    center,
                    add(
                        add(
                            scale(x, radius * angle.cos()),
                            scale(y, radius * angle.sin()),
                        ),
                        scale(z, uv[1]),
                    ),
                )
            }
            Self::Revolution {
                center,
                x,
                y,
                z,
                profile,
                reference_radius,
            } => {
                let at = profile.at(uv[1]);
                let angle = uv[0] / reference_radius;
                // The profile is stated in the frame; revolving it turns its
                // radial part and leaves its axial part where it is.
                let radius = at[0].hypot(at[1]);
                let phase = at[1].atan2(at[0]) + angle;
                add(
                    center,
                    add(
                        add(
                            scale(x, radius * phase.cos()),
                            scale(y, radius * phase.sin()),
                        ),
                        scale(z, at[2]),
                    ),
                )
            }
        }
    }

    /// The parameters of the world point nearest `at`.
    #[must_use]
    pub fn to_uv(self, at: Vec3) -> Point2 {
        match self {
            Self::Plane { origin, x, y, .. } => {
                let local = subtract(at, origin);
                [dot(local, x), dot(local, y)]
            }
            Self::Cylinder {
                center,
                x,
                y,
                z,
                radius,
            } => {
                let local = subtract(at, center);
                let angle = dot(local, y).atan2(dot(local, x));
                [angle * radius, dot(local, z)]
            }
            Self::Revolution {
                center,
                x,
                y,
                z,
                profile,
                reference_radius,
            } => {
                let local = subtract(at, center);
                let (radial, axial) = (dot(local, x).hypot(dot(local, y)), dot(local, z));
                let parameter = nearest_profile_parameter(profile, radial, axial);
                let seat = profile.at(parameter);
                let phase = dot(local, y).atan2(dot(local, x)) - seat[1].atan2(seat[0]);
                [phase * reference_radius, parameter]
            }
        }
    }

    /// The outward normal at `uv`, taken from the surface itself rather than
    /// from the triangles that approximate it.
    #[must_use]
    pub fn normal_at(self, uv: Point2) -> Vec3 {
        match self {
            Self::Plane { normal, .. } => normal,
            Self::Cylinder { x, y, radius, .. } => {
                let angle = uv[0] / radius;
                add(scale(x, angle.cos()), scale(y, angle.sin()))
            }
            Self::Revolution { .. } => {
                // Differencing the map is exact enough at a tessellation's
                // scale and needs no per-profile derivative.
                let step = 1e-4;
                let along = subtract(
                    self.to_xyz([uv[0] + step, uv[1]]),
                    self.to_xyz([uv[0] - step, uv[1]]),
                );
                let across = subtract(
                    self.to_xyz([uv[0], uv[1] + step]),
                    self.to_xyz([uv[0], uv[1] - step]),
                );
                normalize(cross(along, across)).unwrap_or([0.0, 0.0, 1.0])
            }
        }
    }

    /// The period of the first parameter, for a surface that closes in it.
    #[must_use]
    pub fn u_period(self) -> Option<f64> {
        match self {
            Self::Plane { .. } => None,
            Self::Cylinder { radius, .. } => Some(std::f64::consts::TAU * radius),
            Self::Revolution {
                reference_radius, ..
            } => Some(std::f64::consts::TAU * reference_radius),
        }
    }

    /// The period of the *second* parameter, where the surface closes in it.
    ///
    /// A surface of revolution turned from a full circle - a torus, a sphere -
    /// closes in its profile as well as about its axis, and its profile
    /// parameter is an arc length, so it repeats every circumference. A
    /// boundary crossing that seam reads as a jump the width of the profile
    /// unless it is unwrapped, and a jump makes a loop no tiling can take:
    /// this is what left 5 249 faces of the plumbing model untiled.
    #[must_use]
    pub fn v_period(self) -> Option<f64> {
        match self {
            Self::Plane { .. } | Self::Cylinder { .. } => None,
            Self::Revolution { profile, .. } => match profile {
                Profile::Line { .. } => None,
                Profile::Arc { radius, .. } => Some(std::f64::consts::TAU * radius),
            },
        }
    }
}

/// The profile parameter whose point sits nearest `(radial, axial)`, found by
/// sampling and then bisecting: the profiles differ enough that one closed
/// form per case would be more code than this and no more exact at the
/// tolerance a mesh is built to.
fn nearest_profile_parameter(profile: Profile, radial: f64, axial: f64) -> f64 {
    const SAMPLES: usize = 64;
    let (low, high) = profile.span();
    let cost = |parameter: f64| {
        let at = profile.at(parameter);
        (at[0].hypot(at[1]) - radial).hypot(at[2] - axial)
    };
    let mut best = low;
    let mut best_cost = f64::INFINITY;
    for step in 0..=SAMPLES {
        let parameter = (high - low).mul_add(step as f64 / SAMPLES as f64, low);
        let value = cost(parameter);
        if value < best_cost {
            best_cost = value;
            best = parameter;
        }
    }
    let mut window = (high - low) / SAMPLES as f64;
    for _ in 0..40 {
        let left = cost(best - window);
        let right = cost(best + window);
        if left < best_cost && left <= right {
            best -= window;
            best_cost = left;
        } else if right < best_cost {
            best += window;
            best_cost = right;
        } else {
            window /= 2.0;
        }
    }
    best
}

fn read_profile(profile: &BimBrepProfile) -> Option<Profile> {
    match profile {
        BimBrepProfile::Line { origin, direction } => Some(Profile::Line {
            origin: point(origin)?,
            direction: normalize(*direction)?,
        }),
        BimBrepProfile::Arc {
            center,
            x_axis,
            y_axis,
            radius,
        } => {
            let radius = metres(
                radius.value,
                radius.unit.as_ref().map(|unit| unit.id.as_str()),
            )?;
            if radius <= 0.0 {
                return None;
            }
            Some(Profile::Arc {
                center: point(center)?,
                x: normalize(*x_axis)?,
                y: normalize(*y_axis)?,
                radius,
            })
        }
    }
}

/// A `bim-core` point in metres, or `None` when it is stated in another unit.
#[must_use]
pub fn point(at: &BimPoint3) -> Option<Vec3> {
    (at.unit.id == METRES).then_some(at.coordinates)
}

/// A `bim-core` length in metres, or `None` when its unit is not established.
#[must_use]
pub fn metres(value: f64, unit: Option<&str>) -> Option<f64> {
    (unit == Some(METRES)).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{Profile, Surface};
    use crate::vector::distance;

    fn cylinder() -> Surface {
        Surface::Cylinder {
            center: [1.0, 2.0, 3.0],
            x: [1.0, 0.0, 0.0],
            y: [0.0, 1.0, 0.0],
            z: [0.0, 0.0, 1.0],
            radius: 0.5,
        }
    }

    #[test]
    fn a_cylinder_maps_a_point_back_to_itself() {
        let surface = cylinder();
        let at = surface.to_xyz([0.3, 1.25]);
        let back = surface.to_uv(at);
        assert!(distance(surface.to_xyz(back), at) < 1e-9);
    }

    #[test]
    fn a_cylinders_normal_points_away_from_its_axis() {
        let surface = cylinder();
        let normal = surface.normal_at([0.0, 0.0]);
        assert!((normal[0] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_cone_maps_a_point_back_to_itself() {
        let surface = Surface::Revolution {
            center: [0.0, 0.0, 0.0],
            x: [1.0, 0.0, 0.0],
            y: [0.0, 1.0, 0.0],
            z: [0.0, 0.0, 1.0],
            profile: Profile::Line {
                origin: [1.0, 0.0, 0.0],
                direction: [0.6, 0.0, 0.8],
            },
            reference_radius: 1.0,
        };
        let at = surface.to_xyz([0.4, 0.75]);
        let back = surface.to_uv(at);
        assert!(distance(surface.to_xyz(back), at) < 1e-6);
    }
}
