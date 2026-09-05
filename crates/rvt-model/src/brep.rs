//! Assembles a `FamilySymbol`'s boundary representation from the generic
//! [`SerialObject`]s a [`crate::walk_record_collecting`] pass over its
//! `GElement` record produced.
//!
//! Every rule here was measured against record 278446 in the SMALL corpus
//! file, not assumed: loop/edge traversal closes 43/43 loops with 160/160
//! edge-loop incidences accounted for and zero residue; `EdgePnt`-to-3D
//! evaluation agrees across a shared edge's two adjacent faces to under
//! 1e-6 ft on 136/136 checks; and edge direction within a loop is exactly
//! `(m_flags & 1 != 0) != (side == 1)`, which closes 39/39 evaluable loops
//! (144/144 consecutive endpoints) in 3D - the remaining 4 of 43 loops in
//! that record bound `SurfRev` faces, which this module does not evaluate.
//! Coordinates stay in the symbol's own local frame and Revit internal feet;
//! callers apply the instance's `GInstance` transform afterward.
//!
//! What one record could not settle, `rivet brep FILE` measures over all of
//! them: faces resolved, bodies whose every face resolved, and each excluded
//! face grouped by the reason given here. Reading an edge from both adjacent
//! cylinders rather than dropping it was accepted on that - faces resolved
//! 32 083 / 17 513 / 42 563 -> 74 187 / 28 825 / 47 187 across the three corpus
//! files, bodies fully resolved 1 986 / 3 755 / 6 745 -> 12 065 / 6 536 / 7 535,
//! with the emitted IFC still geometrizing on every body and validating clean.

use std::collections::HashMap;

use crate::SerialObject;

/// A loop-closure and cross-face-agreement tolerance, in feet. Measured
/// agreement in the corpus was under 1e-6 ft; this leaves margin without
/// accepting a genuinely different point.
const CLOSURE_TOLERANCE_FEET: f64 = 1.0e-5;
/// How close an `EdgePnt` pair's `v` (or `u`) must be to call it constant,
/// in the same units the file stores (feet for `v`, radians for `u`).
const PARAMETER_TOLERANCE: f64 = 1.0e-6;
/// Guard against a malformed or cyclic edge/loop chain.
const MAX_LOOP_EDGES: usize = 1024;

/// Schema class indices this module needs, resolved once by the caller
/// (mirrors [`crate::PipeLineGeometryFields::parse`]'s `curve_driver_class_index`
/// pattern: this module never looks classes up by name itself).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrepClassIndexes {
    pub face: u16,
    pub edge_loop: u16,
    pub edge: u16,
    pub plane: u16,
    pub cyl_surf: u16,
}

/// One symbol's boundary representation, in the symbol's own local
/// coordinate system and Revit internal feet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SymbolBrep {
    pub faces: Vec<BrepFace>,
    /// Faces present in the record that could not be resolved, and why.
    /// Preserved rather than discarded, per the project's "mark unknown
    /// structure explicitly" rule.
    pub excluded_faces: Vec<BrepExclusion>,
    /// Every edge the record declares that did not resolve. A face is excluded
    /// by the *first* failing edge its loop reaches, so face exclusions
    /// undercount and cannot say how badly a reading missed; these can.
    pub failed_edges: Vec<BrepEdgeFailure>,
}

