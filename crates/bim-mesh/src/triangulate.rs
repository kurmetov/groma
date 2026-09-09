//! Triangulation of a planar polygon with holes, in a surface's own two
//! parameters.
//!
//! Ear clipping is used rather than a constrained Delaunay triangulation
//! because what the caller needs is a valid tiling of the trimmed region, not
//! a well-shaped one: [`crate::refine`] subdivides afterwards against the
//! surface it came from, and a sliver that is flat on the surface costs a
//! triangle rather than an error. Holes are bridged into the outer loop first,
//! by the standard rightmost-vertex construction, so the clipper only ever
//! sees one simple polygon.

/// Two-dimensional point in a surface's parameter space.
pub type Point2 = [f64; 2];

/// Twice the signed area of `polygon`, positive when it winds
/// counter-clockwise.
fn signed_area2(polygon: &[Point2]) -> f64 {
    let mut total = 0.0;
    for index in 0..polygon.len() {
        let current = polygon[index];
        let next = polygon[(index + 1) % polygon.len()];
        total += current[0].mul_add(next[1], -(next[0] * current[1]));
    }
    total
}

/// Whether `polygon` winds counter-clockwise.
#[must_use]
pub fn is_counter_clockwise(polygon: &[Point2]) -> bool {
    signed_area2(polygon) > 0.0
}

/// Twice the signed area of the triangle `a`, `b`, `c`.
fn cross(a: Point2, b: Point2, c: Point2) -> f64 {
    (b[0] - a[0]).mul_add(c[1] - a[1], -((b[1] - a[1]) * (c[0] - a[0])))
}

/// Whether `point` lies inside or on the triangle `a`, `b`, `c`.
fn inside_triangle(a: Point2, b: Point2, c: Point2, point: Point2) -> bool {
    let first = cross(a, b, point);
    let second = cross(b, c, point);
    let third = cross(c, a, point);
    (first >= 0.0 && second >= 0.0 && third >= 0.0)
        || (first <= 0.0 && second <= 0.0 && third <= 0.0)
}

/// One vertex of the working polygon: where it came from, and where it is.
#[derive(Clone, Copy)]
struct Vertex {
    /// Index into the caller's own flattened vertex list. Bridging duplicates
    /// a vertex, so two entries may share one origin.
    origin: usize,
    at: Point2,
}

