#![forbid(unsafe_code)]
// A tessellator counts in integers and measures in floats, so the two meet on
// nearly every line. Every cast below is bounded: a segment count by its own
// clamp, a vertex index by the buffer it addresses.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
//! Tessellation of `bim-core` geometry into indexed triangle meshes.
//!
//! The decoders upstream of this crate recover analytic geometry - planes,
//! cylinders, surfaces of revolution, arcs - because that is what the source
//! files hold. A viewer needs triangles. This crate is the one place that
//! crossing happens, so the approximation it makes is stated once and can be
//! measured: every triangle vertex sits exactly on the surface it came from,
//! and the chord between two of them stays within
//! [`MeshOptions::chord_tolerance`] of it.
//!
//! Nothing here invents geometry. A face whose surface, unit or boundary this
//! cannot read is left out of the mesh and counted in [`Mesh::skipped_faces`],
//! rather than replaced by a box or a guess.

mod curve;
mod surface;
mod triangulate;
mod vector;

use std::collections::HashMap;

use bim_core::{BimBoundingBox, BimBrep, BimBrepFace, BimGeometry, BimSweptDisk};

pub use surface::METRES;
use surface::{Point2, Surface, metres, point};
use vector::{Vec3, add, cross, distance, dot, mix, normalize, scale, subtract};

/// How finely to approximate curved geometry.
#[derive(Clone, Copy, Debug)]
pub struct MeshOptions {
    /// The furthest a triangle edge may sit from the surface it approximates,
    /// in metres.
    pub chord_tolerance: f64,
    /// How many times a triangle may be split to reach that tolerance across
    /// a face's interior. Splitting is decided per edge, so neighbouring
    /// triangles always agree and the mesh stays crack-free.
    pub refinement_depth: u8,
    /// A ceiling on one element's triangles, so a pathological body cannot
    /// consume the conversion.
    pub triangle_budget: usize,
}

impl Default for MeshOptions {
    fn default() -> Self {
        Self {
            chord_tolerance: 0.004,
            refinement_depth: 2,
            triangle_budget: 400_000,
        }
    }
}

/// An indexed triangle mesh in world coordinates, in metres.
///
/// `edge_positions` and `edge_indices` carry the source's own boundary curves
/// as line segments. They are not derived from the triangles: they are the
/// edges the B-rep declared, which is why a cylinder shows one silhouette
/// rather than every facet seam.
#[derive(Clone, Debug, Default)]
pub struct Mesh {
    /// `xyz` triples. Kept in double precision because world coordinates run
    /// to kilometres and the packer, not this crate, decides what to round to.
    pub positions: Vec<f64>,
    pub normals: Vec<f32>,
    pub indices: Vec<u32>,
    pub edge_positions: Vec<f64>,
    pub edge_indices: Vec<u32>,
    /// Faces the source declared that this could not read.
    pub skipped_faces: usize,
}

impl Mesh {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty() && self.edge_indices.is_empty()
    }

    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.positions.len() / 3
    }

    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// The mesh's own extent, or `None` when it holds no points.
    #[must_use]
    pub fn bounds(&self) -> Option<([f64; 3], [f64; 3])> {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        let mut seen = false;
        for chunk in self
            .positions
            .chunks_exact(3)
            .chain(self.edge_positions.chunks_exact(3))
        {
            seen = true;
            for axis in 0..3 {
                min[axis] = min[axis].min(chunk[axis]);
                max[axis] = max[axis].max(chunk[axis]);
            }
        }
        seen.then_some((min, max))
    }

    fn push_vertex(&mut self, at: Vec3, normal: Vec3) -> u32 {
        let index = self.vertex_count();
        self.positions.extend_from_slice(&at);
        self.normals
            .extend_from_slice(&[normal[0] as f32, normal[1] as f32, normal[2] as f32]);
        u32::try_from(index).unwrap_or(u32::MAX)
    }

    fn push_triangle(&mut self, a: u32, b: u32, c: u32) {
        if a == b || b == c || a == c {
            return;
        }
        self.indices.extend_from_slice(&[a, b, c]);
    }

    fn push_polyline(&mut self, points: &[Vec3], closed: bool) {
        if points.len() < 2 {
            return;
        }
        let base = self.edge_positions.len() / 3;
        for at in points {
            self.edge_positions.extend_from_slice(at);
        }
        let count = points.len();
        let last = if closed { count } else { count - 1 };
        for step in 0..last {
            let from = u32::try_from(base + step).unwrap_or(u32::MAX);
            let to = u32::try_from(base + (step + 1) % count).unwrap_or(u32::MAX);
            self.edge_indices.extend_from_slice(&[from, to]);
        }
    }

    /// Six times the volume the triangles enclose. Meaningful only for a
    /// closed shell, where its sign says whether the faces wind outward.
    fn signed_volume6(&self) -> f64 {
        let vertex = |index: u32| -> Vec3 {
            let at = index as usize * 3;
            [
                self.positions[at],
                self.positions[at + 1],
                self.positions[at + 2],
            ]
        };
        self.indices
            .chunks_exact(3)
            .map(|triangle| {
                let a = vertex(triangle[0]);
                let b = vertex(triangle[1]);
                let c = vertex(triangle[2]);
                dot(a, cross(b, c))
            })
            .sum()
    }

    /// Turn every triangle and every normal around.
    fn reverse(&mut self) {
        for triangle in self.indices.chunks_exact_mut(3) {
            triangle.swap(1, 2);
        }
        for component in &mut self.normals {
            *component = -*component;
        }
    }
}