impl SymbolBrep {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrepExclusion {
    pub face_id: u32,
    pub reason: &'static str,
}

/// One edge that did not resolve, with the size of the discrepancy where the
/// failure has one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrepEdgeFailure {
    pub edge_id: u32,
    pub reason: &'static str,
    /// How far apart the two adjacent faces placed a shared endpoint, in feet.
    /// `None` for a failure that is not a disagreement between two readings.
    ///
    /// This is the number that separates a tolerance problem from a wrong
    /// surface: a fraction of a millimetre means the two readings agree and the
    /// tolerance is too tight, while feet means one of the two faces is being
    /// evaluated against a surface that is not its own.
    pub gap_feet: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrepFace {
    pub surface: BrepSurface,
    /// The face's boundary loops: the first is the outer bound, any further
    /// loops are holes (`GEdgeLoop.m_nextLoop`).
    pub loops: Vec<BrepLoop>,
}

pub type BrepLoop = Vec<BrepEdge>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrepEdge {
    pub start: [f64; 3],
    pub end: [f64; 3],
    pub curve: BrepCurve,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrepCurve {
    Line,
    Arc(BrepArc),
}

/// A circular arc. `point(angle) = center + radius * (cos(angle) * x_axis +
/// sin(angle) * cross(z_axis, x_axis))`; `start_angle`/`end_angle` are the
/// file's own `EdgePnt.u` values, so `end_angle >= start_angle` exactly when
/// the arc runs in the direction that formula traces with increasing angle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrepArc {
    pub center: [f64; 3],
    pub x_axis: [f64; 3],
    pub z_axis: [f64; 3],
    pub radius: f64,
    pub start_angle: f64,
    pub end_angle: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrepSurface {
    Plane {
        origin: [f64; 3],
        x_axis: [f64; 3],
        y_axis: [f64; 3],
    },
    Cylinder {
        center: [f64; 3],
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        z_axis: [f64; 3],
        radius: f64,
    },
}

/// Why one edge did not resolve, before it is attributed to an edge id.
#[derive(Clone, Copy, Debug, PartialEq)]
struct EdgeFailure {
    reason: &'static str,
    gap_feet: Option<f64>,
}

impl From<&'static str> for EdgeFailure {
    fn from(reason: &'static str) -> Self {
        Self {
            reason,
            gap_feet: None,
        }
    }
}

/// One `Edge` object's data needed by every loop that uses it, resolved once
/// regardless of how many faces reference it.
struct ResolvedEdge {
    pface: [u32; 2],
    /// Identifiers `[next0, next1, prev0, prev1]`, i.e. `Edge.identifiers[2..6]`.
    links: [u32; 4],
    /// 3D endpoints in the file's own `first`/`last` order, not loop order.
    raw_start: [f64; 3],
    raw_end: [f64; 3],
    /// The curve, also in the file's own `first`/`last` order.
    raw_curve: BrepCurve,
    flags: i64,
}

/// Assemble every face this record's objects describe. Faces whose surface
/// or boundary could not be resolved are counted in
/// [`SymbolBrep::excluded_faces`], not silently dropped.
#[must_use]
pub fn assemble(objects: &[SerialObject], classes: &BrepClassIndexes) -> SymbolBrep {
    let by_id: HashMap<u32, &SerialObject> = objects
        .iter()
        .filter(|object| {
            object.class_index == classes.edge_loop || object.class_index == classes.edge
        })
        .map(|object| (object.object_id, object))
        .collect();

    let face_surfaces = resolve_face_surfaces(objects, classes);
    let edges = resolve_edges(objects, classes, &face_surfaces);
    let mut failed_edges = edges
        .iter()
        .filter_map(|(id, resolved)| {
            resolved.as_ref().err().map(|failure| BrepEdgeFailure {
                edge_id: *id,
                reason: failure.reason,
                gap_feet: failure.gap_feet,
            })
        })
        .collect::<Vec<_>>();
    failed_edges.sort_by_key(|failure| failure.edge_id);

    let mut faces = Vec::new();
    let mut excluded_faces = Vec::new();
    for face in objects
        .iter()
        .filter(|object| object.class_index == classes.face)
    {
        match assemble_face(face, classes, &by_id, &edges, &face_surfaces) {
            Ok(assembled) => faces.push(assembled),
            Err(reason) => excluded_faces.push(BrepExclusion {
                face_id: face.object_id,
                reason,
            }),
        }
    }
    SymbolBrep {
        faces,
        excluded_faces,
        failed_edges,
    }
}

/// Pair each `Face` with its surface's numeric data. `Face.m_pSurf`'s
/// `object_id` is a real per-face identifier for a `Plane` - confirmed on
/// both 278446 (encounter order happened to agree there too) and 190108,
/// where interleaved `Plane`/`CylSurf` faces make encounter order diverge
/// from id order and a `Plane` is looked up by id directly instead. Every
/// `CylSurf` in a record still shares one sentinel id (measured on 278446:
/// all 18 `CylSurf` references share one id; 0xFFFFFFFF on 190108), so those
/// are still paired by encounter order: `objects` is the exact BFS dequeue
/// order the walk produced, which preserves the relative order of any
/// per-class subsequence, so the Kth face (by encounter order) whose surface
/// is `CylSurf` pairs with the Kth `CylSurf` object (by encounter order).
fn resolve_face_surfaces(
    objects: &[SerialObject],
    classes: &BrepClassIndexes,
) -> HashMap<u32, Option<BrepSurface>> {
    let planes_by_id: HashMap<u32, &SerialObject> = objects
        .iter()
        .filter(|object| object.class_index == classes.plane)
        .map(|object| (object.object_id, object))
        .collect();
    let mut cylinders = objects
        .iter()
        .filter(|object| object.class_index == classes.cyl_surf);
    let mut resolved = HashMap::new();
    for face in objects
        .iter()
        .filter(|object| object.class_index == classes.face)
    {
        let Some(surface_ref) = face
            .references
            .last()
            .filter(|reference| reference.object_id != 0)
        else {
            resolved.insert(face.object_id, None);
            continue;
        };
        let surface = if surface_ref.class_index == classes.plane {
            planes_by_id
                .get(&surface_ref.object_id)
                .copied()
                .and_then(plane_surface)
        } else if surface_ref.class_index == classes.cyl_surf {
            cylinders.next().and_then(cylinder_surface)
        } else {
            None
        };
        resolved.insert(face.object_id, surface);
    }
    resolved
}

fn plane_surface(object: &SerialObject) -> Option<BrepSurface> {
    let n = &object.numbers;
    if n.len() < 13 {
        return None;
    }
    Some(BrepSurface::Plane {
        origin: [n[4], n[5], n[6]],
        x_axis: [n[7], n[8], n[9]],
        y_axis: [n[10], n[11], n[12]],
    })
}

fn cylinder_surface(object: &SerialObject) -> Option<BrepSurface> {
    let n = &object.numbers;
    if n.len() < 17 {
        return None;
    }
    Some(BrepSurface::Cylinder {
        center: [n[4], n[5], n[6]],
        x_axis: [n[7], n[8], n[9]],
        y_axis: [n[10], n[11], n[12]],
        z_axis: [n[13], n[14], n[15]],
        radius: n[16],
    })
}

fn eval_uv(surface: BrepSurface, u: f64, v: f64) -> [f64; 3] {
    match surface {
        BrepSurface::Plane {
            origin,
            x_axis,
            y_axis,
        } => add3(origin, add3(scale3(x_axis, u), scale3(y_axis, v))),
        BrepSurface::Cylinder {
            center,
            x_axis,
            y_axis,
            z_axis,
            radius,
        } => {
            let (cu, su) = (u.cos(), u.sin());
            let radial = add3(scale3(x_axis, radius * cu), scale3(y_axis, radius * su));
            add3(center, add3(radial, scale3(z_axis, v)))
        }
    }
}

fn resolve_edges(
    objects: &[SerialObject],
    classes: &BrepClassIndexes,
    face_surfaces: &HashMap<u32, Option<BrepSurface>>,
) -> HashMap<u32, Result<ResolvedEdge, EdgeFailure>> {
    objects
        .iter()
        .filter(|object| object.class_index == classes.edge)
        .map(|edge| (edge.object_id, resolve_edge(edge, face_surfaces)))
        .collect()
}

fn resolve_edge(
    edge: &SerialObject,
    face_surfaces: &HashMap<u32, Option<BrepSurface>>,
) -> Result<ResolvedEdge, EdgeFailure> {
    if edge.identifiers.len() != 6 {
        return Err("edge does not declare six identifiers".into());
    }
    let Some(&flags) = edge.small_integers.first() else {
        return Err("edge has no m_flags".into());
    };
    if edge.numbers.len() < 8 {
        return Err("edge has no first/last EdgePnt".into());
    }
    let tail = &edge.numbers[edge.numbers.len() - 8..];
    let (first_pnt, last_pnt) = (&tail[0..4], &tail[4..8]);
    let pface = [edge.identifiers[0], edge.identifiers[1]];
    let links = [
        edge.identifiers[2],
        edge.identifiers[3],
        edge.identifiers[4],
        edge.identifiers[5],
    ];

    let surfaces = [
        face_surfaces.get(&pface[0]).copied().flatten(),
        face_surfaces.get(&pface[1]).copied().flatten(),
    ];
    let point_for = |side: usize, pnt: &[f64]| -> Option<[f64; 3]> {
        surfaces[side].map(|surface| eval_uv(surface, pnt[side * 2], pnt[side * 2 + 1]))
    };
    let (raw_start, raw_end) = pick_agreeing_endpoints(
        point_for(0, first_pnt),
        point_for(1, first_pnt),
        point_for(0, last_pnt),
        point_for(1, last_pnt),
    )?;

    let classify = |side: usize| {
        classify_cylinder_edge(
            surfaces[side].expect("caller checked this side is a cylinder"),
            (first_pnt[side * 2], first_pnt[side * 2 + 1]),
            (last_pnt[side * 2], last_pnt[side * 2 + 1]),
            raw_start,
            raw_end,
        )
    };
    let raw_curve = match (surfaces[0], surfaces[1]) {
        // An edge between two cylinders is read from both parameterisations
        // and only accepted when they describe the same 3D curve. Neither side
        // is privileged and neither is guessed at: the file stores this edge's
        // `u`/`v` on each adjacent face, so each says on its own what the curve
        // is, and the two are an oracle for each other.
        //
        // The geometry says the two must agree. The only circles lying on a
        // right circular cylinder are its cross-sections, and the only straight
        // lines are its rulings; so a circle here is constant-`v` on *both*
        // cylinders and a line is constant-`u` on both. A tee's seam, where a
        // branch meets a run, is a quartic that is neither on either side, and
        // is still excluded rather than approximated by its endpoints.
        (Some(BrepSurface::Cylinder { .. }), Some(BrepSurface::Cylinder { .. })) => {
            match (classify(0), classify(1)) {
                (Ok(first), Ok(second)) => {
                    if curves_agree(first, second) {
                        first
                    } else {
                        return Err("the two cylinders disagree on the edge's curve".into());
                    }
                }
                // Asymmetry is a contradiction, not a partial success: a curve
                // that is a cross-section or a ruling of one cylinder is one of
                // the other too. Taking the side that answered would be reading
                // through a disagreement.
                (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
                    return Err("only one of the two cylinders gives the edge a curve".into());
                }
                (Err(reason), Err(_)) => return Err(reason.into()),
            }
        }
        (Some(BrepSurface::Cylinder { .. }), _) => classify(0).map_err(EdgeFailure::from)?,
        (_, Some(BrepSurface::Cylinder { .. })) => classify(1).map_err(EdgeFailure::from)?,
        _ => straight_line(raw_start, raw_end).map_err(EdgeFailure::from)?,
    };

    Ok(ResolvedEdge {
        pface,
        links,
        raw_start,
        raw_end,
        raw_curve,
        flags,
    })
}

/// Prefer whichever side(s) evaluated; when both did, they must agree.
fn pick_agreeing_endpoints(
    start0: Option<[f64; 3]>,
    start1: Option<[f64; 3]>,
    end0: Option<[f64; 3]>,
    end1: Option<[f64; 3]>,
) -> Result<([f64; 3], [f64; 3]), EdgeFailure> {
    let start = agree(start0, start1)?;
    let end = agree(end0, end1)?;
    Ok((start, end))
}

fn agree(a: Option<[f64; 3]>, b: Option<[f64; 3]>) -> Result<[f64; 3], EdgeFailure> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let gap = distance(a, b);
            if gap <= CLOSURE_TOLERANCE_FEET {
                Ok(a)
            } else {
                Err(EdgeFailure {
                    reason: "cross-face EdgePnt evaluation disagreed",
                    gap_feet: Some(gap),
                })
            }
        }
        (Some(point), None) | (None, Some(point)) => Ok(point),
        (None, None) => Err(EdgeFailure::from(
            "neither adjacent face has a usable surface",
        )),
    }
}