/// Triangulate `outer` with `holes` cut out of it.
///
/// Every returned index addresses the concatenation of `outer` and each hole
/// in the order given: `outer[i]` is `i`, and the first vertex of the first
/// hole is `outer.len()`. Triangles come back wound counter-clockwise in the
/// parameter space, whatever winding the input loops had.
///
/// An empty result means the loops did not describe a region this can tile -
/// a degenerate loop, or one that crosses itself. The caller decides what to
/// do about that; nothing is guessed here.
#[must_use]
pub fn triangulate(outer: &[Point2], holes: &[&[Point2]]) -> Vec<[usize; 3]> {
    if outer.len() < 3 {
        return Vec::new();
    }
    let mut chain: Vec<Vertex> = Vec::with_capacity(outer.len() + 8);
    let outer_ccw = is_counter_clockwise(outer);
    for (index, at) in outer.iter().enumerate() {
        chain.push(Vertex {
            origin: index,
            at: *at,
        });
    }
    if !outer_ccw {
        chain.reverse();
    }

    // Bridge the holes in from the rightmost outward, so a bridge already cut
    // never has to be crossed by a later one.
    let mut base = outer.len();
    let mut pending: Vec<(f64, Vec<Vertex>)> = Vec::with_capacity(holes.len());
    for hole in holes {
        let start = base;
        base += hole.len();
        if hole.len() < 3 {
            continue;
        }
        let mut ring: Vec<Vertex> = hole
            .iter()
            .enumerate()
            .map(|(index, at)| Vertex {
                origin: start + index,
                at: *at,
            })
            .collect();
        // A hole runs against the outer loop's winding.
        if is_counter_clockwise(hole) {
            ring.reverse();
        }
        let rightmost = ring
            .iter()
            .enumerate()
            .max_by(|left, right| {
                left.1.at[0]
                    .partial_cmp(&right.1.at[0])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map_or(0.0, |(_, vertex)| vertex.at[0]);
        pending.push((rightmost, ring));
    }
    pending.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (_, ring) in pending {
        bridge_hole(&mut chain, &ring);
    }

    clip_ears(&chain)
}

/// Splice `hole` into `chain` along a mutually visible pair of vertices.
fn bridge_hole(chain: &mut Vec<Vertex>, hole: &[Vertex]) {
    let Some(hole_index) = hole
        .iter()
        .enumerate()
        .max_by(|left, right| {
            left.1.at[0]
                .partial_cmp(&right.1.at[0])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
    else {
        return;
    };
    let origin = hole[hole_index].at;
    let Some(outer_index) = visible_from(chain, origin) else {
        return;
    };

    let mut spliced = Vec::with_capacity(chain.len() + hole.len() + 2);
    spliced.extend_from_slice(&chain[..=outer_index]);
    for step in 0..hole.len() {
        spliced.push(hole[(hole_index + step) % hole.len()]);
    }
    spliced.push(hole[hole_index]);
    spliced.push(chain[outer_index]);
    spliced.extend_from_slice(&chain[outer_index + 1..]);
    *chain = spliced;
}

/// The chain vertex to bridge `origin` to: Eberly's construction, casting a
/// ray in `+x` and then choosing the reflex vertex that the bridge must not
/// cut through.
fn visible_from(chain: &[Vertex], origin: Point2) -> Option<usize> {
    let mut best_distance = f64::INFINITY;
    let mut best_edge = None;
    for index in 0..chain.len() {
        let start = chain[index].at;
        let end = chain[(index + 1) % chain.len()].at;
        // Only edges the ray can cross, taken in the winding's own direction.
        if (start[1] > origin[1]) == (end[1] > origin[1]) {
            continue;
        }
        let span = end[1] - start[1];
        if span.abs() <= f64::MIN_POSITIVE {
            continue;
        }
        let ratio = (origin[1] - start[1]) / span;
        let crossing = (end[0] - start[0]).mul_add(ratio, start[0]);
        if crossing < origin[0] {
            continue;
        }
        let distance = crossing - origin[0];
        if distance < best_distance {
            best_distance = distance;
            best_edge = Some((index, crossing));
        }
    }
    let (edge, crossing) = best_edge?;
    let start = edge;
    let end = (edge + 1) % chain.len();
    // The endpoint the ray hit nearer in x is the candidate; if the segment
    // between it and `origin` contains a reflex vertex the bridge would cross,
    // that vertex is used instead.
    let candidate = if chain[start].at[0] > chain[end].at[0] {
        start
    } else {
        end
    };
    let apex = [crossing, origin[1]];
    let mut chosen = candidate;
    let mut best_angle = f64::INFINITY;
    for index in 0..chain.len() {
        if index == candidate {
            continue;
        }
        let at = chain[index].at;
        if !inside_triangle(origin, apex, chain[candidate].at, at) {
            continue;
        }
        let previous = chain[(index + chain.len() - 1) % chain.len()].at;
        let next = chain[(index + 1) % chain.len()].at;
        // Reflex vertices only: a convex one cannot block the bridge.
        if cross(previous, at, next) > 0.0 {
            continue;
        }
        let angle = ((at[1] - origin[1]) / (at[0] - origin[0]).hypot(at[1] - origin[1])).abs();
        if angle < best_angle {
            best_angle = angle;
            chosen = index;
        }
    }
    Some(chosen)
}

/// Ear-clip one simple, counter-clockwise polygon.
fn clip_ears(chain: &[Vertex]) -> Vec<[usize; 3]> {
    let mut remaining: Vec<usize> = (0..chain.len()).collect();
    let mut triangles = Vec::with_capacity(chain.len().saturating_sub(2));
    // Every successful clip removes a vertex; the guard bounds the search for
    // an ear so a polygon this cannot tile ends rather than spins.
    let mut without_progress = 0_usize;
    let mut cursor = 0_usize;
    while remaining.len() > 3 {
        if without_progress > remaining.len() {
            return triangles;
        }
        let count = remaining.len();
        let previous = remaining[(cursor + count - 1) % count];
        let current = remaining[cursor % count];
        let next = remaining[(cursor + 1) % count];
        let (a, b, c) = (chain[previous].at, chain[current].at, chain[next].at);
        if cross(a, b, c) > 0.0 && !contains_other(chain, &remaining, previous, current, next) {
            triangles.push([
                chain[previous].origin,
                chain[current].origin,
                chain[next].origin,
            ]);
            remaining.remove(cursor % count);
            cursor %= remaining.len();
            without_progress = 0;
        } else {
            cursor = (cursor + 1) % count;
            without_progress += 1;
        }
    }
    if remaining.len() == 3 {
        let (a, b, c) = (
            chain[remaining[0]].at,
            chain[remaining[1]].at,
            chain[remaining[2]].at,
        );
        if cross(a, b, c).abs() > 0.0 {
            triangles.push([
                chain[remaining[0]].origin,
                chain[remaining[1]].origin,
                chain[remaining[2]].origin,
            ]);
        }
    }
    triangles
}

/// Whether any other remaining vertex falls inside the candidate ear.
fn contains_other(
    chain: &[Vertex],
    remaining: &[usize],
    previous: usize,
    current: usize,
    next: usize,
) -> bool {
    let (a, b, c) = (chain[previous].at, chain[current].at, chain[next].at);
    remaining.iter().any(|index| {
        if *index == previous || *index == current || *index == next {
            return false;
        }
        let at = chain[*index].at;
        // Bridging a hole duplicates the two vertices it joins, so a corner of
        // the candidate ear can appear again elsewhere in the chain. Such a
        // vertex is the corner, not a point blocking it.
        if coincident(at, a) || coincident(at, b) || coincident(at, c) {
            return false;
        }
        inside_triangle(a, b, c, at)
    })
}

/// Whether two parameter points are the same point.
fn coincident(a: Point2, b: Point2) -> bool {
    (a[0] - b[0]).hypot(a[1] - b[1]) <= f64::EPSILON
}

#[cfg(test)]
mod tests {
    use super::{Point2, triangulate};

    fn area(points: &[Point2], triangles: &[[usize; 3]]) -> f64 {
        triangles
            .iter()
            .map(|triangle| {
                let a = points[triangle[0]];
                let b = points[triangle[1]];
                let c = points[triangle[2]];
                ((b[0] - a[0]).mul_add(c[1] - a[1], -((b[1] - a[1]) * (c[0] - a[0])))).abs() / 2.0
            })
            .sum()
    }

    #[test]
    fn a_square_tiles_into_two_triangles_covering_its_area() {
        let square = [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        let triangles = triangulate(&square, &[]);
        assert_eq!(triangles.len(), 2);
        assert!((area(&square, &triangles) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn a_clockwise_square_tiles_to_the_same_area() {
        let square = [[0.0, 0.0], [0.0, 2.0], [2.0, 2.0], [2.0, 0.0]];
        let triangles = triangulate(&square, &[]);
        assert!((area(&square, &triangles) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn a_hole_is_cut_out_of_the_area_rather_than_covered() {
        let outer = [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        let hole = [[1.0, 1.0], [1.0, 3.0], [3.0, 3.0], [3.0, 1.0]];
        let triangles = triangulate(&outer, &[&hole]);
        let points: Vec<Point2> = outer.iter().chain(hole.iter()).copied().collect();
        assert!((area(&points, &triangles) - 12.0).abs() < 1e-6);
    }

    #[test]
    fn an_l_shape_keeps_its_reflex_corner_outside_the_tiling() {
        let shape = [
            [0.0, 0.0],
            [3.0, 0.0],
            [3.0, 1.0],
            [1.0, 1.0],
            [1.0, 3.0],
            [0.0, 3.0],
        ];
        let triangles = triangulate(&shape, &[]);
        assert_eq!(triangles.len(), 4);
        assert!((area(&shape, &triangles) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_degenerate_loop_yields_nothing_rather_than_a_guess() {
        assert!(triangulate(&[[0.0, 0.0], [1.0, 0.0]], &[]).is_empty());
    }
}
