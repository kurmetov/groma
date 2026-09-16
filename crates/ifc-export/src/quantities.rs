//! What an element's own solid measures.
//!
//! Revit computes its quantities when it exports and stores none of them -
//! joining every numeric parameter this decode recovers against every quantity
//! in Revit's own export found no carrier for a single one - so everything
//! here is measured from the body this file carries, and describes that body
//! and nothing else.
//!
//! Only a closed shell of planar faces bounded by straight edges is measured:
//! a curved face would have to be tessellated, and a tessellation is an
//! approximation whose error nothing here bounds. Anything else is left
//! unmeasured rather than estimated.

use bim_core::{BimBrep, BimBrepCurve, BimBrepSurface, BimGeometry, BimPoint3};

/// Up, in the world the bodies are written in. A wall stands in it, a slab
/// lies in it, and the quantity templates name a *height* and a *footprint*
/// that mean nothing without it.
const UP: [f64; 3] = [0.0, 0.0, 1.0];

/// How close to parallel two directions must be to count as the same one.
/// Bodies come from a kernel that wrote their planes out exactly, so faces
/// that share a direction share it to far better than this; the margin is for
/// the arithmetic, not for a judgement about what is flat.
const PARALLEL: f64 = 0.999;

/// What one solid measures.
///
/// Every field is in metres, square metres or cubic metres. Which of them a
/// given entity's quantity set has a name for is the caller's business: a
/// wall's side area is called `NetSideArea` and a slab's face is `NetArea`,
/// and neither entity has a name for what the other measures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Measured {
    /// The volume the closed shell encloses.
    pub(crate) net_volume: f64,
    /// The area of every face of it.
    pub(crate) net_surface_area: f64,
    /// The body's extent along the horizontal direction it runs in.
    pub(crate) length: f64,
    /// Its extent across that direction, horizontally.
    pub(crate) width: f64,
    /// Its extent in `UP`.
    pub(crate) height: f64,
    /// The area of the faces looking across the body, halved: a wall has two
    /// sides and `NetSideArea` names one of them.
    pub(crate) side_area: f64,
    /// The area of the faces looking up or down, halved: what the body covers
    /// where it is a slab, and what it cuts through where it is a column.
    pub(crate) plan_area: f64,
    /// The area of every upright face, whole: the surface a column shows to
    /// the room around it.
    pub(crate) upright_area: f64,
    /// The outer boundary of the lowest face that looks down.
    pub(crate) plan_perimeter: f64,
    /// Whether any face of the shell has something cut out of it - an inner
    /// loop, which is what an opening through a wall leaves behind.
    ///
    /// This is what separates a *gross* quantity from a *net* one. The body
    /// this file carries is already net: every area measured here has its
    /// holes taken out. Where nothing was taken out the two are the same
    /// number and the gross one can be written; where something was, the
    /// gross one is the face before the hole was cut, which this body no
    /// longer holds, and it is left unwritten rather than guessed at from an
    /// extent.
    pub(crate) pierced: bool,
}

/// One planar face of the shell, reduced to what a quantity asks of it.
struct Facet {
    /// The unit normal.
    normal: [f64; 3],
    area: f64,
    /// The mean height of its corners, which is how the lowest face is found.
    elevation: f64,
    /// The length of its outer boundary.
    perimeter: f64,
}