fn straight_line(start: [f64; 3], end: [f64; 3]) -> Result<BrepCurve, &'static str> {
    if distance(start, end) <= CLOSURE_TOLERANCE_FEET {
        return Err("degenerate zero-length edge");
    }
    Ok(BrepCurve::Line)
}

fn classify_cylinder_edge(
    cylinder: BrepSurface,
    (u_start, v_start): (f64, f64),
    (u_end, v_end): (f64, f64),
    raw_start: [f64; 3],
    raw_end: [f64; 3],
) -> Result<BrepCurve, &'static str> {
    let BrepSurface::Cylinder {
        center,
        x_axis,
        y_axis,
        z_axis,
        radius,
    } = cylinder
    else {
        unreachable!("caller passed a Cylinder surface");
    };
    let angular_span = (u_end - u_start).abs();
    if (v_end - v_start).abs() <= PARAMETER_TOLERANCE && angular_span > PARAMETER_TOLERANCE {
        if angular_span > std::f64::consts::TAU + PARAMETER_TOLERANCE {
            return Err("cylinder edge angular span exceeds a full turn");
        }
        let height = v_start.midpoint(v_end);
        let arc = BrepArc {
            center: add3(center, scale3(z_axis, height)),
            x_axis,
            z_axis,
            radius,
            start_angle: u_start,
            end_angle: u_end,
        };
        if distance(arc_point(arc, u_start), raw_start) > CLOSURE_TOLERANCE_FEET
            || distance(arc_point(arc, u_end), raw_end) > CLOSURE_TOLERANCE_FEET
        {
            return Err("cylinder arc reconstruction disagreed with the evaluated endpoints");
        }
        return Ok(BrepCurve::Arc(arc));
    }
    if (u_end - u_start).abs() <= PARAMETER_TOLERANCE
        && (v_end - v_start).abs() > PARAMETER_TOLERANCE
    {
        return straight_line(raw_start, raw_end);
    }
    let _ = y_axis;
    Err("cylinder edge parametrization is neither a constant-v arc nor a constant-u line")
}

