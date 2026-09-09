//! Sampling a face's boundary edges into polylines.
//!
//! The sampling density is what bounds the whole tessellation's error: the
//! trimmed region is triangulated between these points and nowhere else, so an
//! arc sampled to a chord tolerance carries that tolerance into every triangle
//! that touches it.

use bim_core::{BimBrepArc, BimBrepCurve, BimBrepEdge};

use crate::surface::{metres, point};
use crate::vector::{Vec3, add, cross, normalize, scale};

/// The number of straight pieces an arc of `radius` needs for its chord to
/// stay within `tolerance` of it.
#[must_use]
pub fn arc_segments(radius: f64, sweep: f64, tolerance: f64) -> usize {
    let sweep = sweep.abs();
    if radius <= 0.0 || sweep <= 0.0 {
        return 1;
    }
    // A chord subtending `step` deviates by radius * (1 - cos(step / 2)).
    let ratio = (1.0 - (tolerance / radius).min(1.0)).clamp(-1.0, 1.0);
    let step = 2.0 * ratio.acos();
    if step <= 1e-6 {
        return 128;
    }
    ((sweep / step).ceil() as usize).clamp(1, 256)
}

/// The points of one arc, from its start angle to its end angle inclusive.
fn sample_arc(arc: &BimBrepArc, tolerance: f64) -> Option<Vec<Vec3>> {
    let center = point(&arc.center)?;
    let radius = metres(
        arc.radius.value,
        arc.radius.unit.as_ref().map(|unit| unit.id.as_str()),
    )?;
    let x = normalize(arc.x_axis)?;
    let z = normalize(arc.z_axis)?;
    let y = normalize(cross(z, x))?;
    let sweep = arc.end_angle - arc.start_angle;
    let segments = arc_segments(radius, sweep, tolerance);
    let mut points = Vec::with_capacity(segments + 1);
    for step in 0..=segments {
        let angle = sweep.mul_add(step as f64 / segments as f64, arc.start_angle);
        points.push(add(
            center,
            add(
                scale(x, radius * angle.cos()),
                scale(y, radius * angle.sin()),
            ),
        ));
    }
    Some(points)
}

/// Append `edge` to `into`, leaving its last point off so the next edge
/// contributes it. A loop closes on the first point of its first edge.
///
/// Returns `false` when the edge is stated in a unit or a frame this cannot
/// read; the caller drops the whole face rather than tiling a partial one.
pub fn append_edge(edge: &BimBrepEdge, tolerance: f64, into: &mut Vec<Vec3>) -> bool {
    match &edge.curve {
        BimBrepCurve::Line => {
            let Some(start) = point(&edge.start) else {
                return false;
            };
            into.push(start);
            true
        }
        BimBrepCurve::Arc(arc) => {
            let Some(points) = sample_arc(arc, tolerance) else {
                return false;
            };
            into.extend_from_slice(&points[..points.len().saturating_sub(1)]);
            true
        }
        BimBrepCurve::Polyline(points) => {
            for at in points.iter().take(points.len().saturating_sub(1)) {
                let Some(at) = point(at) else {
                    return false;
                };
                into.push(at);
            }
            true
        }
    }
}

/// One boundary loop as a closed polyline, without repeating its first point
/// at the end.
#[must_use]
pub fn sample_loop(edges: &[BimBrepEdge], tolerance: f64) -> Option<Vec<Vec3>> {
    let mut points = Vec::with_capacity(edges.len() * 2);
    for edge in edges {
        if !append_edge(edge, tolerance, &mut points) {
            return None;
        }
    }
    // Two consecutive points at the same place carry no direction and turn an
    // ear-clipping step into a degenerate triangle.
    points.dedup_by(|left, right| crate::vector::distance(*left, *right) < 1e-9);
    if points.len() > 1 && crate::vector::distance(points[0], points[points.len() - 1]) < 1e-9 {
        points.pop();
    }
    (points.len() >= 3).then_some(points)
}

#[cfg(test)]
mod tests {
    use super::arc_segments;

    #[test]
    fn a_tighter_tolerance_asks_for_more_pieces() {
        let coarse = arc_segments(0.5, std::f64::consts::TAU, 0.01);
        let fine = arc_segments(0.5, std::f64::consts::TAU, 0.001);
        assert!(fine > coarse);
    }

    #[test]
    fn a_larger_radius_at_one_tolerance_asks_for_more_pieces() {
        let small = arc_segments(0.1, std::f64::consts::TAU, 0.002);
        let large = arc_segments(2.0, std::f64::consts::TAU, 0.002);
        assert!(large > small);
    }

    #[test]
    fn a_degenerate_arc_still_yields_one_piece() {
        assert_eq!(arc_segments(0.0, 1.0, 0.001), 1);
    }
}