/// Tessellate one element's geometry.
///
/// The result may be empty: an element whose only geometry is an axis line
/// contributes edges and no triangles, and an element whose faces this cannot
/// read contributes neither.
#[must_use]
pub fn tessellate(geometry: &BimGeometry, options: &MeshOptions) -> Mesh {
    let mut mesh = Mesh::default();
    match geometry {
        BimGeometry::Brep(brep) => tessellate_brep(brep, options, &mut mesh),
        // Each member is a closed solid in its own right, so meshing them into
        // one mesh is the whole of it.
        BimGeometry::Assembly(parts) => {
            for part in parts {
                tessellate_brep(part, options, &mut mesh);
            }
        }
        BimGeometry::SweptDisk(disk) => tessellate_swept_disk(disk, options, &mut mesh),
        BimGeometry::BoundingBox(box3) => tessellate_bounding_box(box3, &mut mesh),
        BimGeometry::AxisLine(line) => {
            if let (Some(start), Some(end)) = (point(&line.start), point(&line.end)) {
                mesh.push_polyline(&[start, end], false);
            }
        }
    }
    mesh
}

/// The extent of a geometry without tessellating it.
///
/// This reads only the points the source states - a face's edge endpoints, a
/// box's corners, a swept disk's ends widened by its radius - so a converter
/// can order elements in space before deciding what to build. An arc bulging
/// past its endpoints is not accounted for, which makes this a close bound
/// rather than a tight one; it is used for grouping, never for measurement.
#[must_use]
pub fn approximate_bounds(geometry: &BimGeometry) -> Option<([f64; 3], [f64; 3])> {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut seen = false;
    let mut widen = |at: Vec3, radius: f64| {
        seen = true;
        for axis in 0..3 {
            min[axis] = min[axis].min(at[axis] - radius);
            max[axis] = max[axis].max(at[axis] + radius);
        }
    };
    let widen_brep = |brep: &bim_core::BimBrep, widen: &mut dyn FnMut(Vec3, f64)| {
        for face in &brep.faces {
            for boundary in &face.loops {
                for edge in boundary {
                    if let Some(at) = point(&edge.start) {
                        widen(at, 0.0);
                    }
                    if let Some(at) = point(&edge.end) {
                        widen(at, 0.0);
                    }
                }
            }
        }
    };
    match geometry {
        BimGeometry::Brep(brep) => widen_brep(brep, &mut widen),
        BimGeometry::Assembly(parts) => {
            for part in parts {
                widen_brep(part, &mut widen);
            }
        }
        BimGeometry::BoundingBox(box3) => {
            if let (Some(low), Some(high)) = (point(&box3.min), point(&box3.max)) {
                widen(low, 0.0);
                widen(high, 0.0);
            }
        }
        BimGeometry::SweptDisk(disk) => {
            let radius = metres(
                disk.radius.value,
                disk.radius.unit.as_ref().map(|unit| unit.id.as_str()),
            )
            .unwrap_or(0.0);
            if let Some(at) = point(&disk.directrix.start) {
                widen(at, radius);
            }
            if let Some(at) = point(&disk.directrix.end) {
                widen(at, radius);
            }
        }
        BimGeometry::AxisLine(line) => {
            if let Some(at) = point(&line.start) {
                widen(at, 0.0);
            }
            if let Some(at) = point(&line.end) {
                widen(at, 0.0);
            }
        }
    }
    seen.then_some((min, max))
}

fn tessellate_brep(brep: &BimBrep, options: &MeshOptions, mesh: &mut Mesh) {
    for face in &brep.faces {
        if mesh.triangle_count() >= options.triangle_budget {
            mesh.skipped_faces += 1;
            continue;
        }
        if !tessellate_face(face, options, mesh) {
            mesh.skipped_faces += 1;
        }
    }
    // A shell the decoder vouched for as closed also fixes which way its faces
    // face: the winding that encloses a positive volume is the outward one.
    // Nothing is assumed about an open shell, whose sign means nothing.
    if brep.complete && mesh.skipped_faces == 0 && mesh.signed_volume6() < 0.0 {
        mesh.reverse();
    }
}