/// Whether two curves derived independently over the same endpoints describe
/// the same 3D curve.
///
/// Compared geometrically rather than field by field: each side's angles and
/// axes are expressed in its own cylinder's frame, so two correct readings of
/// one circle need not share a single number. The endpoints already agree - a
/// resolved edge is built from `raw_start`/`raw_end`, which both sides
/// evaluated - so what is left to check is the path between them, and sampling
/// the midpoint catches the case the endpoints cannot: the same circle traced
/// the long way round instead of the short.
fn curves_agree(first: BrepCurve, second: BrepCurve) -> bool {
    match (first, second) {
        (BrepCurve::Line, BrepCurve::Line) => true,
        (BrepCurve::Arc(first), BrepCurve::Arc(second)) => {
            (first.radius - second.radius).abs() <= CLOSURE_TOLERANCE_FEET
                && distance(first.center, second.center) <= CLOSURE_TOLERANCE_FEET
                && distance(
                    arc_point(first, first.start_angle.midpoint(first.end_angle)),
                    arc_point(second, second.start_angle.midpoint(second.end_angle)),
                ) <= CLOSURE_TOLERANCE_FEET
        }
        (BrepCurve::Line, BrepCurve::Arc(_)) | (BrepCurve::Arc(_), BrepCurve::Line) => false,
    }
}

fn arc_point(arc: BrepArc, angle: f64) -> [f64; 3] {
    let y_axis = cross3(arc.z_axis, arc.x_axis);
    let (c, s) = (angle.cos(), angle.sin());
    add3(
        arc.center,
        add3(
            scale3(arc.x_axis, arc.radius * c),
            scale3(y_axis, arc.radius * s),
        ),
    )
}

