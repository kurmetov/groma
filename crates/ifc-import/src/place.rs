//! Placements: the affine maps an IFC file states between one coordinate
//! system and the next.
//!
//! Geometry in `bim-core` is in world coordinates, so every placement a file
//! declares has to be composed and applied rather than carried. That is what
//! this does: an [`Affine`] is a 3x3 basis and an origin, composed down the
//! `ObjectPlacement` chain and applied to the points a representation states.

use std::collections::HashMap;

use crate::step::{Entity, Parsed, Value};
use crate::units::Units;

pub type Vec3 = [f64; 3];

/// A basis and an origin: `apply(p) = basis * p + origin`.
///
/// The basis is not required to be orthonormal - a mapped item may scale one -
/// but it is always a linear map, so a plane stays a plane and a straight
/// edge stays straight. Curves are sampled into points before they meet one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub basis: [Vec3; 3],
    pub origin: Vec3,
}

pub const IDENTITY: Affine = Affine {
    basis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    origin: [0.0, 0.0, 0.0],
};

impl Default for Affine {
    fn default() -> Self {
        IDENTITY
    }
}

impl Affine {
    /// The point `at` in the coordinates this maps into.
    #[must_use]
    pub fn point(&self, at: Vec3) -> Vec3 {
        [
            self.basis[0][0].mul_add(
                at[0],
                self.basis[1][0].mul_add(at[1], self.basis[2][0] * at[2]),
            ) + self.origin[0],
            self.basis[0][1].mul_add(
                at[0],
                self.basis[1][1].mul_add(at[1], self.basis[2][1] * at[2]),
            ) + self.origin[1],
            self.basis[0][2].mul_add(
                at[0],
                self.basis[1][2].mul_add(at[1], self.basis[2][2] * at[2]),
            ) + self.origin[2],
        ]
    }

    /// A direction, which the origin does not move.
    #[must_use]
    pub fn direction(&self, at: Vec3) -> Vec3 {
        let moved = self.point(at);
        [
            moved[0] - self.origin[0],
            moved[1] - self.origin[1],
            moved[2] - self.origin[2],
        ]
    }

    /// `self` applied after `inner`.
    #[must_use]
    pub fn then(&self, inner: &Self) -> Self {
        Self {
            basis: [
                self.direction(inner.basis[0]),
                self.direction(inner.basis[1]),
                self.direction(inner.basis[2]),
            ],
            origin: self.point(inner.origin),
        }
    }

    /// How much this stretches each basis direction, or `None` where the three
    /// do not agree. A shape whose size survives a placement unchanged - a
    /// swept disk's radius - may only be carried through a placement that
    /// scales every direction alike.
    #[must_use]
    pub fn uniform_scale(&self) -> Option<f64> {
        let lengths = [
            length(self.basis[0]),
            length(self.basis[1]),
            length(self.basis[2]),
        ];
        let spread = lengths[0].max(lengths[1]).max(lengths[2]);
        let least = lengths[0].min(lengths[1]).min(lengths[2]);
        (spread - least <= spread * 1e-9).then_some(spread)
    }
}

#[must_use]
pub fn length(at: Vec3) -> f64 {
    at[0]
        .mul_add(at[0], at[1].mul_add(at[1], at[2] * at[2]))
        .sqrt()
}

#[must_use]
pub fn normalize(at: Vec3) -> Option<Vec3> {
    let size = length(at);
    (size > 1e-12).then(|| [at[0] / size, at[1] / size, at[2] / size])
}

#[must_use]
pub fn cross(left: Vec3, right: Vec3) -> Vec3 {
    [
        left[1].mul_add(right[2], -(left[2] * right[1])),
        left[2].mul_add(right[0], -(left[0] * right[2])),
        left[0].mul_add(right[1], -(left[1] * right[0])),
    ]
}