/// Measure a body, where it is one this can measure exactly.
pub(crate) fn measure(geometry: &BimGeometry) -> Option<Measured> {
    let BimGeometry::Brep(brep) = geometry else {
        return None;
    };
    if !brep.complete || brep.faces.is_empty() {
        return None;
    }
    // Six times the signed volume, so the division happens once at the end.
    let mut six_volume = 0.0_f64;
    let mut facets = Vec::with_capacity(brep.faces.len());
    let mut corners = Vec::new();
    // Every coordinate is read relative to one corner of the body. The
    // divergence theorem multiplies each triangle's normal by the position it
    // stands at, so a small body at the far end of a site - where a
    // coordinate runs to 1 500 m and a door leaf is 0.05 m3 - sums terms five
    // digits larger than the answer and loses those digits to cancellation.
    // Against Revit's own export that showed up as instances of one family
    // disagreeing with each other in the fourth digit, which is the
    // arithmetic rather than the bodies: volume and area do not move when the
    // body does, so measuring from the body's own corner is the same
    // measurement, taken where the doubles still have room for it.
    let shift = first_corner(brep)?;
    for face in &brep.faces {
        if !matches!(face.surface, BimBrepSurface::Plane { .. }) {
            return None;
        }
        // Twice the face's area vector. Summing the cross products rather
        // than their lengths is what makes a hole subtract itself: it is
        // wound against the loop it lies in, so its triangles come back out
        // of the face's own area as well as out of the volume.
        let mut area_vector = [0.0_f64; 3];
        let mut perimeter = 0.0_f64;
        let mut elevation = 0.0_f64;
        let mut counted = 0_usize;
        for (index, loop_edges) in face.loops.iter().enumerate() {
            let first_corner = corners.len();
            for edge in loop_edges {
                if !matches!(edge.curve, BimBrepCurve::Line) {
                    return None;
                }
                corners.push(subtract(metric_coordinates(&edge.start)?, shift));
            }
            let ring = &corners[first_corner..];
            let Some((origin, rest)) = ring.split_first() else {
                continue;
            };
            for pair in rest.windows(2) {
                let normal = cross(subtract(pair[0], *origin), subtract(pair[1], *origin));
                six_volume += dot(*origin, normal);
                area_vector = add(area_vector, normal);
            }
            // The first loop is the outer bound; a hole's boundary is not
            // what `Perimeter` names.
            if index == 0 {
                for (from, to) in ring
                    .iter()
                    .zip(ring.iter().cycle().skip(1))
                    .take(ring.len())
                {
                    perimeter += magnitude(subtract(*to, *from));
                }
                elevation = ring.iter().map(|corner| corner[2]).sum::<f64>();
                counted = ring.len();
            }
        }
        let twice_area = magnitude(area_vector);
        if twice_area > 0.0 && counted > 0 {
            facets.push(Facet {
                normal: scale(area_vector, 1.0 / twice_area),
                area: twice_area / 2.0,
                #[allow(clippy::cast_precision_loss)]
                // A face's corner count, far below the precision of a double.
                elevation: elevation / counted as f64,
                perimeter,
            });
        }
    }
    let net_volume = six_volume.abs() / 6.0;
    let net_surface_area = facets.iter().map(|facet| facet.area).sum::<f64>();
    if !net_volume.is_finite() || !net_surface_area.is_finite() || net_volume <= 0.0 {
        return None;
    }
    let (across, along) = frame(&facets);
    let extent = |axis: [f64; 3]| extent(&corners, axis);
    let directed = |axis: [f64; 3]| {
        facets
            .iter()
            .filter(|facet| dot(facet.normal, axis).abs() > PARALLEL)
            .map(|facet| facet.area)
            .sum::<f64>()
            / 2.0
    };
    Some(Measured {
        net_volume,
        net_surface_area,
        length: extent(along),
        width: extent(across),
        height: extent(UP),
        side_area: directed(across),
        plan_area: directed(UP),
        upright_area: facets
            .iter()
            .filter(|facet| dot(facet.normal, UP).abs() < 1.0 - PARALLEL)
            .map(|facet| facet.area)
            .sum(),
        plan_perimeter: plan_perimeter(&facets),
        pierced: brep.faces.iter().any(|face| face.loops.len() > 1),
    })
}

/// One corner of the body, to measure everything else against.
fn first_corner(brep: &BimBrep) -> Option<[f64; 3]> {
    metric_coordinates(&brep.faces.first()?.loops.first()?.first()?.start)
}

/// The horizontal directions the body itself declares: across the upright face
/// pair that carries the most area, and along the perpendicular to it.
///
/// A wall's sides are the largest thing about it, so this is the wall's own
/// across-and-along; a body with no upright face at all - a slab - keeps the
/// world's, which is the frame its own extents were already measured in.
fn frame(facets: &[Facet]) -> ([f64; 3], [f64; 3]) {
    let widest = facets
        .iter()
        .filter(|facet| dot(facet.normal, UP).abs() < 1.0 - PARALLEL)
        .max_by(|left, right| left.area.total_cmp(&right.area));
    let across = widest.map_or([0.0, 1.0, 0.0], |facet| facet.normal);
    let along = cross(UP, across);
    let length = magnitude(along);
    if length > 0.0 {
        (across, scale(along, 1.0 / length))
    } else {
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0])
    }
}

/// The outer boundary of the lowest face that looks down: a slab's perimeter
/// is the shape of its underside, not of whatever face happens to be first.
fn plan_perimeter(facets: &[Facet]) -> f64 {
    facets
        .iter()
        .filter(|facet| dot(facet.normal, UP) < -PARALLEL)
        .min_by(|left, right| left.elevation.total_cmp(&right.elevation))
        .map_or(0.0, |facet| facet.perimeter)
}

fn extent(corners: &[[f64; 3]], axis: [f64; 3]) -> f64 {
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for corner in corners {
        let along = dot(*corner, axis);
        low = low.min(along);
        high = high.max(along);
    }
    if low.is_finite() && high.is_finite() {
        high - low
    } else {
        0.0
    }
}

fn metric_coordinates(point: &BimPoint3) -> Option<[f64; 3]> {
    (point.unit.id == "autodesk.unit.unit:meters-1.0.0"
        && point.coordinates.into_iter().all(f64::is_finite))
    .then_some(point.coordinates)
}

fn add(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

fn subtract(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn scale(value: [f64; 3], factor: f64) -> [f64; 3] {
    [value[0] * factor, value[1] * factor, value[2] * factor]
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter().zip(right).map(|(a, b)| a * b).sum()
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn magnitude(value: [f64; 3]) -> f64 {
    dot(value, value).sqrt()
}