fn assemble_face(
    face: &SerialObject,
    classes: &BrepClassIndexes,
    by_id: &HashMap<u32, &SerialObject>,
    edges: &HashMap<u32, Result<ResolvedEdge, EdgeFailure>>,
    face_surfaces: &HashMap<u32, Option<BrepSurface>>,
) -> Result<BrepFace, &'static str> {
    let surface = face_surfaces
        .get(&face.object_id)
        .copied()
        .flatten()
        .ok_or("face has no supported surface")?;
    let Some(first_loop) = face
        .references
        .first()
        .filter(|reference| reference.object_id != 0)
    else {
        return Err("face has no first loop");
    };
    let loop_object = *by_id
        .get(&first_loop.object_id)
        .ok_or("referenced loop is missing")?;
    if loop_object.class_index != classes.edge_loop {
        return Err("loop reference does not name an EdgeLoop");
    }
    let loop_edges = walk_loop(face.object_id, loop_object, by_id, classes, edges)?;
    // `GEdgeLoop.m_nextLoop` (a further loop for a face with holes) is
    // declared in the schema, but its "no further loop" sentinel is not a
    // null (0) reference: loop 162 in record 278446, a face with no holes,
    // carries `references.first() = (8, class 0)` there, not `(0, _)`. Every
    // face in that record resolves to exactly one loop, so nothing here has
    // exercised what a real second loop looks like. Rather than guess at the
    // sentinel, only the first loop is read; a face with holes is not yet
    // representable and is not claimed to be.
    Ok(BrepFace {
        surface,
        loops: vec![loop_edges],
    })
}

fn walk_loop(
    face_id: u32,
    loop_object: &SerialObject,
    by_id: &HashMap<u32, &SerialObject>,
    classes: &BrepClassIndexes,
    edges: &HashMap<u32, Result<ResolvedEdge, EdgeFailure>>,
) -> Result<BrepLoop, &'static str> {
    if loop_object.identifiers.len() != 3 {
        return Err("loop does not declare pFace/next/prev");
    }
    let [loop_face, first_edge, _last_edge] = [
        loop_object.identifiers[0],
        loop_object.identifiers[1],
        loop_object.identifiers[2],
    ];
    if loop_face != face_id {
        return Err("loop's pFace does not match its owning face");
    }

    let mut ordered = Vec::new();
    let mut cur = first_edge;
    let mut previous_end: Option<[f64; 3]> = None;
    loop {
        if ordered.len() >= MAX_LOOP_EDGES {
            return Err("loop exceeded the edge guard");
        }
        let resolved = edges
            .get(&cur)
            .ok_or("loop edge is not an Edge object")?
            .as_ref()
            .map_err(|failure| failure.reason)?;
        let side = if resolved.pface[0] == face_id {
            0
        } else if resolved.pface[1] == face_id {
            1
        } else {
            return Err("edge's pFace does not name this loop's face");
        };
        let reverse = (resolved.flags & 1 != 0) != (side == 1);
        let (start, end, curve) = if reverse {
            (
                resolved.raw_end,
                resolved.raw_start,
                reverse_curve(resolved.raw_curve),
            )
        } else {
            (resolved.raw_start, resolved.raw_end, resolved.raw_curve)
        };
        if let Some(previous_end) = previous_end {
            if distance(previous_end, start) > CLOSURE_TOLERANCE_FEET {
                return Err("consecutive edges do not share an endpoint");
            }
        }
        previous_end = Some(end);
        ordered.push(BrepEdge { start, end, curve });

        // `links = identifiers[2..6] = [next0, next1, prev0, prev1]`, so the
        // next-edge pointer for this side is `links[side]`, not `links[2+side]`
        // (that would be the *previous*-edge pointer).
        let next = resolved.links[side];
        if next == loop_object.object_id {
            break;
        }
        let Some(candidate) = by_id.get(&next) else {
            return Err("edge's next link is neither this loop nor a known edge");
        };
        if candidate.class_index != classes.edge {
            return Err("edge's next link does not name an Edge object");
        }
        cur = next;
    }
    let start_point = ordered.first().map(|edge| edge.start);
    if let (Some(start), Some(end)) = (start_point, previous_end) {
        if distance(start, end) > CLOSURE_TOLERANCE_FEET {
            return Err("loop did not close in 3D");
        }
    }
    Ok(ordered)
}

fn reverse_curve(curve: BrepCurve) -> BrepCurve {
    match curve {
        BrepCurve::Line => BrepCurve::Line,
        BrepCurve::Arc(arc) => BrepCurve::Arc(BrepArc {
            start_angle: arc.end_angle,
            end_angle: arc.start_angle,
            ..arc
        }),
    }
}