#[must_use]
pub fn dot(left: Vec3, right: Vec3) -> f64 {
    left[0].mul_add(right[0], left[1].mul_add(right[1], left[2] * right[2]))
}

#[must_use]
pub fn subtract(left: Vec3, right: Vec3) -> Vec3 {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

#[must_use]
pub fn add(left: Vec3, right: Vec3) -> Vec3 {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

#[must_use]
pub fn scale(at: Vec3, by: f64) -> Vec3 {
    [at[0] * by, at[1] * by, at[2] * by]
}

/// An `IfcCartesianPoint`, in the file's own length unit. Two coordinates are
/// a point in a profile's plane, which this reads as `z = 0`.
#[must_use]
pub fn cartesian_point(entity: &Entity) -> Option<Vec3> {
    coordinates(entity.attribute(0)?)
}

/// A coordinate list, as `IfcCartesianPoint` and the point lists both state it.
#[must_use]
pub fn coordinates(value: &Value) -> Option<Vec3> {
    let numbers = value.as_list()?;
    if numbers.len() < 2 {
        return None;
    }
    Some([
        numbers[0].as_number()?,
        numbers[1].as_number()?,
        numbers.get(2).and_then(Value::as_number).unwrap_or(0.0),
    ])
}

/// An `IfcDirection`, which carries no unit.
#[must_use]
pub fn direction(entity: &Entity) -> Option<Vec3> {
    coordinates(entity.attribute(0)?)
}

/// Complete a frame from whichever of its axes the file stated.
///
/// `IfcAxis2Placement3D` leaves both directions optional, and the reference
/// direction it does state need not be square to the axis. The axis wins,
/// which is what the schema says: the reference direction only fixes where
/// zero is around it.
#[must_use]
pub fn frame(axis: Option<Vec3>, reference: Option<Vec3>) -> [Vec3; 3] {
    let z = axis.and_then(normalize).unwrap_or([0.0, 0.0, 1.0]);
    let seed = reference.and_then(normalize).unwrap_or_else(|| {
        // Any direction across the axis will do; the one furthest from it
        // keeps the cross product well conditioned.
        if z[2].abs() < 0.9 {
            [0.0, 0.0, 1.0]
        } else {
            [1.0, 0.0, 0.0]
        }
    });
    let squared = subtract(seed, scale(z, dot(seed, z)));
    let x = normalize(squared).unwrap_or_else(|| {
        let fallback = if z[2].abs() < 0.9 {
            [0.0, 0.0, 1.0]
        } else {
            [1.0, 0.0, 0.0]
        };
        normalize(subtract(fallback, scale(z, dot(fallback, z)))).unwrap_or([1.0, 0.0, 0.0])
    });
    [x, cross(z, x), z]
}

/// `IfcAxis2Placement2D` or `IfcAxis2Placement3D`, as an affine map out of the
/// frame it defines.
#[must_use]
pub fn axis_placement(parsed: &Parsed, entity: &Entity, units: &Units) -> Option<Affine> {
    match entity.type_name.as_str() {
        "IFCAXIS2PLACEMENT3D" => {
            let origin = parsed
                .follow(entity.attribute(0))
                .and_then(cartesian_point)
                .map_or([0.0; 3], |at| scale(at, units.length));
            let axis = parsed.follow(entity.attribute(1)).and_then(direction);
            let reference = parsed.follow(entity.attribute(2)).and_then(direction);
            Some(Affine {
                basis: frame(axis, reference),
                origin,
            })
        }
        "IFCAXIS2PLACEMENT2D" => {
            let origin = parsed
                .follow(entity.attribute(0))
                .and_then(cartesian_point)
                .map_or([0.0; 3], |at| scale(at, units.length));
            let reference = parsed.follow(entity.attribute(1)).and_then(direction);
            let x = reference.and_then(normalize).unwrap_or([1.0, 0.0, 0.0]);
            Some(Affine {
                basis: [x, [-x[1], x[0], 0.0], [0.0, 0.0, 1.0]],
                origin,
            })
        }
        _ => None,
    }
}

/// `IfcCartesianTransformationOperator2D`/`3D`, as a mapped item states it.
#[must_use]
pub fn transformation_operator(parsed: &Parsed, entity: &Entity, units: &Units) -> Option<Affine> {
    if !entity
        .type_name
        .starts_with("IFCCARTESIANTRANSFORMATIONOPERATOR")
    {
        return None;
    }
    let three = entity.type_name.contains("3D");
    let stated_x = parsed.follow(entity.attribute(0)).and_then(direction);
    let stated_y = parsed.follow(entity.attribute(1)).and_then(direction);
    let origin = parsed
        .follow(entity.attribute(2))
        .and_then(cartesian_point)
        .map_or([0.0; 3], |at| scale(at, units.length));
    let factor = entity
        .attribute(3)
        .and_then(Value::as_number)
        .unwrap_or(1.0);
    // `Axis3` is the fifth attribute of the 3D operator, and the non-uniform
    // forms state a scale per axis after it.
    let stated_z = three
        .then(|| parsed.follow(entity.attribute(4)).and_then(direction))
        .flatten();
    let axes = if three {
        frame(stated_z, stated_x)
    } else {
        let x = stated_x.and_then(normalize).unwrap_or([1.0, 0.0, 0.0]);
        [x, [-x[1], x[0], 0.0], [0.0, 0.0, 1.0]]
    };
    // A stated Y that is square to the other two is honoured; one that is not
    // is a defect the schema already forbids, and the completed frame stands.
    let y = stated_y
        .and_then(normalize)
        .filter(|stated| dot(*stated, axes[0]).abs() < 1e-6 && dot(*stated, axes[2]).abs() < 1e-6)
        .unwrap_or(axes[1]);
    // The non-uniform forms state a scale per axis after the axes: `Scale2`
    // and `Scale3` follow `Axis3` in 3D, and `Scale2` follows `Scale` in 2D.
    // The uniform forms have neither, and every axis takes `Scale`.
    let (second, third) = if three {
        (
            entity
                .attribute(5)
                .and_then(Value::as_number)
                .unwrap_or(factor),
            entity
                .attribute(6)
                .and_then(Value::as_number)
                .unwrap_or(factor),
        )
    } else {
        (
            entity
                .attribute(4)
                .and_then(Value::as_number)
                .unwrap_or(factor),
            factor,
        )
    };
    Some(Affine {
        basis: [
            scale(axes[0], factor),
            scale(y, second),
            scale(axes[2], third),
        ],
        origin,
    })
}

/// Resolves an element's `ObjectPlacement` into world coordinates, caching
/// what it has already walked.
///
/// A placement chain is shared: every element on a storey hangs off that
/// storey's placement, so resolving one resolves the tail for all of them.
pub struct Placements<'parsed> {
    parsed: &'parsed Parsed,
    units: Units,
    resolved: HashMap<u64, Option<Affine>>,
    /// Placements this could not read, so a caller can report them.
    pub unread: usize,
}

impl<'parsed> Placements<'parsed> {
    #[must_use]
    pub fn new(parsed: &'parsed Parsed, units: Units) -> Self {
        Self {
            parsed,
            units,
            resolved: HashMap::new(),
            unread: 0,
        }
    }

    /// The world placement `id` names, or `None` where the file states one
    /// this cannot read - a grid placement, or a chain that points at itself.
    pub fn world(&mut self, id: u64) -> Option<Affine> {
        self.resolve(id, 0)
    }

    fn resolve(&mut self, id: u64, depth: u16) -> Option<Affine> {
        if let Some(known) = self.resolved.get(&id) {
            return *known;
        }
        // A chain deeper than this is a cycle or a defect; either way it is
        // refused rather than followed until the stack runs out.
        if depth > 64 {
            self.unread += 1;
            return None;
        }
        let placement = self.parsed.get(id)?;
        // `IfcGridPlacement` locates an element on a grid, which this does not
        // read. It is refused rather than placed at the origin.
        let resolved = if placement.type_name == "IFCLOCALPLACEMENT" {
            let relative = self
                .parsed
                .follow(placement.attribute(1))
                .and_then(|entity| axis_placement(self.parsed, entity, &self.units))
                .unwrap_or(IDENTITY);
            match placement.attribute(0).and_then(Value::as_reference) {
                Some(parent) => self
                    .resolve(parent, depth + 1)
                    .map(|outer| outer.then(&relative)),
                None => Some(relative),
            }
        } else {
            self.unread += 1;
            None
        };
        self.resolved.insert(id, resolved);
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::{Affine, IDENTITY, Placements, axis_placement, frame};
    use crate::step::parse;
    use crate::units::Units;

    fn file(data: &str) -> crate::step::Parsed {
        let text = format!("ISO-10303-21;\nDATA;\n{data}ENDSEC;\nEND-ISO-10303-21;\n");
        parse(text.as_bytes()).expect("a STEP file")
    }

    #[test]
    fn completes_a_frame_from_the_axis_alone() {
        let axes = frame(Some([0.0, 0.0, 1.0]), None);
        assert!((axes[2][2] - 1.0).abs() < 1e-12);
        // x, y and z stay a right-handed orthonormal set.
        assert!((super::dot(axes[0], axes[1])).abs() < 1e-12);
        assert!((super::length(super::cross(axes[0], axes[1])) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn squares_a_reference_direction_that_leans_on_the_axis() {
        let axes = frame(Some([0.0, 0.0, 1.0]), Some([1.0, 0.0, 0.5]));
        assert!(axes[0][2].abs() < 1e-12, "x is brought into the axis plane");
        assert!((axes[0][0] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn composes_the_placement_chain_in_the_files_own_length_unit() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((1000.,0.,0.));\n\
             #2=IFCAXIS2PLACEMENT3D(#1,$,$);\n\
             #3=IFCLOCALPLACEMENT($,#2);\n\
             #4=IFCCARTESIANPOINT((0.,2000.,0.));\n\
             #5=IFCAXIS2PLACEMENT3D(#4,$,$);\n\
             #6=IFCLOCALPLACEMENT(#3,#5);\n",
        );
        let units = Units {
            length: 0.001,
            angle: 1.0,
            stated_length: true,
        };
        let mut placements = Placements::new(&parsed, units);
        let world = placements.world(6).expect("a placement");
        assert!((world.origin[0] - 1.0).abs() < 1e-12);
        assert!((world.origin[1] - 2.0).abs() < 1e-12);
        assert_eq!(placements.unread, 0);
    }

    #[test]
    fn refuses_a_placement_it_does_not_read_rather_than_placing_it_at_the_origin() {
        let parsed = file("#1=IFCGRIDPLACEMENT($,$);\n");
        let mut placements = Placements::new(&parsed, Units::default());
        assert_eq!(placements.world(1), None);
        assert_eq!(placements.unread, 1);
    }

    #[test]
    fn turns_a_point_through_a_quarter_turn() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #2=IFCDIRECTION((0.,0.,1.));\n\
             #3=IFCDIRECTION((0.,1.,0.));\n\
             #4=IFCAXIS2PLACEMENT3D(#1,#2,#3);\n",
        );
        let placement = axis_placement(&parsed, parsed.get(4).expect("#4"), &Units::default())
            .expect("a frame");
        let moved = placement.point([1.0, 0.0, 0.0]);
        assert!((moved[0]).abs() < 1e-12);
        assert!((moved[1] - 1.0).abs() < 1e-12);
        assert_eq!(IDENTITY.then(&placement), placement);
        assert_eq!(Affine::default(), IDENTITY);
    }
}