/// Tessellate one face, reporting whether it could be read at all.
fn tessellate_face(face: &BimBrepFace, options: &MeshOptions, mesh: &mut Mesh) -> bool {
    let Some(surface) = Surface::from_bim(&face.surface) else {
        return false;
    };
    let mut rings = Vec::with_capacity(face.loops.len());
    for boundary in &face.loops {
        let Some(points) = curve::sample_loop(boundary, options.chord_tolerance) else {
            return false;
        };
        mesh.push_polyline(&points, true);
        rings.push(points);
    }
    if rings.is_empty() {
        return false;
    }

    let period = surface.u_period();
    let unwrapped: Vec<Ring> = rings
        .iter()
        .map(|points| unwrap_ring(&surface, points, period))
        .collect();

    // A face that closes all the way round its surface has no outer loop in
    // the parameter plane: its boundary runs off one side and back on the
    // other. Two such loops bound a band, which is built directly.
    let encircling: Vec<&Ring> = unwrapped.iter().filter(|ring| ring.encircles).collect();
    if !encircling.is_empty() {
        let Some(period) = period else {
            return false;
        };
        if encircling.len() == 2 {
            build_band(
                &surface,
                encircling[0],
                encircling[1],
                period,
                options,
                mesh,
            );
            return true;
        }
        // One loop that winds once round bounds a patch, not a band: the
        // exporter walks half the circumference at one end of the tube, steps
        // along the axis, and walks the rest at the other, so two such faces
        // tile the wall between them. Unwrapped, that loop is a path across
        // one period which starts and ends at the same second parameter, and
        // the face is what lies between the path and that line - which is a
        // band with one side held constant.
        if encircling.len() == 1 && unwrapped.len() == 1 {
            // The path must cross the period once without turning back: then
            // it has a single second parameter at each point along the way,
            // and what lies between it and its own start line is the face. A
            // loop that doubles back is some other shape this does not read.
            let Some(path) = one_way_round(encircling[0], period) else {
                return false;
            };
            build_winding_patch(&surface, &path, period, mesh);
            return true;
        }
        return false;
    }

    // Which loop bounds what, by containment: a loop inside no other bounds a
    // region, a loop inside one of those is a hole in it, and a loop inside a
    // hole bounds a region again.
    //
    // Taking the largest loop for the boundary and the rest for holes is right
    // for the faces a kernel states and wrong for some of the ones this
    // decodes: on the 231 MB architectural model 1 054 faces carry a loop
    // lying wholly outside the loop that would have been chosen - two disjoint
    // regions stated on one face - and reading the second as a hole in the
    // first leaves a polygon that cannot be tiled at all.
    let mut vertices: Vec<Point2> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(unwrapped.len());
    for ring in &unwrapped {
        let start = vertices.len();
        vertices.extend_from_slice(&ring.uv);
        spans.push((start, vertices.len()));
    }
    let mut triangles = Vec::new();
    let mut tiled_every_region = true;
    for (outer, holes) in regions(&unwrapped) {
        let boundary = vertices[spans[outer].0..spans[outer].1].to_vec();
        let cut: Vec<Vec<Point2>> = holes
            .iter()
            .map(|hole| vertices[spans[*hole].0..spans[*hole].1].to_vec())
            .collect();
        let borrowed: Vec<&[Point2]> = cut.iter().map(Vec::as_slice).collect();
        let region = triangulate::triangulate(&boundary, &borrowed);
        if region.is_empty() {
            tiled_every_region = false;
            continue;
        }
        // `triangulate` indexes the boundary and then each hole in turn; the
        // caller's own numbering is what the rest of this works in.
        let mut at = |index: usize| {
            if index < boundary.len() {
                return spans[outer].0 + index;
            }
            let mut index = index - boundary.len();
            for (hole, points) in holes.iter().zip(&cut) {
                if index < points.len() {
                    return spans[*hole].0 + index;
                }
                index -= points.len();
            }
            spans[outer].0
        };
        triangles.extend(region.into_iter().map(|corners| corners.map(&mut at)));
    }
    if triangles.is_empty() {
        return false;
    }
    let triangles = refine(&surface, &mut vertices, triangles, options);
    emit(&surface, &vertices, &triangles, mesh);
    // A face is read when every region of it is: one region tiled out of two
    // is a face with a piece missing, which is what the count is for.
    tiled_every_region
}

/// Group a face's loops into the regions they bound: each outer loop with the
/// loops that are holes in it.
///
/// Containment is read from one point of each loop, which settles it for loops
/// that do not cross - and loops that cross bound nothing this can tile, so
/// the tiling refuses them rather than this. Depth decides the role: a loop
/// inside an even number of others bounds material, an odd number makes it a
/// hole, and a hole's own holes are regions inside it.
fn regions(rings: &[Ring]) -> Vec<(usize, Vec<usize>)> {
    let mut inside: Vec<Vec<usize>> = vec![Vec::new(); rings.len()];
    for (at, ring) in rings.iter().enumerate() {
        let Some(point) = ring.uv.first() else {
            continue;
        };
        for (other, around) in rings.iter().enumerate() {
            if other != at && encloses(&around.uv, *point) {
                inside[at].push(other);
            }
        }
    }
    let mut grouped = Vec::new();
    for at in 0..rings.len() {
        if inside[at].len() % 2 != 0 || rings[at].uv.len() < 3 {
            continue;
        }
        // Its holes are the loops one level in: inside this one, and inside
        // nothing else that is itself inside this one.
        let holes = (0..rings.len())
            .filter(|other| {
                inside[*other].len() == inside[at].len() + 1
                    && inside[*other].contains(&at)
                    && rings[*other].uv.len() >= 3
            })
            .collect();
        grouped.push((at, holes));
    }
    grouped
}

/// Is `point` inside this loop? Even-odd, the same rule the grouping's depth
/// count is built on.
fn encloses(ring: &[Point2], point: Point2) -> bool {
    let mut inside = false;
    for index in 0..ring.len() {
        let start = ring[index];
        let end = ring[(index + 1) % ring.len()];
        if (start[1] > point[1]) != (end[1] > point[1]) {
            let span = end[1] - start[1];
            if span.abs() <= f64::MIN_POSITIVE {
                continue;
            }
            let ratio = (point[1] - start[1]) / span;
            if (end[0] - start[0]).mul_add(ratio, start[0]) > point[0] {
                inside = !inside;
            }
        }
    }
    inside
}

/// One boundary loop carried into the surface's parameter plane, with the
/// first parameter unwrapped so a loop crossing the seam stays continuous.
struct Ring {
    uv: Vec<Point2>,
    /// Whether the loop goes all the way round a surface that closes.
    encircles: bool,
    /// Unsigned parameter area, used to pick the outer loop.
    area: f64,
}