fn add3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale3(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GElementNodeReference;

    const FACE: u16 = 1649;
    const EDGE_LOOP: u16 = 1311;
    const EDGE: u16 = 1300;
    const PLANE: u16 = 565;
    const CYL_SURF: u16 = 1039;

    fn classes() -> BrepClassIndexes {
        BrepClassIndexes {
            face: FACE,
            edge_loop: EDGE_LOOP,
            edge: EDGE,
            plane: PLANE,
            cyl_surf: CYL_SURF,
        }
    }

    fn reference(object_id: u32, class_index: u16) -> GElementNodeReference {
        GElementNodeReference {
            object_id,
            class_index,
        }
    }

    fn plane_object(id: u32, origin: [f64; 3], x: [f64; 3], y: [f64; 3]) -> SerialObject {
        let mut numbers = vec![0.0; 4];
        numbers.extend(origin);
        numbers.extend(x);
        numbers.extend(y);
        SerialObject {
            object_id: id,
            class_index: PLANE,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
        }
    }

    /// A cylinder of `radius` about the +Z axis through `center`.
    fn cylinder_object(id: u32, center: [f64; 3], radius: f64) -> SerialObject {
        let mut numbers = vec![0.0; 4];
        numbers.extend(center);
        numbers.extend([1.0, 0.0, 0.0]); // x_axis
        numbers.extend([0.0, 1.0, 0.0]); // y_axis
        numbers.extend([0.0, 0.0, 1.0]); // z_axis
        numbers.push(radius);
        SerialObject {
            object_id: id,
            class_index: CYL_SURF,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
        }
    }

    fn face_object(id: u32, first_loop: u32, surface: GElementNodeReference) -> SerialObject {
        SerialObject {
            object_id: id,
            class_index: FACE,
            offset: 0,
            bytes: 0,
            references: vec![reference(first_loop, EDGE_LOOP), surface],
            identifiers: Vec::new(),
            numbers: Vec::new(),
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
        }
    }

    fn loop_object(id: u32, face: u32, first_edge: u32, last_edge: u32) -> SerialObject {
        SerialObject {
            object_id: id,
            class_index: EDGE_LOOP,
            offset: 0,
            bytes: 0,
            references: vec![reference(0, EDGE_LOOP)],
            identifiers: vec![face, first_edge, last_edge],
            numbers: Vec::new(),
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
        }
    }

    /// A straight edge whose side-0 face is the only side with a resolvable
    /// surface in these fixtures (side 1's face is a placeholder id with no
    /// `Face` object, so its evaluation is always `None` and only side 0's
    /// `(u, v)` pairs matter).
    #[allow(clippy::too_many_arguments)]
    fn line_edge(
        id: u32,
        face0: u32,
        face1: u32,
        next: [u32; 2],
        prev: [u32; 2],
        flags: i64,
        first_uv: (f64, f64),
        last_uv: (f64, f64),
    ) -> SerialObject {
        SerialObject {
            object_id: id,
            class_index: EDGE,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: vec![face0, face1, next[0], next[1], prev[0], prev[1]],
            numbers: vec![
                first_uv.0, first_uv.1, 0.0, 0.0, last_uv.0, last_uv.1, 0.0, 0.0,
            ],
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: vec![flags],
        }
    }

    /// An edge carrying a real `(u, v)` pair on *both* adjacent faces, which
    /// is what a cylinder-to-cylinder edge needs: `line_edge` leaves side 1's
    /// pair at zero because its fixtures only ever resolve side 0.
    #[allow(clippy::too_many_arguments)]
    fn two_sided_edge(
        id: u32,
        faces: [u32; 2],
        next: [u32; 2],
        prev: [u32; 2],
        flags: i64,
        first_uv: [(f64, f64); 2],
        last_uv: [(f64, f64); 2],
    ) -> SerialObject {
        SerialObject {
            object_id: id,
            class_index: EDGE,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: vec![faces[0], faces[1], next[0], next[1], prev[0], prev[1]],
            numbers: vec![
                first_uv[0].0,
                first_uv[0].1,
                first_uv[1].0,
                first_uv[1].1,
                last_uv[0].0,
                last_uv[0].1,
                last_uv[1].0,
                last_uv[1].1,
            ],
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: vec![flags],
        }
    }

    /// A tube of radius 2 running from z=0 to z=5, split lengthwise into two
    /// half-cylinder faces that meet along the rulings at u=0 and u=pi. Face 1
    /// is the u in [0, pi] half; face 2 carries the other half and is left
    /// without a loop, so only face 1 is assembled. Both rulings are
    /// cylinder-to-cylinder edges - the shape this module used to drop
    /// wholesale - while the two arcs close against unmodeled caps (face 3).
    ///
    /// `seam` replaces the v=5 arc's side-1 parametrization, which is how the
    /// disagreement case is built.
    fn split_tube(seam: Option<[(f64, f64); 2]>) -> Vec<SerialObject> {
        let pi = std::f64::consts::PI;
        let arc_side_one = seam.unwrap_or([(0.0, 5.0), (pi, 5.0)]);
        let arc_face_one = if seam.is_some() { 2 } else { 3 };
        vec![
            cylinder_object(u32::MAX, [0.0, 0.0, 0.0], 2.0),
            cylinder_object(u32::MAX, [0.0, 0.0, 0.0], 2.0),
            face_object(1, 10, reference(u32::MAX, CYL_SURF)),
            face_object(2, 0, reference(u32::MAX, CYL_SURF)),
            loop_object(10, 1, 300, 303),
            // The u=0 ruling, shared by the two halves: constant u on both.
            two_sided_edge(
                300,
                [1, 2],
                [301, 0],
                [10, 0],
                0,
                [(0.0, 0.0), (0.0, 0.0)],
                [(0.0, 5.0), (0.0, 5.0)],
            ),
            // The v=5 arc.
            two_sided_edge(
                301,
                [1, arc_face_one],
                [302, 0],
                [300, 0],
                0,
                [(0.0, 5.0), arc_side_one[0]],
                [(pi, 5.0), arc_side_one[1]],
            ),
            // The u=pi ruling, again shared by the two halves.
            two_sided_edge(
                302,
                [1, 2],
                [303, 0],
                [301, 0],
                0,
                [(pi, 5.0), (pi, 5.0)],
                [(pi, 0.0), (pi, 0.0)],
            ),
            // The v=0 arc, back to the start.
            two_sided_edge(
                303,
                [1, 3],
                [10, 0],
                [302, 0],
                0,
                [(pi, 0.0), (0.0, 0.0)],
                [(0.0, 0.0), (0.0, 0.0)],
            ),
        ]
    }

    #[test]
    fn reads_an_edge_between_two_cylinders_from_both_parametrizations() {
        let brep = assemble(&split_tube(None), &classes());
        let tube = brep
            .faces
            .iter()
            .find(|face| matches!(face.surface, BrepSurface::Cylinder { .. }))
            .expect("the half-tube face resolved");
        let edges = &tube.loops[0];
        assert_eq!(edges.len(), 4);

        // The two rulings are cylinder-to-cylinder edges and now resolve;
        // before, either one excluded the whole face.
        assert_eq!(edges[0].curve, BrepCurve::Line);
        assert_eq!(edges[2].curve, BrepCurve::Line);
        assert!(distance(edges[0].start, [2.0, 0.0, 0.0]) < 1.0e-9);
        assert!(distance(edges[0].end, [2.0, 0.0, 5.0]) < 1.0e-9);
        assert!(distance(edges[2].start, [-2.0, 0.0, 5.0]) < 1.0e-9);
        assert!(distance(edges[2].end, [-2.0, 0.0, 0.0]) < 1.0e-9);

        // The arcs against the unmodeled caps are unaffected.
        for index in [1, 3] {
            match edges[index].curve {
                BrepCurve::Arc(arc) => assert!((arc.radius - 2.0).abs() < 1.0e-9),
                BrepCurve::Line => panic!("edge {index} should be an arc"),
            }
        }
    }

    #[test]
    fn rejects_a_cylinder_edge_the_two_faces_read_as_different_arcs() {
        // The same seam, but side 1 sweeps u from 0 to -pi where side 0 sweeps
        // 0 to +pi. Both readings share the edge's endpoints exactly - (2,0,5)
        // and (-2,0,5) - and both reconstruct against them, so nothing short of
        // comparing the path between them tells the two apart. They are
        // opposite halves of one circle.
        let pi = std::f64::consts::PI;
        let brep = assemble(&split_tube(Some([(0.0, 5.0), (-pi, 5.0)])), &classes());
        assert!(
            brep.faces.is_empty(),
            "a face resolved through a disagreement: {:?}",
            brep.faces
        );
        assert!(
            brep.excluded_faces
                .iter()
                .any(|exclusion| exclusion.face_id == 1
                    && exclusion.reason == "the two cylinders disagree on the edge's curve"),
            "{:?}",
            brep.excluded_faces
        );
    }

    #[test]
    fn assembles_a_planar_rectangle_from_four_line_edges() {
        // A unit square in the XY plane: origin (0,0,0), x_axis=(1,0,0),
        // y_axis=(0,1,0). EdgePnt (u,v) evaluates to (u,v,0) on this face.
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 10, reference(900, PLANE));
        let corner_loop = loop_object(10, 1, 100, 103);
        // A closed cycle 100 -> 101 -> 102 -> 103 -> back to loop 10. The loop
        // itself is the sentinel: the first edge's prev and the last edge's
        // next both point at the loop's own id, not at each other directly
        // (measured: "the first edge's prev[i] is the loop id, the last
        // edge's next[i] is the loop id"). Each edge names face 1 as its
        // only real side (face 2 is a placeholder unused in this fixture)
        // and flags=0 so side 0 is never reversed.
        let edges = vec![
            line_edge(100, 1, 2, [101, 0], [10, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(101, 1, 2, [102, 0], [100, 0], 0, (1.0, 0.0), (1.0, 1.0)),
            line_edge(102, 1, 2, [103, 0], [101, 0], 0, (1.0, 1.0), (0.0, 1.0)),
            line_edge(103, 1, 2, [10, 0], [102, 0], 0, (0.0, 1.0), (0.0, 0.0)),
        ];
        let mut objects = vec![plane, face, corner_loop];
        objects.extend(edges);

        let brep = assemble(&objects, &classes());
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        assert_eq!(brep.faces.len(), 1);
        let face = &brep.faces[0];
        assert_eq!(
            face.surface,
            BrepSurface::Plane {
                origin: [0.0, 0.0, 0.0],
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            }
        );
        assert_eq!(face.loops.len(), 1);
        let points: Vec<[f64; 3]> = face.loops[0].iter().map(|edge| edge.start).collect();
        assert_eq!(
            points,
            [
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0]
            ]
        );
        assert!(
            face.loops[0]
                .iter()
                .all(|edge| edge.curve == BrepCurve::Line)
        );
    }

    #[test]
    fn reverses_an_edge_when_its_flag_and_side_disagree() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 10, reference(900, PLANE));
        let corner_loop = loop_object(10, 1, 100, 103);
        // side==0 and flags&1==1: reverse = true != false = true, so this
        // edge's file order (1,0,0)->(0,0,0) is used end-to-start here.
        let edges = vec![
            line_edge(100, 1, 2, [101, 0], [10, 0], 1, (1.0, 0.0), (0.0, 0.0)),
            line_edge(101, 1, 2, [102, 0], [100, 0], 0, (1.0, 0.0), (1.0, 1.0)),
            line_edge(102, 1, 2, [103, 0], [101, 0], 0, (1.0, 1.0), (0.0, 1.0)),
            line_edge(103, 1, 2, [10, 0], [102, 0], 0, (0.0, 1.0), (0.0, 0.0)),
        ];
        let mut objects = vec![plane, face, corner_loop];
        objects.extend(edges);

        let brep = assemble(&objects, &classes());
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        let start = brep.faces[0].loops[0][0].start;
        assert!(distance(start, [0.0, 0.0, 0.0]) < 1.0e-9);
    }

    #[test]
    fn excludes_a_face_whose_loop_does_not_close_in_3d() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 10, reference(900, PLANE));
        let corner_loop = loop_object(10, 1, 100, 101);
        // Two edges whose endpoints do not meet: not a valid loop.
        let edges = vec![
            line_edge(100, 1, 2, [101, 0], [101, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(101, 1, 2, [100, 0], [100, 0], 0, (5.0, 5.0), (0.0, 0.0)),
        ];
        let mut objects = vec![plane, face, corner_loop];
        objects.extend(edges);

        let brep = assemble(&objects, &classes());
        assert!(brep.faces.is_empty());
        assert_eq!(brep.excluded_faces.len(), 1);
        assert_eq!(brep.excluded_faces[0].face_id, 1);
    }

    #[test]
    fn excludes_a_face_with_no_supported_surface() {
        // SurfRev (or anything not Plane/CylSurf) is unresolved by design.
        let face = face_object(1, 10, reference(5, 9999));
        let brep = assemble(&[face], &classes());
        assert!(brep.faces.is_empty());
        assert_eq!(
            brep.excluded_faces[0].reason,
            "face has no supported surface"
        );
    }

    #[test]
    fn resolves_a_quarter_circle_edge_on_a_cylinder_against_a_plane_cap() {
        // A cylinder of radius 2 along +Z through the origin. Its flat cap
        // (face 1, a Plane) is bounded by an arc on the cylinder (face 2)
        // from (2,0,0) to (0,2,0), closed by two straight radii through a
        // third, unmodeled face (id 3, no Face object - always unresolved,
        // so those two edges fall back to a plain Line via the plane side).
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let mut cyl_numbers = vec![0.0; 4];
        cyl_numbers.extend([0.0, 0.0, 0.0]); // center
        cyl_numbers.extend([1.0, 0.0, 0.0]); // x_axis
        cyl_numbers.extend([0.0, 1.0, 0.0]); // y_axis
        cyl_numbers.extend([0.0, 0.0, 1.0]); // z_axis
        cyl_numbers.push(2.0); // radius
        let cylinder = SerialObject {
            object_id: u32::MAX,
            class_index: CYL_SURF,
            offset: 1,
            bytes: 0,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers: cyl_numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
        };
        let cap_face = face_object(1, 10, reference(900, PLANE));
        // A placeholder Face for id 2 so the cylinder surface has something
        // to pair with; it declares no loop, so it is excluded on its own
        // and never asserted on.
        let cyl_face = face_object(2, 0, reference(999, CYL_SURF));
        let cap_loop = loop_object(10, 1, 200, 202);
        // The loop (id 10) is the sentinel: edge 200 is first (prev -> loop),
        // edge 202 is last (next -> loop).
        let arc_edge = {
            let mut edge = line_edge(200, 1, 2, [201, 0], [10, 0], 0, (2.0, 0.0), (0.0, 2.0));
            // Overwrite side 1 (the cylinder) with the true (u, v) sweep;
            // side 0 (the plane cap) already carries the matching (x, y).
            edge.numbers[2] = 0.0; // u at first point
            edge.numbers[3] = 0.0; // v at first point
            edge.numbers[6] = std::f64::consts::FRAC_PI_2; // u at last point
            edge.numbers[7] = 0.0; // v at last point
            edge
        };
        let radius_a = line_edge(201, 1, 3, [202, 0], [200, 0], 0, (0.0, 2.0), (0.0, 0.0));
        let radius_b = line_edge(202, 1, 3, [10, 0], [201, 0], 0, (0.0, 0.0), (2.0, 0.0));
        let objects = vec![
            plane, cylinder, cap_face, cyl_face, cap_loop, arc_edge, radius_a, radius_b,
        ];

        let brep = assemble(&objects, &classes());
        let cap = brep
            .faces
            .iter()
            .find(|face| matches!(face.surface, BrepSurface::Plane { .. }))
            .expect("the cap face resolved");
        let arc_edge = &cap.loops[0][0];
        assert!(distance(arc_edge.start, [2.0, 0.0, 0.0]) < 1.0e-9);
        assert!(distance(arc_edge.end, [0.0, 2.0, 0.0]) < 1.0e-9);
        match arc_edge.curve {
            BrepCurve::Arc(arc) => {
                assert!((arc.radius - 2.0).abs() < 1.0e-9);
                assert!((arc.start_angle - 0.0).abs() < 1.0e-9);
                assert!((arc.end_angle - std::f64::consts::FRAC_PI_2).abs() < 1.0e-9);
            }
            BrepCurve::Line => panic!("expected an arc"),
        }
        assert_eq!(cap.loops[0][1].curve, BrepCurve::Line);
        assert_eq!(cap.loops[0][2].curve, BrepCurve::Line);
    }
}