fn unwrap_ring(surface: &Surface, points: &[Vec3], period: Option<f64>) -> Ring {
    let mut uv: Vec<Point2> = Vec::with_capacity(points.len());
    let mut previous: Option<f64> = None;
    for at in points {
        let mut current = surface.to_uv(*at);
        if let (Some(period), Some(previous)) = (period, previous) {
            let turns = ((previous - current[0]) / period).round();
            current[0] = period.mul_add(turns, current[0]);
        }
        previous = Some(current[0]);
        uv.push(current);
    }
    let encircles = period.is_some_and(|period| {
        uv.first().zip(uv.last()).is_some_and(|(first, last)| {
            (last[0] - first[0]).abs() > period * 0.5 || {
                // A loop stated as a closed circle repeats no point, so its
                // span falls one step short of a full turn.
                let closing = uv
                    .windows(2)
                    .map(|pair| (pair[1][0] - pair[0][0]).abs())
                    .fold(0.0_f64, f64::max);
                (last[0] - first[0]).abs() + closing > period * 0.9
            }
        })
    });
    let area = if encircles {
        0.0
    } else {
        let mut total = 0.0;
        for index in 0..uv.len() {
            let current = uv[index];
            let next = uv[(index + 1) % uv.len()];
            total += current[0].mul_add(next[1], -(next[0] * current[1]));
        }
        total.abs() / 2.0
    };
    Ring {
        uv,
        encircles,
        area,
    }
}

/// A winding loop as a path that crosses the period once in one direction, or
/// `None` where it turns back on itself.
///
/// The direction the source wound it in is not fixed, so a path running the
/// other way is reversed rather than refused.
fn one_way_round(ring: &Ring, period: f64) -> Option<Ring> {
    let first = *ring.uv.first()?;
    let last = *ring.uv.last()?;
    let mut steps: Vec<f64> = ring
        .uv
        .windows(2)
        .map(|pair| pair[1][0] - pair[0][0])
        .collect();
    // The step that closes the loop crosses the seam the way the rest of it
    // was walked: a loop wound the other way comes back a period below where
    // it started, not above.
    let travelled: f64 = steps.iter().sum();
    let closing = if travelled >= 0.0 {
        first[0] + period - last[0]
    } else {
        first[0] - period - last[0]
    };
    steps.push(closing);
    // The seam step closes the loop, so the tolerance is scaled to the period
    // rather than absolute: a point either side of a corner may wobble.
    let slack = period * 1e-9;
    let rising = steps.iter().all(|step| *step >= -slack);
    let falling = steps.iter().all(|step| *step <= slack);
    if rising == falling {
        // Either it turns back, or it never moves at all.
        return None;
    }
    if rising {
        return Some(Ring {
            uv: ring.uv.clone(),
            encircles: true,
            area: ring.area,
        });
    }
    let mut uv = ring.uv.clone();
    uv.reverse();
    // Reversed, the path now starts where it used to end and still has to
    // begin at the seam it will close a period later.
    Some(Ring {
        uv,
        encircles: true,
        area: ring.area,
    })
}

/// The face bounded by a loop that winds once round the surface.
///
/// The loop is walked as a path across one period, and the face is what lies
/// between that path and the second parameter the path set out from. Stitching
/// at the path's own points rather than at even steps keeps the corner where
/// it changes ends exactly where the source put it; where the path already
/// runs along that line the quads there are empty, which is the half of the
/// wall the neighbouring face covers.
fn build_winding_patch(surface: &Surface, ring: &Ring, period: f64, mesh: &mut Mesh) {
    let Some(first) = ring.uv.first().copied() else {
        return;
    };
    let held = first[1];
    let count = ring.uv.len();
    for index in 0..count {
        let a = ring.uv[index];
        let b = if index + 1 < count {
            ring.uv[index + 1]
        } else {
            [first[0] + period, held]
        };
        let lower_a = surface.push_uv(mesh, a);
        let lower_b = surface.push_uv(mesh, b);
        let upper_a = surface.push_uv(mesh, [a[0], held]);
        let upper_b = surface.push_uv(mesh, [b[0], held]);
        mesh.push_triangle(lower_a, lower_b, upper_b);
        mesh.push_triangle(lower_a, upper_b, upper_a);
    }
}

/// Tile the band between two loops that each go once round the surface.
fn build_band(
    surface: &Surface,
    first: &Ring,
    second: &Ring,
    period: f64,
    options: &MeshOptions,
    mesh: &mut Mesh,
) {
    let steps = first
        .uv
        .len()
        .max(second.uv.len())
        .max(curve::arc_segments(
            period / std::f64::consts::TAU,
            std::f64::consts::TAU,
            options.chord_tolerance,
        ))
        .min(512);
    let start = first.uv[0][0];
    let mut lower = Vec::with_capacity(steps + 1);
    let mut upper = Vec::with_capacity(steps + 1);
    for step in 0..=steps {
        let along = period.mul_add(step as f64 / steps as f64, start);
        let first_v = sample_ring_v(first, along, period);
        let second_v = sample_ring_v(second, along, period);
        lower.push(surface.push_uv(mesh, [along, first_v]));
        upper.push(surface.push_uv(mesh, [along, second_v]));
    }
    for step in 0..steps {
        mesh.push_triangle(lower[step], lower[step + 1], upper[step + 1]);
        mesh.push_triangle(lower[step], upper[step + 1], upper[step]);
    }
}

/// The second parameter of `ring` where its first parameter is `along`.
fn sample_ring_v(ring: &Ring, along: f64, period: f64) -> f64 {
    let first = ring.uv[0][0];
    let mut target = along;
    // Bring the query into the ring's own turn.
    let turns = ((first - target) / period).round();
    target = period.mul_add(turns, target);
    let count = ring.uv.len();
    for index in 0..count {
        let a = ring.uv[index];
        let mut b = ring.uv[(index + 1) % count];
        if index + 1 == count {
            let closing = ((a[0] - b[0]) / period).round();
            b[0] = period.mul_add(closing, b[0]);
        }
        let (low, high) = if a[0] <= b[0] { (a, b) } else { (b, a) };
        if target >= low[0] && target <= high[0] {
            let span = high[0] - low[0];
            if span <= f64::MIN_POSITIVE {
                return low[1];
            }
            let ratio = (target - low[0]) / span;
            return (high[1] - low[1]).mul_add(ratio, low[1]);
        }
    }
    ring.uv[0][1]
}

impl Surface {
    /// Place one parameter pair into `mesh` and return its vertex index.
    fn push_uv(&self, mesh: &mut Mesh, uv: Point2) -> u32 {
        mesh.push_vertex(self.to_xyz(uv), self.normal_at(uv))
    }
}

/// Split triangle edges until the surface between their endpoints is within
/// tolerance of them.
///
/// The decision is a function of an edge's two endpoints alone, so two
/// triangles sharing an edge always make the same one and the tiling stays
/// watertight. A planar face never splits, because its chord error is zero.
fn refine(
    surface: &Surface,
    vertices: &mut Vec<Point2>,
    mut triangles: Vec<[usize; 3]>,
    options: &MeshOptions,
) -> Vec<[usize; 3]> {
    if matches!(surface, Surface::Plane { .. }) {
        return triangles;
    }
    let mut midpoints: HashMap<(usize, usize), usize> = HashMap::new();
    for _ in 0..options.refinement_depth {
        if triangles.len() * 4 > options.triangle_budget {
            break;
        }
        let mut split = |vertices: &mut Vec<Point2>, a: usize, b: usize| -> Option<usize> {
            let key = if a < b { (a, b) } else { (b, a) };
            if let Some(existing) = midpoints.get(&key) {
                return Some(*existing);
            }
            let middle = [
                f64::midpoint(vertices[a][0], vertices[b][0]),
                f64::midpoint(vertices[a][1], vertices[b][1]),
            ];
            let chord = mix(
                surface.to_xyz(vertices[a]),
                surface.to_xyz(vertices[b]),
                0.5,
            );
            if distance(chord, surface.to_xyz(middle)) <= options.chord_tolerance {
                return None;
            }
            vertices.push(middle);
            let index = vertices.len() - 1;
            midpoints.insert(key, index);
            Some(index)
        };
        let mut next = Vec::with_capacity(triangles.len());
        let mut changed = false;
        for triangle in &triangles {
            let [a, b, c] = *triangle;
            let splits = [
                split(vertices, a, b),
                split(vertices, b, c),
                split(vertices, c, a),
            ];
            changed |= splits.iter().any(Option::is_some);
            emit_split(triangle, &splits, &mut next);
        }
        triangles = next;
        if !changed {
            break;
        }
    }
    triangles
}

/// Retile one triangle around whichever of its edges were split.
fn emit_split(triangle: &[usize; 3], splits: &[Option<usize>; 3], into: &mut Vec<[usize; 3]>) {
    let [a, b, c] = *triangle;
    match (splits[0], splits[1], splits[2]) {
        (None, None, None) => into.push([a, b, c]),
        (Some(m), None, None) => into.extend_from_slice(&[[a, m, c], [m, b, c]]),
        (None, Some(m), None) => into.extend_from_slice(&[[b, m, a], [m, c, a]]),
        (None, None, Some(m)) => into.extend_from_slice(&[[c, m, b], [m, a, b]]),
        (Some(first), Some(second), None) => {
            into.extend_from_slice(&[[a, first, second], [first, b, second], [a, second, c]]);
        }
        (None, Some(first), Some(second)) => {
            into.extend_from_slice(&[[b, first, second], [first, c, second], [b, second, a]]);
        }
        (Some(first), None, Some(second)) => {
            into.extend_from_slice(&[[c, second, first], [second, a, first], [c, first, b]]);
        }
        (Some(first), Some(second), Some(third)) => into.extend_from_slice(&[
            [a, first, third],
            [first, b, second],
            [third, second, c],
            [first, second, third],
        ]),
    }
}

fn emit(surface: &Surface, vertices: &[Point2], triangles: &[[usize; 3]], mesh: &mut Mesh) {
    let mut placed: Vec<Option<u32>> = vec![None; vertices.len()];
    for triangle in triangles {
        let mut indices = [0_u32; 3];
        for (slot, vertex) in triangle.iter().enumerate() {
            indices[slot] = *placed[*vertex].get_or_insert_with(|| {
                let uv = vertices[*vertex];
                mesh.push_vertex(surface.to_xyz(uv), surface.normal_at(uv))
            });
        }
        mesh.push_triangle(indices[0], indices[1], indices[2]);
    }
}

fn tessellate_swept_disk(disk: &BimSweptDisk, options: &MeshOptions, mesh: &mut Mesh) {
    let (Some(start), Some(end)) = (point(&disk.directrix.start), point(&disk.directrix.end))
    else {
        return;
    };
    let Some(radius) = metres(
        disk.radius.value,
        disk.radius.unit.as_ref().map(|unit| unit.id.as_str()),
    ) else {
        return;
    };
    let Some(axis) = normalize(subtract(end, start)) else {
        return;
    };
    if radius <= 0.0 {
        return;
    }
    // Any direction across the axis will do for the profile's own frame; the
    // one furthest from the axis keeps the cross product well conditioned.
    let seed = if axis[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let Some(x) = normalize(cross(seed, axis)) else {
        return;
    };
    let y = cross(axis, x);
    let steps = curve::arc_segments(radius, std::f64::consts::TAU, options.chord_tolerance).max(6);
    let mut lower = Vec::with_capacity(steps);
    let mut upper = Vec::with_capacity(steps);
    let mut lower_ring = Vec::with_capacity(steps);
    let mut upper_ring = Vec::with_capacity(steps);
    for step in 0..steps {
        let angle = std::f64::consts::TAU * step as f64 / steps as f64;
        let normal = add(scale(x, angle.cos()), scale(y, angle.sin()));
        let offset = scale(normal, radius);
        let at_start = add(start, offset);
        let at_end = add(end, offset);
        lower.push(mesh.push_vertex(at_start, normal));
        upper.push(mesh.push_vertex(at_end, normal));
        lower_ring.push(at_start);
        upper_ring.push(at_end);
    }
    for step in 0..steps {
        let next = (step + 1) % steps;
        mesh.push_triangle(lower[step], lower[next], upper[next]);
        mesh.push_triangle(lower[step], upper[next], upper[step]);
    }
    cap(mesh, &lower_ring, scale(axis, -1.0), true);
    cap(mesh, &upper_ring, axis, false);
    mesh.push_polyline(&lower_ring, true);
    mesh.push_polyline(&upper_ring, true);
}

/// Close one end of a tube with a fan.
fn cap(mesh: &mut Mesh, ring: &[Vec3], normal: Vec3, reversed: bool) {
    if ring.len() < 3 {
        return;
    }
    let mut center = [0.0; 3];
    for at in ring {
        center = add(center, *at);
    }
    let center = scale(center, 1.0 / ring.len() as f64);
    let hub = mesh.push_vertex(center, normal);
    let rim: Vec<u32> = ring
        .iter()
        .map(|at| mesh.push_vertex(*at, normal))
        .collect();
    for step in 0..rim.len() {
        let next = (step + 1) % rim.len();
        if reversed {
            mesh.push_triangle(hub, rim[next], rim[step]);
        } else {
            mesh.push_triangle(hub, rim[step], rim[next]);
        }
    }
}

/// Each face of a box, as four corner masks wound counter-clockwise seen from
/// outside, and the outward normal that goes with them.
const BOX_FACES: [([usize; 4], [f64; 3]); 6] = [
    ([0, 4, 6, 2], [-1.0, 0.0, 0.0]),
    ([1, 3, 7, 5], [1.0, 0.0, 0.0]),
    ([0, 1, 5, 4], [0.0, -1.0, 0.0]),
    ([2, 6, 7, 3], [0.0, 1.0, 0.0]),
    ([0, 2, 3, 1], [0.0, 0.0, -1.0]),
    ([4, 5, 7, 6], [0.0, 0.0, 1.0]),
];

fn tessellate_bounding_box(box3: &BimBoundingBox, mesh: &mut Mesh) {
    let (Some(min), Some(max)) = (point(&box3.min), point(&box3.max)) else {
        return;
    };
    let corner = |mask: usize| -> Vec3 {
        [
            if mask & 1 == 0 { min[0] } else { max[0] },
            if mask & 2 == 0 { min[1] } else { max[1] },
            if mask & 4 == 0 { min[2] } else { max[2] },
        ]
    };
    for (quad, normal) in BOX_FACES {
        let placed: Vec<u32> = quad
            .iter()
            .map(|mask| mesh.push_vertex(corner(*mask), normal))
            .collect();
        mesh.push_triangle(placed[0], placed[1], placed[2]);
        mesh.push_triangle(placed[0], placed[2], placed[3]);
        let ring: Vec<Vec3> = quad.iter().map(|mask| corner(*mask)).collect();
        mesh.push_polyline(&ring, true);
    }
}

#[cfg(test)]
mod tests {
    use super::{MeshOptions, tessellate};
    use bim_core::{
        BimBoundingBox, BimBrep, BimBrepArc, BimBrepCurve, BimBrepEdge, BimBrepFace,
        BimBrepSurface, BimGeometry, BimLineSegment, BimNumber, BimPoint3, BimSweptDisk, BimUnit,
    };

    fn metres_unit() -> BimUnit {
        BimUnit::new(super::METRES, "Meters")
    }

    fn at(coordinates: [f64; 3]) -> BimPoint3 {
        BimPoint3 {
            coordinates,
            unit: metres_unit(),
        }
    }

    fn line(start: [f64; 3], end: [f64; 3]) -> BimBrepEdge {
        BimBrepEdge {
            start: at(start),
            end: at(end),
            curve: BimBrepCurve::Line,
        }
    }

    /// The unit cube, stated as six planar faces wound outward.
    fn unit_cube() -> BimBrep {
        // Origin, the two in-plane axes, and the four corners of the ring.
        type PlanarFace = ([f64; 3], [f64; 3], [f64; 3], [[f64; 3]; 4]);
        let planes: [PlanarFace; 6] = [
            (
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
                [
                    [0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0],
                    [0.0, 1.0, 1.0],
                    [0.0, 0.0, 1.0],
                ],
            ),
            (
                [1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 0.0],
                [
                    [1.0, 0.0, 0.0],
                    [1.0, 0.0, 1.0],
                    [1.0, 1.0, 1.0],
                    [1.0, 1.0, 0.0],
                ],
            ),
            (
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [1.0, 0.0, 0.0],
                [
                    [0.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0],
                    [1.0, 0.0, 1.0],
                    [1.0, 0.0, 0.0],
                ],
            ),
            (
                [0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [
                    [0.0, 1.0, 0.0],
                    [1.0, 1.0, 0.0],
                    [1.0, 1.0, 1.0],
                    [0.0, 1.0, 1.0],
                ],
            ),
            (
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [
                    [0.0, 0.0, 0.0],
                    [1.0, 0.0, 0.0],
                    [1.0, 1.0, 0.0],
                    [0.0, 1.0, 0.0],
                ],
            ),
            (
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0],
                [
                    [0.0, 0.0, 1.0],
                    [0.0, 1.0, 1.0],
                    [1.0, 1.0, 1.0],
                    [1.0, 0.0, 1.0],
                ],
            ),
        ];
        let faces = planes
            .into_iter()
            .map(|(origin, x_axis, y_axis, ring)| BimBrepFace {
                surface: BimBrepSurface::Plane {
                    origin: at(origin),
                    x_axis,
                    y_axis,
                },
                loops: vec![vec![
                    line(ring[0], ring[1]),
                    line(ring[1], ring[2]),
                    line(ring[2], ring[3]),
                    line(ring[3], ring[0]),
                ]],
            })
            .collect();
        BimBrep {
            faces,
            complete: true,
        }
    }

    /// The total area of a mesh's triangles. A fold in the parameter plane
    /// shows up here as a multiple, where a bounding box would not move.
    fn area(mesh: &super::Mesh) -> f64 {
        mesh.indices
            .chunks_exact(3)
            .map(|face| {
                let corner = |i: usize| {
                    let at = face[i] as usize * 3;
                    [
                        mesh.positions[at],
                        mesh.positions[at + 1],
                        mesh.positions[at + 2],
                    ]
                };
                let (a, b, c) = (corner(0), corner(1), corner(2));
                let normal = super::cross(super::subtract(b, a), super::subtract(c, a));
                f64::sqrt(super::dot(normal, normal)) / 2.0
            })
            .sum()
    }

    fn volume(mesh: &super::Mesh) -> f64 {
        mesh.signed_volume6() / 6.0
    }

    #[test]
    fn a_cube_tessellates_to_twelve_triangles_enclosing_its_volume() {
        let mesh = tessellate(&BimGeometry::Brep(unit_cube()), &MeshOptions::default());
        assert_eq!(mesh.triangle_count(), 12);
        assert_eq!(mesh.skipped_faces, 0);
        assert!((volume(&mesh) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_closed_shell_comes_out_wound_outward() {
        // Reversing every loop must not flip the reported volume: a complete
        // shell is oriented by the volume it encloses, not by the order the
        // record happened to state its edges in.
        let mut brep = unit_cube();
        for face in &mut brep.faces {
            for boundary in &mut face.loops {
                boundary.reverse();
            }
        }
        let mesh = tessellate(&BimGeometry::Brep(brep), &MeshOptions::default());
        assert!(volume(&mesh) > 0.0);
    }

    /// A cylinder wall Revit states as one loop that winds once round, rather
    /// than as the two rim circles of a band: it runs along the axis, half way
    /// round at one end, back along the axis, and the other half at the other.
    /// This is the commonest face in a model full of holes, and reading it as
    /// a band left every such wall out of the mesh.
    fn half_tube(radius: f64, height: f64) -> BimBrepFace {
        let axis = |angle: f64, z: f64| [radius * f64::cos(angle), radius * f64::sin(angle), z];
        let arc = |start: [f64; 3], end: [f64; 3], z: f64, from: f64, to: f64| BimBrepEdge {
            start: at(start),
            end: at(end),
            curve: BimBrepCurve::Arc(BimBrepArc {
                center: at([0.0, 0.0, z]),
                x_axis: [1.0, 0.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                radius: BimNumber {
                    value: radius,
                    unit: Some(metres_unit()),
                },
                start_angle: from,
                end_angle: to,
            }),
        };
        let pi = std::f64::consts::PI;
        BimBrepFace {
            surface: BimBrepSurface::Cylinder {
                center: at([0.0, 0.0, 0.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                radius: BimNumber {
                    value: radius,
                    unit: Some(metres_unit()),
                },
            },
            loops: vec![vec![
                line(axis(0.0, height), axis(0.0, 0.0)),
                arc(axis(0.0, 0.0), axis(pi, 0.0), 0.0, 0.0, pi),
                line(axis(pi, 0.0), axis(pi, height)),
                arc(axis(pi, height), axis(0.0, height), height, -pi, 0.0),
            ]],
        }
    }

    #[test]
    fn a_wall_stated_as_one_winding_loop_is_read_rather_than_dropped() {
        let radius = 0.1;
        let height = 0.003;
        let brep = BimBrep {
            faces: vec![half_tube(radius, height)],
            complete: false,
        };
        let mesh = tessellate(&BimGeometry::Brep(brep), &MeshOptions::default());
        assert_eq!(mesh.skipped_faces, 0, "the winding loop was refused");
        assert!(mesh.triangle_count() > 0, "no triangles were built");

        // Every vertex must sit on the cylinder it was cut from, and inside
        // the band: a seam handled wrongly throws points off the surface.
        for xyz in mesh.positions.chunks_exact(3) {
            let off_axis = f64::hypot(xyz[0], xyz[1]);
            assert!(
                (off_axis - radius).abs() < 1e-9,
                "a vertex left the cylinder: {off_axis} against {radius}"
            );
            assert!(
                (-1e-9..=height + 1e-9).contains(&xyz[2]),
                "a vertex left the band: {}",
                xyz[2]
            );
        }

        // Half the circumference, times the height: the patch covers one side
        // of the tube, not nothing and not the whole of it.
        let area = area(&mesh);
        // Half the circumference times the height. The facets are inscribed
        // in the arc, so the patch comes out a little under that and never
        // over: a fold in the parameter plane shows up here as a multiple.
        let expected = std::f64::consts::PI * radius * height;
        assert!(
            (expected * 0.97..=expected).contains(&area),
            "the patch covers {area}, not the {expected} of half a tube"
        );
    }

    #[test]
    fn a_winding_loop_wound_the_other_way_reads_the_same() {
        // The direction the exporter wound the loop in is not fixed, and a
        // wall must not appear or vanish with it.
        let radius = 0.1;
        let height = 0.003;
        let mut face = half_tube(radius, height);
        face.loops[0].reverse();
        for edge in &mut face.loops[0] {
            std::mem::swap(&mut edge.start, &mut edge.end);
            if let BimBrepCurve::Arc(arc) = &mut edge.curve {
                std::mem::swap(&mut arc.start_angle, &mut arc.end_angle);
            }
        }
        let brep = BimBrep {
            faces: vec![face],
            complete: false,
        };
        let mesh = tessellate(&BimGeometry::Brep(brep), &MeshOptions::default());
        assert_eq!(
            mesh.skipped_faces, 0,
            "the reversed winding loop was refused"
        );

        let forward = tessellate(
            &BimGeometry::Brep(BimBrep {
                faces: vec![half_tube(radius, height)],
                complete: false,
            }),
            &MeshOptions::default(),
        );
        let (there, back) = (area(&forward), area(&mesh));
        assert!(
            (there - back).abs() < there * 1e-9,
            "the same wall covers {there} one way round and {back} the other"
        );
    }

    /// A face stating two loops that lie side by side bounds two regions, and
    /// both are tiled. Reading the second as a hole in the first - which is
    /// what taking the largest loop for the boundary does - leaves a polygon
    /// that tiles to nothing at all. 1 054 faces of the 231 MB architectural
    /// model are stated this way.
    #[test]
    fn a_face_whose_loops_lie_side_by_side_tiles_both() {
        let square = |x: f64, side: f64| {
            vec![
                line([x, 0.0, 0.0], [x + side, 0.0, 0.0]),
                line([x + side, 0.0, 0.0], [x + side, side, 0.0]),
                line([x + side, side, 0.0], [x, side, 0.0]),
                line([x, side, 0.0], [x, 0.0, 0.0]),
            ]
        };
        let face = BimBrepFace {
            surface: BimBrepSurface::Plane {
                origin: at([0.0, 0.0, 0.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            },
            loops: vec![square(0.0, 1.0), square(2.0, 3.0)],
        };
        let mesh = tessellate(
            &BimGeometry::Brep(BimBrep {
                faces: vec![face],
                complete: false,
            }),
            &MeshOptions::default(),
        );
        assert_eq!(mesh.skipped_faces, 0);
        assert!(
            (area(&mesh) - 10.0).abs() < 1e-9,
            "one square metre beside nine, tiled to {}",
            area(&mesh)
        );
    }

    /// And a loop that really is a hole is still a hole.
    #[test]
    fn a_face_whose_second_loop_is_inside_the_first_cuts_it_out() {
        let square = |x: f64, y: f64, side: f64, forward: bool| {
            let corners = [
                [x, y, 0.0],
                [x + side, y, 0.0],
                [x + side, y + side, 0.0],
                [x, y + side, 0.0],
            ];
            (0..4)
                .map(|at| {
                    let (from, to) = if forward {
                        (corners[at], corners[(at + 1) % 4])
                    } else {
                        (corners[(4 - at) % 4], corners[(3 - at) % 4])
                    };
                    line(from, to)
                })
                .collect::<Vec<_>>()
        };
        let face = BimBrepFace {
            surface: BimBrepSurface::Plane {
                origin: at([0.0, 0.0, 0.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            },
            loops: vec![square(0.0, 0.0, 3.0, true), square(1.0, 1.0, 1.0, false)],
        };
        let mesh = tessellate(
            &BimGeometry::Brep(BimBrep {
                faces: vec![face],
                complete: false,
            }),
            &MeshOptions::default(),
        );
        assert_eq!(mesh.skipped_faces, 0);
        assert!(
            (area(&mesh) - 8.0).abs() < 1e-9,
            "nine square metres less one, tiled to {}",
            area(&mesh)
        );
    }

    #[test]
    fn a_face_in_an_unreadable_unit_is_counted_rather_than_guessed() {
        let mut brep = unit_cube();
        brep.faces[0].loops[0][0].start.unit =
            BimUnit::new("autodesk.unit.unit:feet-1.0.0", "Feet");
        let mesh = tessellate(&BimGeometry::Brep(brep), &MeshOptions::default());
        assert_eq!(mesh.skipped_faces, 1);
        assert_eq!(mesh.triangle_count(), 10);
    }

    #[test]
    fn a_swept_disk_closes_and_holds_its_volume() {
        let disk = BimSweptDisk {
            directrix: BimLineSegment {
                start: at([0.0, 0.0, 0.0]),
                end: at([0.0, 0.0, 2.0]),
            },
            radius: BimNumber {
                value: 0.5,
                unit: Some(metres_unit()),
            },
        };
        let mesh = tessellate(&BimGeometry::SweptDisk(disk), &MeshOptions::default());
        // A tube is tiled by a prism, whose volume sits just inside the
        // cylinder's by exactly the chord tolerance the mesh was built to.
        let expected = std::f64::consts::PI * 0.25 * 2.0;
        assert!(volume(&mesh) < expected);
        assert!(volume(&mesh) > expected * 0.98);
    }

    #[test]
    fn a_bounding_box_encloses_exactly_its_extent() {
        let box3 = BimBoundingBox {
            min: at([1.0, 2.0, 3.0]),
            max: at([3.0, 5.0, 9.0]),
        };
        let mesh = tessellate(&BimGeometry::BoundingBox(box3), &MeshOptions::default());
        assert!((volume(&mesh) - 36.0).abs() < 1e-9);
        assert_eq!(mesh.bounds().map(|(min, _)| min), Some([1.0, 2.0, 3.0]));
    }
}
