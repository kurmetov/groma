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
/// Guard on the `m_nextLoop` chain: how many loops one face may carry. The
/// most any face in the corpus carries is far below this; it exists so a
/// chain that points back at itself terminates.
const MAX_FACE_LOOPS: usize = 256;

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
    pub cone_surf: u16,
    pub surf_rev: u16,
    pub ruled_surf: u16,
    /// The two profile curve classes read here. `SurfRev` names one object
    /// and `RuledSurf` two; 1 800 of SMALL's 1 844 `SurfRev` profiles and
    /// 214 / 276 / 330 of the corpus's 246 / 276 / 878 `RuledSurf` sides are
    /// one of these. A `GEllipse`, `GHermiteSpline` or `GNurbSpline` profile
    /// is left unread rather than approximated.
    pub g_line: u16,
    pub g_arc: u16,
}

/// One symbol's boundary representation, in the symbol's own local
/// coordinate system and Revit internal feet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SymbolBrep {
    pub faces: Vec<BrepFace>,
    /// The `Face` object identifier of each entry of `faces`, in the same
    /// order. Kept so a caller can ask the record what a resolved face
    /// declares about itself - its `GFace` fields - without re-deriving which
    /// object each face came from.
    pub face_ids: Vec<u32>,
    /// The bodies the record declares, in the order it declares them. See
    /// [`BrepBody`]: a record is not one body, and reading it as one is what
    /// made a wall's solid appear to disagree with the box on its own record.
    pub bodies: Vec<BrepBody>,
    /// Faces present in the record that could not be resolved, and why.
    /// Preserved rather than discarded, per the project's "mark unknown
    /// structure explicitly" rule.
    pub excluded_faces: Vec<BrepExclusion>,
    /// Faces the record declares that are on no boundary of it: they name no
    /// loop and no edge names them. Kept apart from [`SymbolBrep::excluded_faces`]
    /// because they are not a reading that failed - see [`assemble`].
    pub unbounded_faces: Vec<u32>,
    /// Every edge the record declares that did not resolve. A face is excluded
    /// by the *first* failing edge its loop reaches, so face exclusions
    /// undercount and cannot say how badly a reading missed; these can.
    pub failed_edges: Vec<BrepEdgeFailure>,
    /// What ordering a face's edges by their endpoints says on the faces that
    /// declare a loop, where the declared loop is the answer. See
    /// [`OrderingControl`].
    pub ordering_control: OrderingControl,
    /// What the `GEdgeLoop.m_nextLoop` chains yielded. See [`HoleTally`].
    pub holes: HoleTally,
}

/// What reading a face's further loops - its holes - produced, and what the
/// edges say about it.
///
/// A hole is read from the chain `GEdgeLoop.m_nextLoop` writes: a terminal
/// loop writes a null identifier, a face with holes writes a live reference to
/// the next one. Each link is walked by [`walk_loop`] exactly as the first
/// loop is, so a loop naming a different face, or one whose edges do not
/// close, is refused by the same checks rather than by a new rule.
///
/// `edges_accounted` is the independent check, and it is the reason to believe
/// the chain: `GEdge.m_pFace` names a face from the edge's own side, so the
/// edges that name a face are known without reading any loop at all. A face
/// whose loops use exactly those edges has had its whole boundary read; one
/// that leaves some over has a hole nobody read. That count moves from
/// `edges_short` to `edges_accounted` as holes are read, which no arrangement
/// of a wrong chain would do.
///
/// `unread` keeps the chains that stopped early. The face itself is *not*
/// excluded for one: it keeps the loops that did read, exactly as it did
/// before any hole was read at all, so nothing that resolved before stops
/// resolving now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoleTally {
    /// Faces carrying at least one further loop.
    pub faces: usize,
    /// Further loops read, over all faces.
    pub loops: usize,
    /// Faces whose loops use exactly the edges that name the face.
    pub edges_accounted: usize,
    /// The same count over the first loop alone, which is what reading no hole
    /// at all would give: the difference is what reading the chain bought.
    pub first_loop_accounted: usize,
    /// Faces some of whose edges no loop of theirs uses - an unread hole.
    pub edges_short: usize,
    /// Faces using more edge incidences than name them, which no correct
    /// reading produces and which nothing in the corpus has yet shown.
    pub edges_over: usize,
    /// Chains that stopped before their terminal loop, with the reason.
    pub unread: Vec<BrepExclusion>,
}

/// The control on [`order_face_edges`], which is a reconstruction rather than
/// a reading and so may not be used unchecked.
///
/// A face that declares a loop is ordered both ways - by the chain the file
/// declares and by joining endpoints - and the two are compared as cyclic
/// sequences. Only a face whose declared loop uses every edge naming it is
/// asked, so the answer the reconstruction has to give is one ring: closing
/// several where the file declares one is a contradiction, not a hole.
///
/// The two ways of failing are kept apart because they mean opposite things. A
/// `refused` face is one the ordering declined - an ambiguous corner, a ring
/// that did not close - and declining is what it does on a face it cannot
/// read, so it costs nothing but the face. A `contradicted` face is one where
/// it produced a ring *and the file says a different one* - a different
/// ordering, or more rings than the one declared: that is the reconstruction
/// being wrong while believing itself right, which is the only result that
/// would refuse it the licence to run at all.
///
/// `not_comparable` counts the faces the question cannot be put to: more edges
/// name the face than its loop uses, so the two orderings are over different
/// sets before either runs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OrderingControl {
    pub agreed: usize,
    pub refused: usize,
    pub contradicted: usize,
    pub not_comparable: usize,
}

/// What the bodies a record declares say about why its faces do not bound a
/// volume, best reading first.
///
/// Nothing here reads a byte: every field it consults - `BrepBody::edges`,
/// `one_sided_edges`, `open_edges`, and the loops the faces already carry -
/// is assembled before it runs. It exists because "every face resolved" and
/// "bounds a volume by its own loops" were 40 489 and 14 979 records on AR S1
/// and the 25 510 between them were one undivided number, so a record that
/// legitimately holds no solid and a solid missing a face from its shell
/// counted the same.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BrepOpenness {
    /// A body of the record closes by its own loops: every edge it draws is
    /// drawn by exactly one other face of the same body. The record fails the
    /// record-wide test only for what it carries beside that body, which is
    /// what a wall's record looks like and what the exporter already selects
    /// from by the box.
    BoundsAVolume,
    /// A body whose faces pair up curve for curve, with two of its edges
    /// running between the same pair of points - a circle written as two arcs,
    /// which is every pipe in a plumbing model. The boundary closes; what does
    /// not is the reading of it that keys an edge by its endpoints alone.
    ClosesCurveForCurve,
    /// A body whose every edge names two faces, both of them in it, whose
    /// loops still leave an edge undrawn by any second face. The topology says
    /// closed and the geometry says otherwise, so one of the two readings is
    /// wrong: a hole nobody read, or a loop using an edge that is not on this
    /// boundary.
    ClosedOnItsEdgesOnly,
    /// A body with nothing one-sided about it that names a face outside
    /// itself: the shell has a hole exactly where that face should be. This
    /// is the class where an unread face costs a solid.
    ShellWithAHole,
    /// Every body has an edge with nothing on the other side - a null side in
    /// `GEdge.m_pFace` - which is what a free surface is. A record whose best
    /// body is one of these holds no solid to export, and its not bounding a
    /// volume is the file's answer rather than a shortfall of ours.
    FreeSurface,
    /// Bodies, but no edge names a face of any of them, so there is no
    /// boundary to close.
    NoEdgeNamesItsFaces,
    /// No node of the record names a face, so it declares no body at all.
    NoBodyDeclared,
}

impl BrepOpenness {
    /// The reading in one line, for a report.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::BoundsAVolume => "a body of the record bounds a volume by its own loops",
            Self::ClosesCurveForCurve => {
                "a body pairs curve for curve, but two of its edges share both endpoints"
            }
            Self::ClosedOnItsEdgesOnly => {
                "a body closes on its edges, but its loops leave one unmatched"
            }
            Self::ShellWithAHole => "a body's shell has a hole where a face it names should be",
            Self::FreeSurface => {
                "every body is a free surface: an edge has nothing on the far side"
            }
            Self::NoEdgeNamesItsFaces => "no edge names a face of any body",
            Self::NoBodyDeclared => "the record declares no body",
        }
    }
}

impl SymbolBrep {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// Whether what this holds can be claimed to be a closed solid.
    ///
    /// Every face the record declared has to have resolved, and every body's
    /// boundary has to close on its own faces - see [`BrepBody::is_closed`].
    /// The second half is what the old "no face was excluded" test could not
    /// say: a record carrying a solid and a free surface beside it excluded
    /// nothing and was still not a closed shell.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.excluded_faces.is_empty() && self.bodies.iter().all(BrepBody::is_closed)
    }

    /// Whether these faces bound a volume by their own loops: every edge one
    /// of them draws is drawn by exactly one other.
    ///
    /// This is a geometric reading of closure and it answers a different
    /// question from [`BrepBody::is_closed`]. That one is topological - it
    /// asks whether every edge naming a face by `GEdge.m_pFace` finds its
    /// other face in the same body - so a face the record declares and this
    /// set leaves out always leaves it open, whether or not any loop here ever
    /// used that edge. What an exporter needs to know is whether the faces it
    /// is about to write close, and that is what this says.
    ///
    /// Edges are matched on their endpoints, quantised to
    /// [`CLOSURE_TOLERANCE_FEET`] and taken unordered: the two faces that
    /// share an edge traverse it in opposite directions.
    #[must_use]
    pub fn bounds_a_volume(&self) -> bool {
        faces_closure(self.faces.iter()) == FaceClosure::Closes
    }

    /// The best reading the bodies of this record support.
    ///
    /// [`SymbolBrep::bounds_a_volume`] puts its question to the record as a
    /// whole, and a record is not one body: a wall declaring its solid plus a
    /// free surface for each plane its compound structure separates on can
    /// never answer yes, however completely it was read. What an exporter
    /// takes from a record is one body - the one reproducing the box on the
    /// same record - so the question worth putting to a record that does not
    /// close is which of its bodies came closest. That is what this answers,
    /// and [`BrepOpenness`] says what each answer is evidence of.
    #[must_use]
    pub fn openness(&self) -> BrepOpenness {
        self.bodies
            .iter()
            .map(|body| self.body_openness(body))
            .min()
            .unwrap_or(BrepOpenness::NoBodyDeclared)
    }

    /// One body's reading. [`BrepOpenness`] is ordered best first, so
    /// [`SymbolBrep::openness`] is a minimum over the record's bodies.
    fn body_openness(&self, body: &BrepBody) -> BrepOpenness {
        if body.edges == 0 {
            return BrepOpenness::NoEdgeNamesItsFaces;
        }
        match faces_closure(body.faces.iter().filter_map(|index| self.faces.get(*index))) {
            FaceClosure::Closes => return BrepOpenness::BoundsAVolume,
            FaceClosure::SharedEndpoints => return BrepOpenness::ClosesCurveForCurve,
            FaceClosure::Empty | FaceClosure::Gap => {}
        }
        if body.is_closed() {
            return BrepOpenness::ClosedOnItsEdgesOnly;
        }
        if body.one_sided_edges == 0 {
            return BrepOpenness::ShellWithAHole;
        }
        BrepOpenness::FreeSurface
    }

    /// One declared body on its own.
    ///
    /// Its faces are this body's; the record-level tallies are carried over
    /// unchanged, because a failed edge or an unread hole chain is recorded
    /// against the record and cannot be attributed to one of its bodies after
    /// the fact. The faces the record could not read are *not* carried over -
    /// they belong to the record, and whether they leave this shell open is
    /// what [`BrepBody::open_edges`] says.
    #[must_use]
    pub fn body(&self, index: usize) -> Option<Self> {
        let body = self.bodies.get(index)?;
        Some(Self {
            faces: body
                .faces
                .iter()
                .filter_map(|face| self.faces.get(*face).cloned())
                .collect(),
            face_ids: body
                .faces
                .iter()
                .filter_map(|face| self.face_ids.get(*face).copied())
                .collect(),
            bodies: vec![BrepBody {
                faces: (0..body.faces.len()).collect(),
                ..body.clone()
            }],
            excluded_faces: Vec::new(),
            // Like `excluded_faces`, these belong to the record and not to one
            // of its shells: a face on no boundary is on this body's no more
            // than on any other.
            unbounded_faces: Vec::new(),
            failed_edges: self.failed_edges.clone(),
            ordering_control: self.ordering_control,
            holes: self.holes.clone(),
        })
    }
}

/// How a set of faces pairs its edges up, which is the geometric reading of
/// closure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaceClosure {
    /// No face drew an edge, so there is nothing to close.
    Empty,
    /// Every edge is drawn by exactly two faces, and no two edges run between
    /// the same pair of points.
    Closes,
    /// Every edge is drawn by exactly two faces, and two of them run between
    /// the same pair of points - which is what a circle written as two arcs
    /// looks like, and which reading the boundary by endpoints alone cannot
    /// tell from an edge drawn four times.
    SharedEndpoints,
    /// Some edge is not drawn by exactly two faces: a boundary with a gap in
    /// it, or one face drawing over another.
    Gap,
}

/// A point on the [`CLOSURE_TOLERANCE_FEET`] grid.
type ClosureKey = [i64; 3];

/// One edge as the two faces sharing it both see it: its endpoints, taken
/// unordered, and the point it passes through halfway along.
type CurveKey = (ClosureKey, ClosureKey, ClosureKey);

/// Quantise a point onto the [`CLOSURE_TOLERANCE_FEET`] grid.
fn closure_key(point: [f64; 3]) -> ClosureKey {
    point.map(|value| {
        #[allow(clippy::cast_possible_truncation)]
        {
            (value / CLOSURE_TOLERANCE_FEET).round() as i64
        }
    })
}

/// The point an edge passes through halfway along, in the same quantised
/// coordinates as its endpoints.
///
/// Two faces sharing an edge read it from the one `GEdge`, so they place this
/// point identically; two *different* edges between the same pair of points
/// place it apart. That is the whole of what it is for - endpoints alone
/// cannot tell those two cases from each other, and a pipe's circular seam is
/// written as the second one.
///
/// Orientation does not move it: reversing an edge swaps a line's endpoints,
/// swaps an arc's two angles - leaving their mean where it was - and reverses
/// a polyline, whose smallest interior point is the same set either way.
fn edge_midpoint_key(edge: &BrepEdge) -> ClosureKey {
    match &edge.curve {
        BrepCurve::Line => closure_key(scale3(add3(edge.start, edge.end), 0.5)),
        BrepCurve::Arc(arc) => {
            let middle = f64::midpoint(arc.start_angle, arc.end_angle);
            let y_axis = cross3(arc.z_axis, arc.x_axis);
            closure_key(add3(
                arc.center,
                add3(
                    scale3(arc.x_axis, arc.radius * middle.cos()),
                    scale3(y_axis, arc.radius * middle.sin()),
                ),
            ))
        }
        // The endpoints are the first and last of these, so an interior point
        // is what says which curve this is; the smallest is picked because it
        // is the one choice a reversal cannot move.
        BrepCurve::Polyline(points) => points
            .get(1..points.len().saturating_sub(1))
            .unwrap_or_default()
            .iter()
            .map(|point| closure_key(*point))
            .min()
            .unwrap_or_else(|| closure_key(scale3(add3(edge.start, edge.end), 0.5))),
    }
}

/// How a set of faces closes: every edge one of them draws has to be drawn by
/// exactly one other.
///
/// Points are quantised to [`CLOSURE_TOLERANCE_FEET`] and an edge's two
/// endpoints are taken unordered, because the two faces sharing an edge
/// traverse it in opposite directions. [`SymbolBrep::bounds_a_volume`] and the
/// per-body reading in [`SymbolBrep::openness`] both go through here, so a
/// record and one of its bodies are judged by the same test rather than by two
/// that could drift.
fn faces_closure<'a>(faces: impl Iterator<Item = &'a BrepFace>) -> FaceClosure {
    let mut curves: HashMap<CurveKey, usize> = HashMap::new();
    let mut ends: HashMap<(ClosureKey, ClosureKey), usize> = HashMap::new();
    let mut edges = 0_usize;
    for face in faces {
        for face_loop in &face.loops {
            for edge in face_loop {
                let (start, end) = (closure_key(edge.start), closure_key(edge.end));
                let pair = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                *ends.entry(pair).or_default() += 1;
                *curves
                    .entry((pair.0, pair.1, edge_midpoint_key(edge)))
                    .or_default() += 1;
                edges += 1;
            }
        }
    }
    if edges == 0 {
        return FaceClosure::Empty;
    }
    if !curves.values().all(|uses| *uses == 2) {
        return FaceClosure::Gap;
    }
    if ends.values().all(|uses| *uses == 2) {
        FaceClosure::Closes
    } else {
        FaceClosure::SharedEndpoints
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrepBody {
    /// The `GBRep` node that declares these faces. More than one body can
    /// carry the same node: a node holding two shells declares two bodies.
    pub node_id: u32,
    /// Indices into [`SymbolBrep::faces`], in the order the node names them.
    pub faces: Vec<usize>,
    /// Edges naming a face of this body.
    pub edges: usize,
    /// Of those, edges whose `GEdge.m_pFace` leaves one side null: nothing is
    /// on the other side, so the face is a free surface rather than part of a
    /// solid.
    pub one_sided_edges: usize,
    /// Of those, edges naming a second face that is not in this body - one
    /// that did not resolve. The shell has a hole where that face should be.
    pub open_edges: usize,
}

impl BrepBody {
    /// Whether this body's boundary closes: every edge two-sided, and the face
    /// on the other side of each one read and in this body.
    ///
    /// This is a stronger statement than "the record excluded no face", and a
    /// local one: it is about this shell, not about everything the record
    /// happens to carry beside it.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.edges > 0 && self.one_sided_edges == 0 && self.open_edges == 0
    }

    /// Whether this body is a solid rather than a free surface.
    #[must_use]
    pub fn is_solid(&self) -> bool {
        self.edges > 0 && self.one_sided_edges == 0
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
    /// How far apart the two adjacent faces placed a shared `EdgePnt`, in feet.
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

#[derive(Clone, Debug, PartialEq)]
pub struct BrepEdge {
    pub start: [f64; 3],
    pub end: [f64; 3],
    pub curve: BrepCurve,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BrepCurve {
    Line,
    Arc(BrepArc),
    /// A source-sampled curve. The points include both topological endpoints
    /// and every `GEdge.m_interiorEdgePnts` sample between them, in traversal
    /// order and in the symbol's local frame.
    Polyline(Vec<[f64; 3]>),
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
    /// A profile curve, held in this frame's own coordinates, swept about the
    /// frame's `z_axis`. `SurfRev` supplies the profile as another object;
    /// `ConeSurf` supplies its apex and half-angle directly. `u` turns the
    /// profile and `v` runs along it.
    Revolution {
        center: [f64; 3],
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        z_axis: [f64; 3],
        profile: BrepProfile,
    },
    /// Two profiles joined by straight rulings:
    /// `S(u, v) = (1 - v) * first(u) + v * second(u)`.
    ///
    /// Unlike every other surface here `RuledSurf` declares no frame, so both
    /// profiles are already in the body's own coordinates. `u` runs along the
    /// profiles, normalised onto each one's `m_endParams`; `v` runs across the
    /// rulings, with the first profile at `v = 0` and the second at `v = 1`.
    ///
    /// Measured on SMALL's record 50329: a face whose profiles are two arcs of
    /// radius 0.1875 and 0.14583 carries an edge holding `v = 1` whose seven
    /// `EdgePnt`s step `u` uniformly over [0, 1]; the adjacent plane places
    /// those same points on a 180-degree arc of radius 0.14583 - the second
    /// profile, swept over exactly the `[pi, 2pi]` its `m_endParams` declares.
    /// Its edges holding `u` constant carry no interior points at all, which
    /// is what a straight ruling needs.
    Ruled {
        first: BrepRuling,
        second: BrepRuling,
    },
}

/// The curve a [`BrepSurface::Revolution`] turns, in that surface's frame.
///
/// Revolved about the frame's `z`, a line coplanar with the axis gives a cone,
/// an arc whose plane holds the axis gives a torus, and one whose centre is on
/// the axis gives a sphere. Measured over SMALL's 1 844 `SurfRev` objects that
/// is every one of them: 1 040 lines, all slanted and all coplanar with the
/// axis; 728 arcs off the axis and 32 on it, all in a plane that holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrepProfile {
    /// `origin + v * direction`.
    Line {
        origin: [f64; 3],
        direction: [f64; 3],
    },
    /// `center + radius * (cos(v) * x_axis + sin(v) * y_axis)`.
    Arc {
        center: [f64; 3],
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        radius: f64,
    },
}

/// One side of a [`BrepSurface::Ruled`].
///
/// `RuledSurf` declares two profile references and two points. Measured over
/// the corpus's 246 / 276 / 878 objects, a null reference and a non-zero
/// matching point occur together and never apart: 8 / 192 / 66 sides are a
/// null reference beside a used point, and every live reference sits beside a
/// zero point. So a null reference states that the profile has collapsed to
/// that point, and both sides of the surface are seen to do it - side 1 on
/// SMALL and BIG, side 2 on MEDIUM.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrepRuling {
    /// A degenerate profile: every `u` reaches the same point.
    Point([f64; 3]),
    /// `profile` evaluated at `start + u * (end - start)`. The surface's `u`
    /// is normalised - every `RuledSurf` envelope in the corpus states a `u`
    /// range inside [0, 1] - and the interval is the curve's own
    /// `GCurve.m_endParams`.
    Curve {
        profile: BrepProfile,
        start: f64,
        end: f64,
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
#[allow(clippy::too_many_lines)] // One pass over the objects, then the bodies.
pub fn assemble(
    objects: &[SerialObject],
    classes: &BrepClassIndexes,
    body_classes: &[u16],
) -> SymbolBrep {
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

    // Which edges name each face, and on which side. `GEdge.m_pFace` is a
    // bare identifier pair, so this is the boundary read from the edges rather
    // than from the face - the only route a face with a null `m_pFirstLoop`
    // leaves open.
    let mut by_face: HashMap<u32, Vec<(u32, usize)>> = HashMap::new();
    for edge in objects
        .iter()
        .filter(|object| object.class_index == classes.edge && object.identifiers.len() == 6)
    {
        for (side, face) in edge.identifiers[..2].iter().enumerate() {
            if *face != 0 {
                by_face
                    .entry(*face)
                    .or_default()
                    .push((edge.object_id, side));
            }
        }
    }

    let mut faces = Vec::new();
    // The identifier of each entry of `faces`, so the bodies below can be
    // stated as indices into it.
    let mut face_ids = Vec::new();
    let mut excluded_faces = Vec::new();
    let mut unbounded_faces = Vec::new();
    let mut ordering_control = OrderingControl::default();
    let mut holes = HoleTally::default();
    for face in objects
        .iter()
        .filter(|object| object.class_index == classes.face)
    {
        let incidences = by_face.get(&face.object_id).map_or(&[][..], Vec::as_slice);
        // A face that names no loop and that no edge names is on no boundary
        // this record draws. `GEdge.m_pFace` names the face on each side of
        // every edge, so a face no edge names borders nothing here, and there
        // is no third place a boundary could be written. Counting it as a face
        // this reader could not read would be a statement about the reader
        // rather than about the file, and on AR S1 it was 60 886 of the 66 135
        // "excluded" faces - which is to say the measurement that decides what
        // to work on next was mostly not about faces at all.
        //
        // The file says the same thing a second way. Every one of these faces
        // has `GInfo.m_flags & 0x80000` clear - 60 886 on AR S1, 4 591 on
        // SMALL, 539 on MEDIUM, 1 350 on BIG, with no exception on any of them
        // - and that bit is already established, by a different route and for
        // a different purpose, as the mark separating a record's own faces
        // from geometry joined into its shell (`FACE_INSIDE_THE_BOX_FLAG` in
        // the CLI). Two independent readings agree they are not the solid's.
        //
        // Nothing here weakens what an exporter is told: these faces are not
        // resolved, they are only not counted as failures, and the shell they
        // are left out of still has to close on its own edges before anything
        // is written from it.
        if incidences.is_empty()
            && face
                .references
                .first()
                .is_none_or(|reference| reference.object_id == 0)
        {
            unbounded_faces.push(face.object_id);
            continue;
        }
        match assemble_face(face, classes, &by_id, &edges, &face_surfaces, incidences) {
            Ok((assembled, stopped)) => {
                if assembled.loops.len() > 1 {
                    holes.faces += 1;
                    holes.loops += assembled.loops.len() - 1;
                }
                if let Some(reason) = stopped {
                    holes.unread.push(BrepExclusion {
                        face_id: face.object_id,
                        reason,
                    });
                }
                let used = assembled.loops.iter().map(Vec::len).sum::<usize>();
                holes.first_loop_accounted +=
                    usize::from(assembled.loops.first().map(Vec::len) == Some(incidences.len()));
                match used.cmp(&incidences.len()) {
                    std::cmp::Ordering::Equal => holes.edges_accounted += 1,
                    std::cmp::Ordering::Less => holes.edges_short += 1,
                    std::cmp::Ordering::Greater => holes.edges_over += 1,
                }
                // The control: where the file declares the answer, ordering by
                // endpoints has to give the same one. Only faces whose loop
                // uses exactly the edges that name the face can be asked -
                // elsewhere the two orderings are over different sets.
                if face
                    .references
                    .first()
                    .is_some_and(|reference| reference.object_id != 0)
                {
                    let declared = &assembled.loops[0];
                    if declared.len() == incidences.len() {
                        match order_face_edges(incidences, &edges) {
                            Ok(rings) if rings.len() == 1 && rings_agree(declared, &rings[0]) => {
                                ordering_control.agreed += 1;
                            }
                            Ok(_) => ordering_control.contradicted += 1,
                            Err(_) => ordering_control.refused += 1,
                        }
                    } else {
                        ordering_control.not_comparable += 1;
                    }
                }
                faces.push(assembled);
                face_ids.push(face.object_id);
            }
            Err(reason) => excluded_faces.push(BrepExclusion {
                face_id: face.object_id,
                reason,
            }),
        }
    }
    let bodies = declared_bodies(objects, classes, body_classes, &face_ids, &edges);
    SymbolBrep {
        faces,
        face_ids,
        bodies,
        excluded_faces,
        unbounded_faces,
        failed_edges,
        ordering_control,
        holes,
    }
}

/// Union-find root of `index`, with path halving.
fn find_root(parent: &mut [usize], mut index: usize) -> usize {
    while parent[index] != index {
        parent[index] = parent[parent[index]];
        index = parent[index];
    }
    index
}

/// Split the record's faces into the bodies it declares.
///
/// Two declarations do the splitting, and neither is a guess about the
/// geometry:
///
/// - `GBRep.m_pFaces`. A `GElement` record is a node graph and `GBRep` -
///   `Geometry` is the subclass the files write - is the node that owns faces.
///   A wall record holds several: the solid, and one node per free surface
///   beside it, which is what the planes of its compound structure's layers
///   are stored as.
/// - `GEdge.m_pFace`, which names the face on each side of an edge. Faces
///   joined by an edge are one shell; a node holding two solids that touch
///   nowhere holds two shells, and the box on the record singles out one of
///   them. A wall with a sweep along it is exactly that case.
///
/// So a body here is one shell of one node. Faces that did not resolve are
/// still placed, because an edge names them whether or not their surface or
/// loop could be read.
fn declared_bodies(
    objects: &[SerialObject],
    classes: &BrepClassIndexes,
    body_classes: &[u16],
    face_ids: &[u32],
    edges: &HashMap<u32, Result<ResolvedEdge, EdgeFailure>>,
) -> Vec<BrepBody> {
    let index_of: HashMap<u32, usize> = face_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    let mut node_of: HashMap<u32, u32> = HashMap::new();
    for node in objects
        .iter()
        .filter(|object| body_classes.contains(&object.class_index))
    {
        for reference in &node.references {
            if reference.class_index == classes.face && reference.object_id != 0 {
                node_of.entry(reference.object_id).or_insert(node.object_id);
            }
        }
    }
    if node_of.is_empty() {
        return Vec::new();
    }
    // Union-find over the faces that resolved, joined by the edges naming two
    // of them. A face that did not resolve joins nothing: it has no geometry
    // to contribute and joining through it would merge two shells on the
    // strength of a face neither of them could read. What it does instead is
    // leave the shells it borders open, which `open_edges` below counts.
    let mut parent: Vec<usize> = (0..face_ids.len()).collect();
    let edge_objects = || {
        objects
            .iter()
            .filter(|object| object.class_index == classes.edge && object.identifiers.len() == 6)
    };
    for edge in edge_objects() {
        let named = &edge.identifiers[..2];
        if named.contains(&0) {
            continue;
        }
        // Only an edge that resolved joins two faces. An edge is read from
        // both of the faces it names and accepted only when the two agree on
        // the same 3D curve, so one that did not resolve is a contradiction
        // between the incidence and the geometry - exactly the case where
        // joining on it would weld a face onto a body it is nowhere near.
        if !edges.get(&edge.object_id).is_some_and(Result::is_ok) {
            continue;
        }
        let (Some(left), Some(right)) = (index_of.get(&named[0]), index_of.get(&named[1])) else {
            continue;
        };
        // Only within one node: an edge crossing two `GBRep` nodes would be a
        // contradiction between the two declarations, and joining on it would
        // silently prefer one.
        if node_of.get(&named[0]) != node_of.get(&named[1]) {
            continue;
        }
        let (left, right) = (
            find_root(&mut parent, *left),
            find_root(&mut parent, *right),
        );
        parent[left] = right;
    }

    let mut bodies: Vec<BrepBody> = Vec::new();
    let mut body_of_root: HashMap<usize, usize> = HashMap::new();
    let mut body_of_face: HashMap<u32, usize> = HashMap::new();
    for (index, face) in face_ids.iter().enumerate() {
        let root = find_root(&mut parent, index);
        let body = *body_of_root.entry(root).or_insert_with(|| {
            bodies.push(BrepBody {
                node_id: node_of.get(face).copied().unwrap_or_default(),
                faces: Vec::new(),
                edges: 0,
                one_sided_edges: 0,
                open_edges: 0,
            });
            bodies.len() - 1
        });
        bodies[body].faces.push(index);
        body_of_face.insert(*face, body);
    }
    for edge in edge_objects() {
        let named = &edge.identifiers[..2];
        let one_sided = named.contains(&0);
        let resolved = edges.get(&edge.object_id).is_some_and(Result::is_ok);
        for (side, face) in named.iter().enumerate() {
            let Some(index) = body_of_face.get(face).copied() else {
                continue;
            };
            let partner = body_of_face.get(&named[1 - side]).copied();
            let body = &mut bodies[index];
            body.edges += 1;
            if one_sided {
                body.one_sided_edges += 1;
            } else if !resolved || partner != Some(index) {
                body.open_edges += 1;
            }
        }
    }
    bodies
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
    let mut cones = objects
        .iter()
        .filter(|object| object.class_index == classes.cone_surf);
    // `SurfRev` shares the same sentinel identifier a `CylSurf` does, so it is
    // paired the same way - by encounter order - for the same reason.
    let mut revolutions = objects
        .iter()
        .filter(|object| object.class_index == classes.surf_rev);
    // `RuledSurf` is sentinel-identified too: every one of the corpus's 88 /
    // 59 / 294 records holding one quotes 0xFFFFFFFF from every face. It is
    // paired by encounter order on the same measured precondition the others
    // need - in every such record the count of faces naming a `RuledSurf`
    // equals the count of `RuledSurf` objects the record writes.
    let mut ruled = objects
        .iter()
        .filter(|object| object.class_index == classes.ruled_surf);
    let curves_by_id: HashMap<(u16, u32), &SerialObject> = objects
        .iter()
        .filter(|object| {
            object.class_index == classes.g_line || object.class_index == classes.g_arc
        })
        .map(|object| ((object.class_index, object.object_id), object))
        .collect();
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
        } else if surface_ref.class_index == classes.cone_surf {
            cones.next().and_then(cone_surface)
        } else if surface_ref.class_index == classes.surf_rev {
            revolutions
                .next()
                .and_then(|object| revolution_surface(object, &curves_by_id, classes))
        } else if surface_ref.class_index == classes.ruled_surf {
            ruled
                .next()
                .and_then(|object| ruled_surface(object, &curves_by_id, classes))
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

/// Read a `ConeSurf` as the line from its apex swept about its local `z`.
/// Its `v` is distance along the generator, so the profile direction is the
/// unit vector `(sin(half_angle), 0, cos(half_angle))`.
fn cone_surface(object: &SerialObject) -> Option<BrepSurface> {
    let n = &object.numbers;
    if n.len() < 17 {
        return None;
    }
    let half_angle = n[16];
    if !half_angle.is_finite()
        || half_angle.abs() <= f64::EPSILON
        || half_angle.abs() >= std::f64::consts::FRAC_PI_2
    {
        return None;
    }
    Some(BrepSurface::Revolution {
        center: [n[4], n[5], n[6]],
        x_axis: [n[7], n[8], n[9]],
        y_axis: [n[10], n[11], n[12]],
        z_axis: [n[13], n[14], n[15]],
        profile: BrepProfile::Line {
            origin: [0.0, 0.0, 0.0],
            direction: [half_angle.sin(), 0.0, half_angle.cos()],
        },
    })
}

/// Read a `SurfRev` and the profile curve it names.
///
/// The numbers are the parent `Surface`'s four-number envelope and then the
/// frame, exactly as `Plane` and `CylSurf` carry theirs; the profile is the one
/// object the class declares, and its own numbers start with `GCurve`'s two
/// end parameters. A profile this does not read - `GEllipse`,
/// `GHermiteSpline` - leaves the face unresolved rather than approximated.
/// Read one profile curve and the parameter interval it declares.
///
/// `GCurve.m_endParams` is the first pair of numbers on every curve, so a
/// profile is its shape together with the interval its own parameter runs
/// over. A class this does not read - `GEllipse`, `GHermiteSpline`,
/// `GNurbSpline` - answers `None` rather than being approximated.
fn profile_curve(
    class_index: u16,
    object_id: u32,
    curves: &HashMap<(u16, u32), &SerialObject>,
    classes: &BrepClassIndexes,
) -> Option<(BrepProfile, f64, f64)> {
    let curve = curves.get(&(class_index, object_id))?;
    let p = &curve.numbers;
    let profile = if class_index == classes.g_line {
        if p.len() < 8 {
            return None;
        }
        BrepProfile::Line {
            origin: [p[2], p[3], p[4]],
            direction: [p[5], p[6], p[7]],
        }
    } else if class_index == classes.g_arc {
        if p.len() < 12 {
            return None;
        }
        BrepProfile::Arc {
            x_axis: [p[2], p[3], p[4]],
            y_axis: [p[5], p[6], p[7]],
            radius: p[8],
            center: [p[9], p[10], p[11]],
        }
    } else {
        return None;
    };
    Some((profile, p[0], p[1]))
}

/// Read a `RuledSurf` and the two profiles it interpolates.
///
/// The numbers are the parent `Surface`'s four-number envelope, then
/// `m_Point1` and `m_Point2`; the two references are `m_pProfileCurve1` and
/// `m_pProfileCurve2`. Every object in the corpus carries exactly those ten
/// numbers and exactly two references. A null reference takes its side's
/// point instead; an unread curve class leaves the face unresolved.
fn ruled_surface(
    object: &SerialObject,
    curves: &HashMap<(u16, u32), &SerialObject>,
    classes: &BrepClassIndexes,
) -> Option<BrepSurface> {
    let n = &object.numbers;
    if n.len() < 10 {
        return None;
    }
    let side = |index: usize| -> Option<BrepRuling> {
        let reference = object.references.get(index)?;
        if reference.object_id == 0 {
            let start = 4 + index * 3;
            let point = n.get(start..start + 3)?;
            return Some(BrepRuling::Point([point[0], point[1], point[2]]));
        }
        let (profile, start, end) =
            profile_curve(reference.class_index, reference.object_id, curves, classes)?;
        Some(BrepRuling::Curve {
            profile,
            start,
            end,
        })
    };
    Some(BrepSurface::Ruled {
        first: side(0)?,
        second: side(1)?,
    })
}

fn revolution_surface(
    object: &SerialObject,
    curves: &HashMap<(u16, u32), &SerialObject>,
    classes: &BrepClassIndexes,
) -> Option<BrepSurface> {
    let n = &object.numbers;
    if n.len() < 16 {
        return None;
    }
    let reference = object.references.first()?;
    // `SurfRev`'s own `v` is the profile's raw parameter, so the interval the
    // curve declares is not needed here; a ruled surface normalises onto it.
    let (profile, _, _) =
        profile_curve(reference.class_index, reference.object_id, curves, classes)?;
    Some(BrepSurface::Revolution {
        center: [n[4], n[5], n[6]],
        x_axis: [n[7], n[8], n[9]],
        y_axis: [n[10], n[11], n[12]],
        z_axis: [n[13], n[14], n[15]],
        profile,
    })
}

/// A point on the profile, in the revolved surface's own frame.
fn profile_point(profile: BrepProfile, v: f64) -> [f64; 3] {
    match profile {
        BrepProfile::Line { origin, direction } => add3(origin, scale3(direction, v)),
        BrepProfile::Arc {
            center,
            x_axis,
            y_axis,
            radius,
        } => add3(
            center,
            add3(
                scale3(x_axis, radius * v.cos()),
                scale3(y_axis, radius * v.sin()),
            ),
        ),
    }
}

/// A point of one side of a ruled surface, at the surface's normalised `u`.
fn ruling_point(ruling: BrepRuling, u: f64) -> [f64; 3] {
    match ruling {
        BrepRuling::Point(point) => point,
        BrepRuling::Curve {
            profile,
            start,
            end,
        } => profile_point(profile, start + u * (end - start)),
    }
}

/// Turn a point of the frame about the frame's own `z`.
fn turn_about_z(point: [f64; 3], u: f64) -> [f64; 3] {
    let (c, s) = (u.cos(), u.sin());
    [
        point[0] * c - point[1] * s,
        point[0] * s + point[1] * c,
        point[2],
    ]
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
        BrepSurface::Revolution {
            center,
            x_axis,
            y_axis,
            z_axis,
            profile,
        } => {
            let turned = turn_about_z(profile_point(profile, v), u);
            add3(
                center,
                add3(
                    scale3(x_axis, turned[0]),
                    add3(scale3(y_axis, turned[1]), scale3(z_axis, turned[2])),
                ),
            )
        }
        BrepSurface::Ruled { first, second } => {
            let from = ruling_point(first, u);
            let to = ruling_point(second, u);
            add3(from, scale3(add3(to, scale3(from, -1.0)), v))
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

    let surfaces = pface.map(|id| face_surfaces.get(&id).copied().flatten());
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
    let opinion = |side: usize| {
        surface_opinion(
            surfaces[side],
            (first_pnt[side * 2], first_pnt[side * 2 + 1]),
            (last_pnt[side * 2], last_pnt[side * 2 + 1]),
            raw_start,
            raw_end,
        )
    };
    let analytic_curve = match (surfaces[0], surfaces[1]) {
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
                    if curves_agree(&first, &second) {
                        Ok(first)
                    } else {
                        Err("the two cylinders disagree on the edge's curve".into())
                    }
                }
                // Asymmetry is a contradiction, not a partial success: a curve
                // that is a cross-section or a ruling of one cylinder is one of
                // the other too. Taking the side that answered would be reading
                // through a disagreement.
                (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
                    Err("only one of the two cylinders gives the edge a curve".into())
                }
                (Err(reason), Err(_)) => Err(reason.into()),
            }
        }
        // Any other pair of curved faces is held to the same oracle, for the
        // same reason: a curve lying on both is named by both.
        _ => match (opinion(0), opinion(1)) {
            (Some(Ok(first)), Some(Ok(second))) => {
                if curves_agree(&first, &second) {
                    Ok(first)
                } else {
                    Err("the two curved faces disagree on the edge's curve".into())
                }
            }
            (Some(Ok(_)), Some(Err(_))) | (Some(Err(_)), Some(Ok(_))) => {
                Err("only one of the two curved faces gives the edge a curve".into())
            }
            (Some(Err(reason)), Some(Err(_))) => Err(reason.into()),
            (Some(only), None) | (None, Some(only)) => only.map_err(EdgeFailure::from),
            (None, None) => straight_line(raw_start, raw_end).map_err(EdgeFailure::from),
        },
    };
    // Exact analytic curves remain exact. When the adjacent surfaces cannot
    // classify an edge as a line or circle, the file's interior EdgePnt array
    // is the path: evaluate every stored UV pair and preserve the samples as a
    // polyline instead of replacing the unknown curve with its chord.
    let raw_curve = analytic_curve.or_else(|analytic_failure| {
        sampled_edge_curve(
            &edge.numbers[..edge.numbers.len() - 8],
            &surfaces,
            raw_start,
            raw_end,
        )
        .unwrap_or(Err(analytic_failure))
    })?;

    Ok(ResolvedEdge {
        pface,
        links,
        raw_start,
        raw_end,
        raw_curve,
        flags,
    })
}

/// What one adjacent face's surface says this edge's curve is.
///
/// A face whose surface curves says on its own what the edge is; a plane does
/// not, and neither does a face this cannot read. `None` is "this side has no
/// opinion", which is why such a side falls through to a line.
fn surface_opinion(
    surface: Option<BrepSurface>,
    first: (f64, f64),
    last: (f64, f64),
    raw_start: [f64; 3],
    raw_end: [f64; 3],
) -> Option<Result<BrepCurve, &'static str>> {
    match surface {
        Some(surface @ BrepSurface::Cylinder { .. }) => Some(classify_cylinder_edge(
            surface, first, last, raw_start, raw_end,
        )),
        Some(surface @ BrepSurface::Revolution { .. }) => Some(classify_revolution_edge(
            surface, first, last, raw_start, raw_end,
        )),
        Some(surface @ BrepSurface::Ruled { .. }) => Some(classify_ruled_edge(
            surface, first, last, raw_start, raw_end,
        )),
        Some(BrepSurface::Plane { .. }) | None => None,
    }
}

/// Evaluate the `m_interiorEdgePnts` array preceding the two endpoint
/// `EdgePnt`s. Each item is `(u0, v0, u1, v1)`, one UV pair for each adjacent
/// face. When both surfaces are readable they must place every sample at the
/// same 3D point, just as they must for the endpoints.
fn sampled_edge_curve(
    interior_numbers: &[f64],
    surfaces: &[Option<BrepSurface>; 2],
    raw_start: [f64; 3],
    raw_end: [f64; 3],
) -> Option<Result<BrepCurve, EdgeFailure>> {
    if interior_numbers.is_empty() {
        return None;
    }
    if interior_numbers.len() % 4 != 0 {
        return Some(Err("edge has a partial interior EdgePnt".into()));
    }

    let mut points = Vec::with_capacity(interior_numbers.len() / 4 + 2);
    points.push(raw_start);
    for pnt in interior_numbers.chunks_exact(4) {
        let point = agree(
            surfaces[0].map(|surface| eval_uv(surface, pnt[0], pnt[1])),
            surfaces[1].map(|surface| eval_uv(surface, pnt[2], pnt[3])),
        );
        match point {
            Ok(point) if point.into_iter().all(f64::is_finite) => points.push(point),
            Ok(_) => {
                return Some(Err(
                    "interior EdgePnt evaluates to a non-finite point".into()
                ));
            }
            Err(failure) => return Some(Err(failure)),
        }
    }
    points.push(raw_end);
    Some(Ok(BrepCurve::Polyline(points)))
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

/// What curve an edge of a revolved face is, from that face's own `(u, v)`.
///
/// Only two families lie on a surface of revolution and are named by a single
/// parameter being constant: an edge at one `u` is the profile itself, turned
/// there, and an edge at one `v` is the circle that point traces about the
/// axis. Anything else - a seam where a revolved face meets another solid - is
/// refused rather than approximated by its endpoints, exactly as the
/// cylinders refuse theirs.
///
/// The arc's `z_axis` is `cross(x, y)` of the turned frame rather than the
/// frame's own `z`: 248 of SMALL's 1 844 frames are left-handed, and taking
/// the declared `z` there would trace the arc the wrong way round.
fn classify_revolution_edge(
    surface: BrepSurface,
    (u_start, v_start): (f64, f64),
    (u_end, v_end): (f64, f64),
    raw_start: [f64; 3],
    raw_end: [f64; 3],
) -> Result<BrepCurve, &'static str> {
    let BrepSurface::Revolution {
        center,
        x_axis,
        y_axis,
        z_axis,
        profile,
    } = surface
    else {
        unreachable!("caller passed a Revolution surface");
    };
    let direction = |local: [f64; 3]| {
        add3(
            scale3(x_axis, local[0]),
            add3(scale3(y_axis, local[1]), scale3(z_axis, local[2])),
        )
    };
    let point = |local: [f64; 3]| add3(center, direction(local));
    let turn = (u_end - u_start).abs();
    let along = (v_end - v_start).abs();
    let arc = if turn <= PARAMETER_TOLERANCE && along > PARAMETER_TOLERANCE {
        // One `u`: the profile curve, turned to where this edge sits.
        match profile {
            BrepProfile::Line { .. } => return straight_line(raw_start, raw_end),
            BrepProfile::Arc {
                center: profile_center,
                x_axis: profile_x,
                y_axis: profile_y,
                radius,
            } => {
                let turned_x = direction(turn_about_z(profile_x, u_start));
                let turned_y = direction(turn_about_z(profile_y, u_start));
                BrepArc {
                    center: point(turn_about_z(profile_center, u_start)),
                    x_axis: turned_x,
                    z_axis: cross3(turned_x, turned_y),
                    radius,
                    start_angle: v_start,
                    end_angle: v_end,
                }
            }
        }
    } else if along <= PARAMETER_TOLERANCE && turn > PARAMETER_TOLERANCE {
        // One `v`: the circle that one point of the profile traces.
        if turn > std::f64::consts::TAU + PARAMETER_TOLERANCE {
            return Err("revolved edge angular span exceeds a full turn");
        }
        let local = profile_point(profile, v_start.midpoint(v_end));
        let radius = local[0].hypot(local[1]);
        if radius <= CLOSURE_TOLERANCE_FEET {
            return Err("revolved edge lies on the axis of revolution");
        }
        let phase = local[1].atan2(local[0]);
        BrepArc {
            center: point([0.0, 0.0, local[2]]),
            x_axis,
            z_axis: cross3(x_axis, y_axis),
            radius,
            start_angle: phase + u_start,
            end_angle: phase + u_end,
        }
    } else {
        return Err(
            "revolved edge parametrization is neither a constant-u profile nor a constant-v circle",
        );
    };
    if distance(arc_point(arc, arc.start_angle), raw_start) > CLOSURE_TOLERANCE_FEET
        || distance(arc_point(arc, arc.end_angle), raw_end) > CLOSURE_TOLERANCE_FEET
    {
        return Err("revolved arc reconstruction disagreed with the evaluated endpoints");
    }
    Ok(BrepCurve::Arc(arc))
}

/// Read an edge of a ruled surface from that surface's own parameters.
///
/// A ruled surface answers for exactly two families of edge. One `u` is a
/// ruling, and a ruling is straight on any ruled surface whatever its
/// profiles are. One `v` at either end of the ruling is a profile itself -
/// the first at `v = 0`, the second at `v = 1` - and is that profile's own
/// shape. One `v` between them is the affine blend of the two profiles: still
/// straight when both profiles are straight, but in general a curve this does
/// not name, and it is refused rather than flattened to its chord. The
/// refusal is what hands the edge to the sampled `EdgePnt` path.
fn classify_ruled_edge(
    surface: BrepSurface,
    (u_start, v_start): (f64, f64),
    (u_end, v_end): (f64, f64),
    raw_start: [f64; 3],
    raw_end: [f64; 3],
) -> Result<BrepCurve, &'static str> {
    let BrepSurface::Ruled { first, second } = surface else {
        unreachable!("caller passed a Ruled surface");
    };
    let along = (u_end - u_start).abs();
    let across = (v_end - v_start).abs();
    if along <= PARAMETER_TOLERANCE && across > PARAMETER_TOLERANCE {
        return straight_line(raw_start, raw_end);
    }
    if across > PARAMETER_TOLERANCE || along <= PARAMETER_TOLERANCE {
        return Err(
            "ruled edge parametrization is neither a constant-u ruling nor a constant-v profile",
        );
    }
    let straight = |ruling: BrepRuling| {
        matches!(
            ruling,
            BrepRuling::Point(_)
                | BrepRuling::Curve {
                    profile: BrepProfile::Line { .. },
                    ..
                }
        )
    };
    if straight(first) && straight(second) {
        // Every blend of two straight profiles is straight, so this holds at
        // any `v`, not only at the two ends.
        return straight_line(raw_start, raw_end);
    }
    let v = v_start.midpoint(v_end);
    let ruling = if v.abs() <= PARAMETER_TOLERANCE {
        first
    } else if (v - 1.0).abs() <= PARAMETER_TOLERANCE {
        second
    } else {
        return Err("ruled edge lies between the two profiles");
    };
    let (profile, start, end) = match ruling {
        BrepRuling::Point(_) => return straight_line(raw_start, raw_end),
        BrepRuling::Curve {
            profile,
            start,
            end,
        } => (profile, start, end),
    };
    let BrepProfile::Arc {
        center,
        x_axis,
        y_axis,
        radius,
    } = profile
    else {
        return straight_line(raw_start, raw_end);
    };
    let parameter = |u: f64| start + u * (end - start);
    let arc = BrepArc {
        center,
        x_axis,
        z_axis: cross3(x_axis, y_axis),
        radius,
        start_angle: parameter(u_start),
        end_angle: parameter(u_end),
    };
    if distance(arc_point(arc, arc.start_angle), raw_start) > CLOSURE_TOLERANCE_FEET
        || distance(arc_point(arc, arc.end_angle), raw_end) > CLOSURE_TOLERANCE_FEET
    {
        return Err("ruled profile arc reconstruction disagreed with the evaluated endpoints");
    }
    Ok(BrepCurve::Arc(arc))
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
fn curves_agree(first: &BrepCurve, second: &BrepCurve) -> bool {
    match (first, second) {
        (BrepCurve::Line, BrepCurve::Line) => true,
        (BrepCurve::Arc(first), BrepCurve::Arc(second)) => {
            (first.radius - second.radius).abs() <= CLOSURE_TOLERANCE_FEET
                && distance(first.center, second.center) <= CLOSURE_TOLERANCE_FEET
                && distance(
                    arc_point(*first, first.start_angle.midpoint(first.end_angle)),
                    arc_point(*second, second.start_angle.midpoint(second.end_angle)),
                ) <= CLOSURE_TOLERANCE_FEET
        }
        (BrepCurve::Line, BrepCurve::Arc(_))
        | (BrepCurve::Arc(_), BrepCurve::Line)
        | (BrepCurve::Polyline(_), _)
        | (_, BrepCurve::Polyline(_)) => false,
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
    incidences: &[(u32, usize)],
) -> Result<(BrepFace, Option<&'static str>), &'static str> {
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
        // No loop object is written for this face, and none can be: the walk
        // queues a node only for a full reference, and that null was the only
        // one to its loop. Measured on four corpus files, no `EdgeLoop`
        // elsewhere claims such a face and the adjoining edges' own `m_next`
        // is null on that side, so the file offers nothing to read here.
        // What is left is the edges that name the face, ordered by their
        // endpoints - a reconstruction, and held to the checks in
        // [`order_face_edges`]. A face without a loop object has no
        // `m_nextLoop` chain either, so its holes are read the same way its
        // outer bound is: as the further rings its own edges close into.
        //
        // A face with no edges either never arrives here: [`assemble`] takes
        // it out first, because it is on no boundary rather than on one this
        // could not order.
        return order_face_edges(incidences, edges).map(|rings| {
            (
                BrepFace {
                    surface,
                    loops: rings,
                },
                None,
            )
        });
    };
    let loop_object = *by_id
        .get(&first_loop.object_id)
        .ok_or("referenced loop is missing")?;
    if loop_object.class_index != classes.edge_loop {
        return Err("loop reference does not name an EdgeLoop");
    }
    let loop_edges = walk_loop(face.object_id, loop_object, by_id, classes, edges)?;
    let mut loops = vec![loop_edges];
    // `GEdgeLoop.m_nextLoop` is the face's next boundary loop - its holes.
    // The sentinel is an ordinary null reference; the `(8, class 0)` that loop
    // 162 of record 278446 appeared to carry there was that null read two
    // bytes early, under a `GInfo.m_flags` width that has since been measured
    // and corrected. A terminal loop writes the null and stops.
    //
    // Each link is walked exactly as the first loop is, so its `m_pFace` must
    // name this same face and its edges must close - see [`walk_loop`]. A
    // chain that stops early keeps the loops already read and reports why, so
    // a face that resolved before one was read still resolves.
    let mut current = loop_object;
    let mut visited = vec![loop_object.object_id];
    let mut stopped = None;
    while let Some(next) = current
        .references
        .first()
        .filter(|reference| reference.object_id != 0)
    {
        if loops.len() >= MAX_FACE_LOOPS {
            stopped = Some("face's loop chain exceeded the loop guard");
            break;
        }
        if visited.contains(&next.object_id) {
            stopped = Some("face's loop chain returned to a loop it had read");
            break;
        }
        let Some(next_object) = by_id.get(&next.object_id).copied() else {
            stopped = Some("next loop in the chain is missing");
            break;
        };
        if next_object.class_index != classes.edge_loop {
            stopped = Some("next loop in the chain does not name an EdgeLoop");
            break;
        }
        match walk_loop(face.object_id, next_object, by_id, classes, edges) {
            Ok(ring) => loops.push(ring),
            Err(reason) => {
                stopped = Some(reason);
                break;
            }
        }
        visited.push(next_object.object_id);
        current = next_object;
    }
    Ok((BrepFace { surface, loops }, stopped))
}

/// Order the edges that name a face into its closed rings by their endpoints.
///
/// This is a reconstruction rather than a reading, so it is held to more than
/// closure. Each step must have exactly one unused edge starting where the
/// last one ended, and an ambiguous junction refuses the face rather than
/// picking; every edge naming the face must be used; and every ring must
/// close. An edge's direction is not guessed at either: it is the same
/// `(m_flags & 1 != 0) != (side == 1)` the declared loops are read with.
///
/// A face's edges need not form a single ring: a face with a hole closes its
/// outer bound and leaves the hole's edges over, exactly as a face that
/// declares a loop leaves them to its `m_nextLoop` chain. So the walk closes a
/// ring where it runs out of continuations and starts the next one from the
/// lowest edge it has not used, rather than refusing the face. Nothing about
/// the first ring changes: a face whose edges do form one is walked, checked
/// and closed exactly as before, which is why the control below still applies
/// to it.
///
/// The check that this is allowed at all is [`OrderingControl`]: on the faces
/// that do declare a loop, this must reproduce it - one ring, the same one.
fn order_face_edges(
    incidences: &[(u32, usize)],
    edges: &HashMap<u32, Result<ResolvedEdge, EdgeFailure>>,
) -> Result<Vec<BrepLoop>, &'static str> {
    if incidences.is_empty() {
        return Err("face has no first loop");
    }
    if incidences.len() > MAX_LOOP_EDGES {
        return Err("face names more edges than the loop guard allows");
    }
    let mut oriented = Vec::with_capacity(incidences.len());
    for &(edge_id, side) in incidences {
        let resolved = edges
            .get(&edge_id)
            .ok_or("loop edge is not an Edge object")?
            .as_ref()
            .map_err(|failure| failure.reason)?;
        oriented.push(oriented_edge(resolved, side));
    }

    let mut used = vec![false; oriented.len()];
    let mut rings: Vec<BrepLoop> = Vec::new();
    while let Some(start) = used.iter().position(|&used| !used) {
        used[start] = true;
        let mut ring = vec![oriented[start].clone()];
        loop {
            let end = ring[ring.len() - 1].end;
            let mut next = None;
            for (index, edge) in oriented.iter().enumerate() {
                if used[index] || distance(edge.start, end) > CLOSURE_TOLERANCE_FEET {
                    continue;
                }
                if next.is_some() {
                    return Err("a face's edges meet ambiguously at one of its corners");
                }
                next = Some(index);
            }
            let Some(index) = next else { break };
            used[index] = true;
            ring.push(oriented[index].clone());
        }
        if distance(ring[0].start, ring[ring.len() - 1].end) > CLOSURE_TOLERANCE_FEET {
            // Which of the two this is says what is wrong with the face. A
            // chain that ran out of edges with none left over is a closed set
            // of edges that does not describe a ring; one that broke off with
            // edges still unused has a boundary with a hole in it - and not a
            // tolerance's worth: measured over MEDIUM's 1 610 such faces the
            // gap is 28.6 mm at the median and never under 0.1 mm, against a
            // join tolerance of 3 um.
            return Err(if used.iter().all(|&used| used) {
                "a face's ordered edges do not close in 3D"
            } else {
                "a face's edges break off before closing a ring"
            });
        }
        rings.push(ring);
    }
    let outer = outer_ring(&rings);
    rings.swap(0, outer);
    Ok(rings)
}

/// Which of a reconstructed face's rings is its outer bound, which the file
/// does not say and the loops of a face that declares one answer by position.
///
/// A hole lies inside the bound it perforates, so its corners span a smaller
/// box - measured on the corners rather than the curves because that is what
/// a ring carries, and understating an arc's bulge cannot make a hole span
/// more than what encloses it. Ties keep the ring the walk closed first.
fn outer_ring(rings: &[BrepLoop]) -> usize {
    let mut widest = (0, f64::NEG_INFINITY);
    for (index, ring) in rings.iter().enumerate() {
        let mut low = [f64::INFINITY; 3];
        let mut high = [f64::NEG_INFINITY; 3];
        for edge in ring {
            for axis in 0..3 {
                low[axis] = low[axis].min(edge.start[axis]);
                high[axis] = high[axis].max(edge.start[axis]);
            }
        }
        let span = distance(low, high);
        if span > widest.1 {
            widest = (index, span);
        }
    }
    widest.0
}

/// Whether two orderings of one face's edges describe the same ring. They are
/// cyclic sequences, so a shared starting edge is looked for first and the
/// rest compared from there.
fn rings_agree(declared: &BrepLoop, ordered: &BrepLoop) -> bool {
    if declared.len() != ordered.len() {
        return false;
    }
    let same = |left: &BrepEdge, right: &BrepEdge| {
        distance(left.start, right.start) <= CLOSURE_TOLERANCE_FEET
            && distance(left.end, right.end) <= CLOSURE_TOLERANCE_FEET
    };
    let Some(offset) = ordered.iter().position(|edge| same(edge, &declared[0])) else {
        return false;
    };
    declared
        .iter()
        .enumerate()
        .all(|(index, edge)| same(edge, &ordered[(offset + index) % ordered.len()]))
}

/// One edge as the face on `side` of it sees it: the file stores the curve in
/// its own `first`/`last` order, and `m_flags`' low bit flips that for side 0.
fn oriented_edge(resolved: &ResolvedEdge, side: usize) -> BrepEdge {
    if (resolved.flags & 1 != 0) ^ (side == 1) {
        BrepEdge {
            start: resolved.raw_end,
            end: resolved.raw_start,
            curve: reverse_curve(&resolved.raw_curve),
        }
    } else {
        BrepEdge {
            start: resolved.raw_start,
            end: resolved.raw_end,
            curve: resolved.raw_curve.clone(),
        }
    }
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
        let edge = oriented_edge(resolved, side);
        if let Some(previous_end) = previous_end {
            if distance(previous_end, edge.start) > CLOSURE_TOLERANCE_FEET {
                return Err("consecutive edges do not share an endpoint");
            }
        }
        previous_end = Some(edge.end);
        ordered.push(edge);

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

fn reverse_curve(curve: &BrepCurve) -> BrepCurve {
    match curve {
        BrepCurve::Line => BrepCurve::Line,
        BrepCurve::Arc(arc) => BrepCurve::Arc(BrepArc {
            start_angle: arc.end_angle,
            end_angle: arc.start_angle,
            ..*arc
        }),
        BrepCurve::Polyline(points) => {
            let mut points = points.clone();
            points.reverse();
            BrepCurve::Polyline(points)
        }
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
    const CONE_SURF: u16 = 815;
    const SURF_REV: u16 = 3986;
    const RULED_SURF: u16 = 3587;
    const G_LINE: u16 = 1798;
    const G_ARC: u16 = 2040;

    fn classes() -> BrepClassIndexes {
        BrepClassIndexes {
            face: FACE,
            edge_loop: EDGE_LOOP,
            edge: EDGE,
            plane: PLANE,
            cyl_surf: CYL_SURF,
            cone_surf: CONE_SURF,
            surf_rev: SURF_REV,
            ruled_surf: RULED_SURF,
            g_line: G_LINE,
            g_arc: G_ARC,
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
            alternate_integers: Vec::new(),
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
            alternate_integers: Vec::new(),
        }
    }

    /// A `ConeSurf` whose apex is `center`, axis is +Z and whose `v`
    /// parameter measures distance along its generator.
    fn cone_object(center: [f64; 3], half_angle: f64) -> SerialObject {
        let mut numbers = vec![0.0; 4];
        numbers.extend(center);
        numbers.extend([1.0, 0.0, 0.0]); // x_axis
        numbers.extend([0.0, 1.0, 0.0]); // y_axis
        numbers.extend([0.0, 0.0, 1.0]); // z_axis
        numbers.push(half_angle);
        SerialObject {
            object_id: u32::MAX,
            class_index: CONE_SURF,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
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
            alternate_integers: Vec::new(),
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
            alternate_integers: Vec::new(),
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
            alternate_integers: Vec::new(),
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
            alternate_integers: Vec::new(),
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
        let brep = assemble(&split_tube(None), &classes(), &[]);
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
            match &edges[index].curve {
                BrepCurve::Arc(arc) => assert!((arc.radius - 2.0).abs() < 1.0e-9),
                BrepCurve::Line | BrepCurve::Polyline(_) => {
                    panic!("edge {index} should be an arc")
                }
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
        let brep = assemble(&split_tube(Some([(0.0, 5.0), (-pi, 5.0)])), &classes(), &[]);
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
    fn reads_a_diagonal_cylinder_edge_from_its_interior_edge_points() {
        let mut edge = line_edge(
            300,
            1,
            2,
            [10, 0],
            [10, 0],
            0,
            (0.0, 0.0),
            (std::f64::consts::FRAC_PI_2, 2.0),
        );
        // `m_interiorEdgePnts` precedes `m_firstAndLastEdgePnts`; side 1 is
        // zero here because this fixture intentionally resolves only side 0.
        edge.numbers.splice(
            0..0,
            [
                std::f64::consts::FRAC_PI_6,
                0.5,
                0.0,
                0.0,
                std::f64::consts::FRAC_PI_3,
                1.5,
                0.0,
                0.0,
            ],
        );
        let mut surfaces = HashMap::new();
        surfaces.insert(
            1,
            Some(BrepSurface::Cylinder {
                center: [0.0, 0.0, 0.0],
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                radius: 2.0,
            }),
        );

        let resolved = resolve_edge(&edge, &surfaces).expect("the sampled edge resolves");
        let BrepCurve::Polyline(points) = resolved.raw_curve else {
            panic!("a diagonal cylinder edge should preserve its sampled path")
        };
        assert_eq!(points.len(), 4);
        assert!(distance(points[0], [2.0, 0.0, 0.0]) < 1.0e-9);
        assert!(distance(points[1], [3.0_f64.sqrt(), 1.0, 0.5]) < 1.0e-9);
        assert!(distance(points[2], [1.0, 3.0_f64.sqrt(), 1.5]) < 1.0e-9);
        assert!(distance(points[3], [0.0, 2.0, 2.0]) < 1.0e-9);

        let reversed = reverse_curve(&BrepCurve::Polyline(points.clone()));
        let BrepCurve::Polyline(reversed) = reversed else {
            unreachable!()
        };
        assert_eq!(reversed, points.into_iter().rev().collect::<Vec<_>>());
    }

    #[test]
    fn keeps_an_exact_cylinder_arc_when_interior_samples_are_present() {
        let mut edge = line_edge(
            300,
            1,
            2,
            [10, 0],
            [10, 0],
            0,
            (0.0, 3.0),
            (std::f64::consts::FRAC_PI_2, 3.0),
        );
        edge.numbers
            .splice(0..0, [std::f64::consts::FRAC_PI_4, 3.0, 0.0, 0.0]);
        let mut surfaces = HashMap::new();
        surfaces.insert(
            1,
            Some(BrepSurface::Cylinder {
                center: [0.0, 0.0, 0.0],
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                radius: 2.0,
            }),
        );

        let resolved = resolve_edge(&edge, &surfaces).expect("the analytic arc resolves");
        assert!(matches!(resolved.raw_curve, BrepCurve::Arc(_)));
    }

    /// `loop_object` with a live `m_nextLoop`: the face carries another loop.
    fn chained_loop_object(
        id: u32,
        face: u32,
        first_edge: u32,
        last_edge: u32,
        next_loop: u32,
    ) -> SerialObject {
        let mut object = loop_object(id, face, first_edge, last_edge);
        object.references = vec![reference(next_loop, EDGE_LOOP)];
        object
    }

    /// A unit square in the XY plane with a smaller square hole in it: the
    /// outer loop's `m_nextLoop` names the hole's loop, and both name face 1.
    const GBREP: u16 = 2160;

    /// A `GBRep` node naming the faces it owns, as `Geometry.m_pFaces` does.
    fn body_node(id: u32, faces: &[u32]) -> SerialObject {
        SerialObject {
            object_id: id,
            class_index: GBREP,
            offset: 0,
            bytes: 0,
            references: faces.iter().map(|face| reference(*face, FACE)).collect(),
            identifiers: Vec::new(),
            numbers: Vec::new(),
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
        }
    }

    /// One square face whose every edge names nothing on the other side: a
    /// free surface, which is what a wall's layer planes are stored as.
    fn free_square(face: u32, plane: u32, first_edge: u32) -> Vec<SerialObject> {
        let corners = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        let mut objects = vec![
            plane_object(plane, [0.0, 0.0, 5.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            face_object(face, first_edge + 90, reference(plane, PLANE)),
            loop_object(first_edge + 90, face, first_edge, first_edge + 3),
        ];
        for (index, corner) in corners.into_iter().enumerate() {
            let next = if index == 3 {
                first_edge + 90
            } else {
                first_edge + u32::try_from(index).unwrap() + 1
            };
            let previous = if index == 0 {
                first_edge + 90
            } else {
                first_edge + u32::try_from(index).unwrap() - 1
            };
            objects.push(line_edge(
                first_edge + u32::try_from(index).unwrap(),
                face,
                0,
                [next, 0],
                [previous, 0],
                0,
                corner,
                corners[(index + 1) % 4],
            ));
        }
        objects
    }

    #[test]
    fn splits_a_record_into_the_bodies_its_nodes_and_edges_declare() {
        // One node holding a square, and a second holding a free surface -
        // the shape of every wall record in the corpus.
        let mut objects = square_with_a_hole(0);
        objects.extend(free_square(3, 901, 200));
        objects.push(body_node(500, &[1]));
        objects.push(body_node(501, &[3]));
        let brep = assemble(&objects, &classes(), &[GBREP]);
        assert_eq!(brep.faces.len(), 2);
        assert_eq!(brep.bodies.len(), 2);

        let square = &brep.bodies[0];
        assert_eq!(square.node_id, 500);
        assert_eq!(square.faces, [0]);
        // Its edges name a second face the record does not hold, so it is a
        // solid whose boundary does not close.
        assert!(square.is_solid());
        assert!(!square.is_closed());
        assert_eq!(square.one_sided_edges, 0);
        assert_eq!(square.open_edges, 8);

        let free = &brep.bodies[1];
        assert_eq!(free.node_id, 501);
        assert_eq!(free.faces, [1]);
        assert!(!free.is_solid());
        assert_eq!(free.one_sided_edges, 4);

        // One body on its own carries only its own faces.
        let only = brep.body(1).unwrap();
        assert_eq!(only.faces.len(), 1);
        assert_eq!(only.faces[0], brep.faces[1]);
        assert!(!only.is_closed());
    }

    fn square_with_a_hole(next_loop_of_the_hole: u32) -> Vec<SerialObject> {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 10, reference(900, PLANE));
        vec![
            plane,
            face,
            chained_loop_object(10, 1, 100, 103, 11),
            chained_loop_object(11, 1, 110, 113, next_loop_of_the_hole),
            line_edge(100, 1, 2, [101, 0], [10, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(101, 1, 2, [102, 0], [100, 0], 0, (1.0, 0.0), (1.0, 1.0)),
            line_edge(102, 1, 2, [103, 0], [101, 0], 0, (1.0, 1.0), (0.0, 1.0)),
            line_edge(103, 1, 2, [10, 0], [102, 0], 0, (0.0, 1.0), (0.0, 0.0)),
            // The hole, traversed the other way round, as a real inner bound
            // is: (0.25,0.25) -> (0.25,0.75) -> (0.75,0.75) -> (0.75,0.25).
            line_edge(110, 1, 2, [111, 0], [11, 0], 0, (0.25, 0.25), (0.25, 0.75)),
            line_edge(111, 1, 2, [112, 0], [110, 0], 0, (0.25, 0.75), (0.75, 0.75)),
            line_edge(112, 1, 2, [113, 0], [111, 0], 0, (0.75, 0.75), (0.75, 0.25)),
            line_edge(113, 1, 2, [11, 0], [112, 0], 0, (0.75, 0.25), (0.25, 0.25)),
        ]
    }

    #[test]
    fn reads_a_face_hole_from_the_next_loop_chain() {
        let brep = assemble(&square_with_a_hole(0), &classes(), &[]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        assert_eq!(brep.faces.len(), 1);
        let face = &brep.faces[0];
        assert_eq!(face.loops.len(), 2, "the hole is a second loop");
        assert_eq!(face.loops[1].len(), 4);
        assert!(distance(face.loops[1][0].start, [0.25, 0.25, 0.0]) < 1.0e-9);
        assert_eq!(brep.holes.faces, 1);
        assert_eq!(brep.holes.loops, 1);
        assert!(brep.holes.unread.is_empty(), "{:?}", brep.holes.unread);
        // Both loops together use exactly the edges that name the face, which
        // is the check that does not come from the chain: reading the outer
        // loop alone leaves four edges over.
        assert_eq!(brep.holes.edges_accounted, 1);
        assert_eq!(brep.holes.first_loop_accounted, 0);
        assert_eq!(brep.holes.edges_short, 0);
    }

    #[test]
    fn stops_a_loop_chain_that_returns_to_a_loop_it_has_read() {
        // The hole names the outer loop as its own next, which is a cycle. The
        // face keeps both loops it did read rather than being excluded.
        let brep = assemble(&square_with_a_hole(10), &classes(), &[]);
        assert_eq!(brep.faces.len(), 1);
        assert_eq!(brep.faces[0].loops.len(), 2);
        assert_eq!(
            brep.holes.unread,
            vec![BrepExclusion {
                face_id: 1,
                reason: "face's loop chain returned to a loop it had read",
            }]
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

        let brep = assemble(&objects, &classes(), &[]);
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

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        let start = brep.faces[0].loops[0][0].start;
        assert!(distance(start, [0.0, 0.0, 0.0]) < 1.0e-9);
    }

    /// The reconstruction for a face whose `m_pFirstLoop` is null: the same
    /// square, with the loop object removed and the face's reference nulled,
    /// so nothing but the four edges' endpoints says what the boundary is.
    #[test]
    fn orders_a_loopless_face_from_the_edges_that_name_it() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 0, reference(900, PLANE));
        // The same four edges as the declared-loop fixture, in an order that
        // is not the ring's, and with their chain links pointing nowhere.
        let edges = vec![
            line_edge(102, 1, 2, [0, 0], [0, 0], 0, (1.0, 1.0), (0.0, 1.0)),
            line_edge(100, 1, 2, [0, 0], [0, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(103, 1, 2, [0, 0], [0, 0], 0, (0.0, 1.0), (0.0, 0.0)),
            line_edge(101, 1, 2, [0, 0], [0, 0], 0, (1.0, 0.0), (1.0, 1.0)),
        ];
        let mut objects = vec![plane, face];
        objects.extend(edges);

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        let ring = &brep.faces[0].loops[0];
        assert_eq!(ring.len(), 4);
        for (index, edge) in ring.iter().enumerate() {
            let next = &ring[(index + 1) % ring.len()];
            assert!(
                distance(edge.end, next.start) < 1.0e-9,
                "the ring is not consecutive at {index}"
            );
        }
    }

    /// A face without a loop object can still have a hole: its edges close
    /// into two rings rather than one, and refusing the face for that would
    /// throw away a boundary the edges fully describe. The hole is written
    /// first here, so the outer bound only reaches `loops[0]` by being the
    /// wider of the two.
    #[test]
    fn closes_a_loopless_face_with_a_hole_into_two_rings() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 0, reference(900, PLANE));
        let edges = vec![
            line_edge(200, 1, 2, [0, 0], [0, 0], 0, (1.0, 1.0), (3.0, 1.0)),
            line_edge(201, 1, 2, [0, 0], [0, 0], 0, (3.0, 1.0), (3.0, 3.0)),
            line_edge(202, 1, 2, [0, 0], [0, 0], 0, (3.0, 3.0), (1.0, 3.0)),
            line_edge(203, 1, 2, [0, 0], [0, 0], 0, (1.0, 3.0), (1.0, 1.0)),
            line_edge(100, 1, 2, [0, 0], [0, 0], 0, (0.0, 0.0), (4.0, 0.0)),
            line_edge(101, 1, 2, [0, 0], [0, 0], 0, (4.0, 0.0), (4.0, 4.0)),
            line_edge(102, 1, 2, [0, 0], [0, 0], 0, (4.0, 4.0), (0.0, 4.0)),
            line_edge(103, 1, 2, [0, 0], [0, 0], 0, (0.0, 4.0), (0.0, 0.0)),
        ];
        let mut objects = vec![plane, face];
        objects.extend(edges);

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        let loops = &brep.faces[0].loops;
        assert_eq!(loops.len(), 2);
        assert_eq!((loops[0].len(), loops[1].len()), (4, 4));
        assert!(
            distance(loops[0][0].start, [0.0, 0.0, 0.0]) < 1.0e-9,
            "the outer bound is not first: {:?}",
            loops[0][0].start
        );
        assert!(distance(loops[1][0].start, [1.0, 1.0, 0.0]) < 1.0e-9);
        // The independent check: every edge naming the face is now used by one
        // of its rings, which reading only the first would not have done.
        assert_eq!(
            brep.holes,
            HoleTally {
                faces: 1,
                loops: 1,
                edges_accounted: 1,
                first_loop_accounted: 0,
                edges_short: 0,
                edges_over: 0,
                unread: Vec::new(),
            }
        );
    }

    /// One edge short, the reconstruction refuses rather than shipping an open
    /// face - and says so under its own reason, not the loop's.
    #[test]
    fn refuses_a_loopless_face_whose_edges_do_not_close() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 0, reference(900, PLANE));
        let edges = vec![
            line_edge(100, 1, 2, [0, 0], [0, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(101, 1, 2, [0, 0], [0, 0], 0, (1.0, 0.0), (1.0, 1.0)),
        ];
        let mut objects = vec![plane, face];
        objects.extend(edges);

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.faces.is_empty());
        assert_eq!(
            brep.excluded_faces[0].reason,
            "a face's ordered edges do not close in 3D"
        );
    }

    /// The other way the reconstruction refuses, and the one the corpus is
    /// mostly made of: the chain breaks off with edges still unused. That is a
    /// boundary with a gap in it, not a closed set of edges in the wrong
    /// order, so it is refused under its own reason - the square here is
    /// perfectly good and still does not save the face.
    #[test]
    fn refuses_a_loopless_face_whose_edges_break_off_with_others_left() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 0, reference(900, PLANE));
        let edges = vec![
            line_edge(100, 1, 2, [0, 0], [0, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(101, 1, 2, [0, 0], [0, 0], 0, (1.0, 0.0), (1.0, 1.0)),
            line_edge(200, 1, 2, [0, 0], [0, 0], 0, (5.0, 5.0), (7.0, 5.0)),
            line_edge(201, 1, 2, [0, 0], [0, 0], 0, (7.0, 5.0), (7.0, 7.0)),
            line_edge(202, 1, 2, [0, 0], [0, 0], 0, (7.0, 7.0), (5.0, 7.0)),
            line_edge(203, 1, 2, [0, 0], [0, 0], 0, (5.0, 7.0), (5.0, 5.0)),
        ];
        let mut objects = vec![plane, face];
        objects.extend(edges);

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.faces.is_empty());
        assert_eq!(
            brep.excluded_faces[0].reason,
            "a face's edges break off before closing a ring"
        );
    }

    /// A face that names no loop and that no edge names is on no boundary the
    /// record draws, so it is not a face this reader failed to read. It is
    /// reported as what it is and does not make the record incomplete.
    #[test]
    fn a_face_no_loop_and_no_edge_names_is_not_an_exclusion() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 0, reference(900, PLANE));
        let brep = assemble(&[plane, face], &classes(), &[]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        assert_eq!(brep.unbounded_faces, vec![1]);
        assert!(brep.faces.is_empty());
    }

    /// And it does not buy the record its closure: a shell that is still open
    /// on its own edges stays open, so nothing reaches an exporter that could
    /// not before.
    #[test]
    fn a_face_on_no_boundary_does_not_close_the_record_it_sits_in() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let square = face_object(1, 10, reference(900, PLANE));
        let square_loop = loop_object(10, 1, 100, 103);
        // A second face nothing bounds: no loop of its own, and the edges
        // above name faces 1 and 3, never this one.
        let stray = face_object(2, 0, reference(900, PLANE));
        let mut objects = vec![plane, square, square_loop, stray];
        objects.extend([
            line_edge(100, 1, 3, [101, 0], [10, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(101, 1, 3, [102, 0], [100, 0], 0, (1.0, 0.0), (1.0, 1.0)),
            line_edge(102, 1, 3, [103, 0], [101, 0], 0, (1.0, 1.0), (0.0, 1.0)),
            line_edge(103, 1, 3, [10, 0], [102, 0], 0, (0.0, 1.0), (0.0, 0.0)),
        ]);

        let brep = assemble(&objects, &classes(), &[]);
        assert_eq!(brep.unbounded_faces, vec![2]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        // One square is a boundary with one side and no volume: the record is
        // not a closed solid and saying so is the whole point of the gate.
        assert!(!brep.bounds_a_volume());
    }

    /// A face whose one loop runs the given points and closes back to the
    /// first, on a plane nothing here evaluates.
    fn loop_face(points: &[[f64; 3]]) -> BrepFace {
        BrepFace {
            surface: BrepSurface::Plane {
                origin: [0.0, 0.0, 0.0],
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            },
            loops: vec![
                points
                    .iter()
                    .enumerate()
                    .map(|(index, start)| BrepEdge {
                        start: *start,
                        end: points[(index + 1) % points.len()],
                        curve: BrepCurve::Line,
                    })
                    .collect(),
            ],
        }
    }

    fn body(faces: &[usize], edges: usize, one_sided: usize, open: usize) -> BrepBody {
        BrepBody {
            node_id: 500,
            faces: faces.to_vec(),
            edges,
            one_sided_edges: one_sided,
            open_edges: open,
        }
    }

    /// The reading `SymbolBrep::openness` gives is of the record's *best*
    /// body, because that is the one an exporter would take. A wall's record -
    /// a shell that closes, plus a free surface beside it - does not bound a
    /// volume as a whole and is still not a shortfall of the reader's.
    #[test]
    fn openness_reads_the_best_body_a_record_declares() {
        let square = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        // Two faces on the same four edges: every edge drawn exactly twice,
        // which is the whole of what closure asks of the loops.
        let closed = SymbolBrep {
            faces: vec![loop_face(&square), loop_face(&square)],
            face_ids: vec![1, 2],
            bodies: vec![body(&[0, 1], 4, 0, 0)],
            ..SymbolBrep::default()
        };
        assert!(closed.bounds_a_volume());
        assert_eq!(closed.openness(), BrepOpenness::BoundsAVolume);

        let with_a_free_surface = SymbolBrep {
            faces: vec![loop_face(&square), loop_face(&square), loop_face(&square)],
            face_ids: vec![1, 2, 3],
            bodies: vec![body(&[0, 1], 4, 0, 0), body(&[2], 4, 4, 0)],
            ..SymbolBrep::default()
        };
        // The record does not close - the free surface's edges are drawn once
        // - and the body an exporter would take still does.
        assert!(!with_a_free_surface.bounds_a_volume());
        assert_eq!(with_a_free_surface.openness(), BrepOpenness::BoundsAVolume);
    }

    /// The four readings that are not a solid, each from the counters that
    /// distinguish it. These are the classes the corpus breakdown is for: a
    /// free surface is the file's own answer, a shell with a hole is ours.
    #[test]
    fn openness_separates_a_free_surface_from_a_shell_with_a_hole() {
        let square = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let one_face = |bodies: Vec<BrepBody>| SymbolBrep {
            faces: vec![loop_face(&square)],
            face_ids: vec![1],
            bodies,
            ..SymbolBrep::default()
        };

        // Every edge with nothing on the far side: a free surface, and no
        // amount of further reading turns it into a solid.
        assert_eq!(
            one_face(vec![body(&[0], 4, 4, 0)]).openness(),
            BrepOpenness::FreeSurface
        );
        // Two-sided everywhere, but the face on the other side of each edge is
        // not in this body: the shell has a hole where that face should be.
        assert_eq!(
            one_face(vec![body(&[0], 4, 0, 4)]).openness(),
            BrepOpenness::ShellWithAHole
        );
        // Two-sided and every partner in the body - the topology says closed -
        // while the loops draw each edge once. The two readings disagree.
        assert_eq!(
            one_face(vec![body(&[0], 4, 0, 0)]).openness(),
            BrepOpenness::ClosedOnItsEdgesOnly
        );
        assert_eq!(
            one_face(vec![body(&[0], 0, 0, 0)]).openness(),
            BrepOpenness::NoEdgeNamesItsFaces
        );
        assert_eq!(
            one_face(Vec::new()).openness(),
            BrepOpenness::NoBodyDeclared
        );
    }

    /// The control the reconstruction is allowed on: a face that declares a
    /// loop is ordered both ways and the two agree.
    #[test]
    fn ordering_by_endpoints_reproduces_a_declared_loop() {
        let plane = plane_object(900, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let face = face_object(1, 10, reference(900, PLANE));
        let corner_loop = loop_object(10, 1, 100, 103);
        let edges = vec![
            line_edge(100, 1, 2, [101, 0], [10, 0], 0, (0.0, 0.0), (1.0, 0.0)),
            line_edge(101, 1, 2, [102, 0], [100, 0], 0, (1.0, 0.0), (1.0, 1.0)),
            line_edge(102, 1, 2, [103, 0], [101, 0], 0, (1.0, 1.0), (0.0, 1.0)),
            line_edge(103, 1, 2, [10, 0], [102, 0], 0, (0.0, 1.0), (0.0, 0.0)),
        ];
        let mut objects = vec![plane, face, corner_loop];
        objects.extend(edges);

        let brep = assemble(&objects, &classes(), &[]);
        assert_eq!(
            brep.ordering_control,
            OrderingControl {
                agreed: 1,
                ..OrderingControl::default()
            }
        );
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

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.faces.is_empty());
        assert_eq!(brep.excluded_faces.len(), 1);
        assert_eq!(brep.excluded_faces[0].face_id, 1);
    }

    #[test]
    fn excludes_a_face_with_no_supported_surface() {
        // A class this does not read at all - not Plane, CylSurf or SurfRev.
        let face = face_object(1, 10, reference(5, 9999));
        let brep = assemble(&[face], &classes(), &[]);
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
            alternate_integers: Vec::new(),
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

        let brep = assemble(&objects, &classes(), &[]);
        let cap = brep
            .faces
            .iter()
            .find(|face| matches!(face.surface, BrepSurface::Plane { .. }))
            .expect("the cap face resolved");
        let arc_edge = &cap.loops[0][0];
        assert!(distance(arc_edge.start, [2.0, 0.0, 0.0]) < 1.0e-9);
        assert!(distance(arc_edge.end, [0.0, 2.0, 0.0]) < 1.0e-9);
        match &arc_edge.curve {
            BrepCurve::Arc(arc) => {
                assert!((arc.radius - 2.0).abs() < 1.0e-9);
                assert!((arc.start_angle - 0.0).abs() < 1.0e-9);
                assert!((arc.end_angle - std::f64::consts::FRAC_PI_2).abs() < 1.0e-9);
            }
            BrepCurve::Line | BrepCurve::Polyline(_) => panic!("expected an arc"),
        }
        assert_eq!(cap.loops[0][1].curve, BrepCurve::Line);
        assert_eq!(cap.loops[0][2].curve, BrepCurve::Line);
    }

    /// A `SurfRev` whose frame is the identity at the origin, revolving the
    /// profile the reference names. Its identifier is a sentinel in the file,
    /// so - like `CylSurf` - it is paired by encounter order and the id the
    /// face quotes does not matter.
    fn revolution_object(profile: GElementNodeReference) -> SerialObject {
        let mut numbers = vec![0.0; 4];
        numbers.extend([0.0, 0.0, 0.0]); // center
        numbers.extend([1.0, 0.0, 0.0]); // x_axis
        numbers.extend([0.0, 1.0, 0.0]); // y_axis
        numbers.extend([0.0, 0.0, 1.0]); // z_axis
        SerialObject {
            object_id: u32::MAX,
            class_index: SURF_REV,
            offset: 1,
            bytes: 0,
            references: vec![profile],
            identifiers: Vec::new(),
            numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
        }
    }

    /// One edge of a fixture face: its `(u, v)` at the first point and at the
    /// last.
    type RuledCorner = ((f64, f64), (f64, f64));

    /// A `RuledSurf`: the parent `Surface`'s envelope, then `m_Point1` and
    /// `m_Point2`, with the two profile references in declaration order. Its
    /// identifier is the same sentinel every face quotes, so - like `CylSurf`
    /// and `SurfRev` - it is paired by encounter order.
    fn ruled_object(
        first: GElementNodeReference,
        second: GElementNodeReference,
        point1: [f64; 3],
        point2: [f64; 3],
    ) -> SerialObject {
        let mut numbers = vec![0.0, 0.0, 1.0, 1.0]; // m_Envelope: u, v over [0, 1]
        numbers.extend(point1);
        numbers.extend(point2);
        SerialObject {
            object_id: u32::MAX,
            class_index: RULED_SURF,
            offset: 1,
            bytes: 0,
            references: vec![first, second],
            identifiers: Vec::new(),
            numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
        }
    }

    /// A `GArc` whose `m_endParams` state the interval a ruled surface's `u`
    /// is normalised onto, rather than the zeroes `g_arc_object` leaves.
    fn g_arc_over(id: u32, center: [f64; 3], radius: f64, start: f64, end: f64) -> SerialObject {
        let mut arc = g_arc_object(id, center, [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], radius);
        arc.numbers[0] = start;
        arc.numbers[1] = end;
        arc
    }

    /// One face of a ruled surface, bounded by the `(u, v)` pairs given. As
    /// with `revolved_face`, side 1 of every edge is a face with no `Face`
    /// object, so only the ruled side has an opinion.
    fn ruled_face(surface: SerialObject, corners: &[RuledCorner]) -> Vec<SerialObject> {
        let count = u32::try_from(corners.len()).expect("a fixture face has few edges");
        let last_edge = 200 + count - 1;
        let mut objects = vec![
            surface,
            face_object(1, 10, reference(999, RULED_SURF)),
            loop_object(10, 1, 200, last_edge),
        ];
        for (index, (first, last)) in (0u32..).zip(corners.iter().copied()) {
            let id = 200 + index;
            let next = if id == last_edge { 10 } else { id + 1 };
            let previous = if index == 0 { 10 } else { id - 1 };
            objects.push(line_edge(
                id,
                1,
                2,
                [next, 0],
                [previous, 0],
                0,
                first,
                last,
            ));
        }
        objects
    }

    /// `GLine`: two `GCurve` end parameters, then the origin and direction.
    fn g_line_object(id: u32, origin: [f64; 3], direction: [f64; 3]) -> SerialObject {
        let mut numbers = vec![0.0; 2];
        numbers.extend(origin);
        numbers.extend(direction);
        SerialObject {
            object_id: id,
            class_index: G_LINE,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
        }
    }

    /// `GArc`: the two end parameters, then the frame, the radius and last the
    /// centre - the order `GArc` declares, not `GLine`'s.
    fn g_arc_object(
        id: u32,
        center: [f64; 3],
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        radius: f64,
    ) -> SerialObject {
        let mut numbers = vec![0.0; 2];
        numbers.extend(x_axis);
        numbers.extend(y_axis);
        numbers.push(radius);
        numbers.extend(center);
        SerialObject {
            object_id: id,
            class_index: G_ARC,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers,
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
        }
    }

    /// One face of a surface of revolution, bounded by the four edges the
    /// `(u, v)` pairs describe. Side 1 of every edge is face 2, which has no
    /// `Face` object, so only the revolved side has an opinion.
    fn revolved_face(
        surface: SerialObject,
        corners: [((f64, f64), (f64, f64)); 4],
    ) -> Vec<SerialObject> {
        let surface_class = surface.class_index;
        let mut objects = vec![
            surface,
            face_object(1, 10, reference(999, surface_class)),
            loop_object(10, 1, 200, 203),
        ];
        for (index, (first, last)) in (0u32..).zip(corners) {
            let id = 200 + index;
            let next = if index == 3 { 10 } else { id + 1 };
            let previous = if index == 0 { 10 } else { id - 1 };
            objects.push(line_edge(
                id,
                1,
                2,
                [next, 0],
                [previous, 0],
                0,
                first,
                last,
            ));
        }
        objects
    }

    /// A cone: the line `[1,0,0] + v * [1,0,1]` turned about +Z, so the radius
    /// at height `v` is `1 + v`. The quarter from `u = 0` to `u = pi/2` and
    /// `v = 0` to `v = 1` is bounded by two circles and two slant rulings.
    fn quarter_cone() -> Vec<SerialObject> {
        let half = std::f64::consts::FRAC_PI_2;
        let mut objects = revolved_face(
            revolution_object(reference(700, G_LINE)),
            [
                ((0.0, 0.0), (half, 0.0)),  // v = 0: the small circle.
                ((half, 0.0), (half, 1.0)), // u = pi/2: a ruling.
                ((half, 1.0), (0.0, 1.0)),  // v = 1: the large circle.
                ((0.0, 1.0), (0.0, 0.0)),   // u = 0: back down the other ruling.
            ],
        );
        objects.push(g_line_object(700, [1.0, 0.0, 0.0], [1.0, 0.0, 1.0]));
        objects
    }

    #[test]
    fn reads_a_cone_from_a_line_revolved_about_the_axis() {
        let brep = assemble(&quarter_cone(), &classes(), &[]);
        let cone = brep.faces.first().expect("the cone face resolved");
        assert!(matches!(
            cone.surface,
            BrepSurface::Revolution {
                profile: BrepProfile::Line { .. },
                ..
            }
        ));
        let edges = &cone.loops[0];
        assert_eq!(edges.len(), 4);

        // Each circle is centred on the axis at its own height, with the
        // radius the profile reaches there.
        for (index, height, radius) in [(0, 0.0, 1.0), (2, 1.0, 2.0)] {
            match &edges[index].curve {
                BrepCurve::Arc(arc) => {
                    assert!(distance(arc.center, [0.0, 0.0, height]) < 1.0e-9);
                    assert!((arc.radius - radius).abs() < 1.0e-9);
                    assert!(distance(arc.z_axis, [0.0, 0.0, 1.0]) < 1.0e-9);
                }
                BrepCurve::Line | BrepCurve::Polyline(_) => {
                    panic!("edge {index} should be a circle of the cone")
                }
            }
        }
        // A revolved line's own profile is straight, wherever it is turned to.
        assert_eq!(edges[1].curve, BrepCurve::Line);
        assert_eq!(edges[3].curve, BrepCurve::Line);
        assert!(distance(edges[1].start, [0.0, 1.0, 0.0]) < 1.0e-9);
        assert!(distance(edges[1].end, [0.0, 2.0, 1.0]) < 1.0e-9);
    }

    #[test]
    fn reads_a_cone_declared_as_a_cone_surface() {
        let half_angle = 0.5_f64.atan();
        let half_turn = std::f64::consts::FRAC_PI_2;
        let objects = revolved_face(
            cone_object([1.0, 2.0, 3.0], half_angle),
            [
                ((0.0, 2.0), (half_turn, 2.0)),
                ((half_turn, 2.0), (half_turn, 4.0)),
                ((half_turn, 4.0), (0.0, 4.0)),
                ((0.0, 4.0), (0.0, 2.0)),
            ],
        );
        let brep = assemble(&objects, &classes(), &[]);
        let cone = brep.faces.first().expect("the ConeSurf face resolved");
        let BrepSurface::Revolution {
            center, profile, ..
        } = cone.surface
        else {
            panic!("ConeSurf should use the common revolution representation")
        };
        assert!(distance(center, [1.0, 2.0, 3.0]) < 1.0e-9);
        let BrepProfile::Line { origin, direction } = profile else {
            unreachable!()
        };
        assert!(distance(origin, [0.0, 0.0, 0.0]) < 1.0e-9);
        assert!((direction[0] - 1.0 / 5.0_f64.sqrt()).abs() < 1.0e-9);
        assert!((direction[2] - 2.0 / 5.0_f64.sqrt()).abs() < 1.0e-9);

        let edges = &cone.loops[0];
        for (index, v) in [(0, 2.0), (2, 4.0)] {
            let BrepCurve::Arc(arc) = &edges[index].curve else {
                panic!("constant-v ConeSurf edge should be a circle")
            };
            assert!((arc.radius - v / 5.0_f64.sqrt()).abs() < 1.0e-9);
            assert!(distance(arc.center, [1.0, 2.0, 3.0 + 2.0 * v / 5.0_f64.sqrt()]) < 1.0e-9);
        }
        assert_eq!(edges[1].curve, BrepCurve::Line);
        assert_eq!(edges[3].curve, BrepCurve::Line);
    }

    /// A torus: the circle of radius 1/2 about `[2,0,0]` in the plane that
    /// holds the axis, turned about +Z. Major radius 2, minor 1/2.
    fn quarter_torus() -> Vec<SerialObject> {
        let half = std::f64::consts::FRAC_PI_2;
        let mut objects = revolved_face(
            revolution_object(reference(700, G_ARC)),
            [
                ((0.0, 0.0), (0.0, half)),   // u = 0: the profile itself.
                ((0.0, half), (half, half)), // v = pi/2: the circle at the top.
                ((half, half), (half, 0.0)), // u = pi/2: the profile, turned.
                ((half, 0.0), (0.0, 0.0)),   // v = 0: the outer equator.
            ],
        );
        objects.push(g_arc_object(
            700,
            [2.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            0.5,
        ));
        objects
    }

    #[test]
    fn reads_a_torus_from_an_arc_revolved_about_the_axis() {
        let brep = assemble(&quarter_torus(), &classes(), &[]);
        let torus = brep.faces.first().expect("the torus face resolved");
        let edges = &torus.loops[0];
        assert_eq!(edges.len(), 4);

        // The constant-u edges are the profile circle, turned to where they
        // sit: minor radius, centred a major radius off the axis.
        match &edges[0].curve {
            BrepCurve::Arc(arc) => {
                assert!(distance(arc.center, [2.0, 0.0, 0.0]) < 1.0e-9);
                assert!((arc.radius - 0.5).abs() < 1.0e-9);
            }
            BrepCurve::Line | BrepCurve::Polyline(_) => {
                panic!("the profile edge should be an arc")
            }
        }
        match &edges[2].curve {
            BrepCurve::Arc(arc) => {
                assert!(distance(arc.center, [0.0, 2.0, 0.0]) < 1.0e-9);
                assert!((arc.radius - 0.5).abs() < 1.0e-9);
            }
            BrepCurve::Line | BrepCurve::Polyline(_) => {
                panic!("the turned profile edge should be an arc")
            }
        }
        // The constant-v edges are the circles those points trace: on the
        // axis, at the radius and height the profile reaches.
        match &edges[1].curve {
            BrepCurve::Arc(arc) => {
                assert!(distance(arc.center, [0.0, 0.0, 0.5]) < 1.0e-9);
                assert!((arc.radius - 2.0).abs() < 1.0e-9);
            }
            BrepCurve::Line | BrepCurve::Polyline(_) => {
                panic!("the top edge should be a circle")
            }
        }
        match &edges[3].curve {
            BrepCurve::Arc(arc) => {
                assert!(distance(arc.center, [0.0, 0.0, 0.0]) < 1.0e-9);
                assert!((arc.radius - 2.5).abs() < 1.0e-9);
            }
            BrepCurve::Line | BrepCurve::Polyline(_) => {
                panic!("the equator edge should be a circle")
            }
        }
    }

    #[test]
    fn refuses_a_revolved_edge_that_turns_and_climbs_at_once() {
        // The same cone, but the first edge runs diagonally in (u, v): it is
        // neither the profile nor a circle, and nothing in the surface names
        // what curve it is. Reading it off its endpoints would invent one.
        let half = std::f64::consts::FRAC_PI_2;
        let mut objects = quarter_cone();
        let diagonal = objects
            .iter_mut()
            .find(|object| object.object_id == 200)
            .expect("the v = 0 edge");
        diagonal.numbers[4] = half; // u at the last point
        diagonal.numbers[5] = 1.0; // v at the last point

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.faces.is_empty(), "{:?}", brep.faces);
        assert_eq!(
            brep.excluded_faces[0].reason,
            "revolved edge parametrization is neither a constant-u profile nor a constant-v circle"
        );
    }

    #[test]
    fn leaves_a_revolved_face_whose_profile_it_cannot_read_unresolved() {
        // A `GEllipse` profile - anything but `GLine` or `GArc` - is left
        // alone rather than approximated, and the face falls out with the
        // reason a face with no surface at all gets.
        let mut objects = quarter_cone();
        objects.retain(|object| object.class_index != G_LINE);
        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.faces.is_empty());
        assert_eq!(
            brep.excluded_faces[0].reason,
            "face has no supported surface"
        );
    }

    /// `u` runs along both profiles, normalised onto each one's own
    /// `m_endParams`, and `v` runs across the rulings with the first profile
    /// at `v = 0` and the second at `v = 1`. A constant-`v` edge at either end
    /// is that profile; a constant-`u` edge is a straight ruling.
    #[test]
    fn a_ruled_surface_interpolates_between_its_two_profiles() {
        let quarter = std::f64::consts::FRAC_PI_2;
        let inner = g_arc_over(301, [0.0, 0.0, 0.0], 1.0, 0.0, quarter);
        let outer = g_arc_over(302, [0.0, 0.0, 1.0], 2.0, 0.0, quarter);
        let surface = ruled_object(
            reference(301, G_ARC),
            reference(302, G_ARC),
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        );
        let mut objects = ruled_face(
            surface,
            &[
                ((0.0, 0.0), (1.0, 0.0)), // the first profile
                ((1.0, 0.0), (1.0, 1.0)), // a ruling
                ((1.0, 1.0), (0.0, 1.0)), // the second profile, reversed
                ((0.0, 1.0), (0.0, 0.0)), // the other ruling
            ],
        );
        objects.push(inner);
        objects.push(outer);

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        let face = brep.faces.first().expect("the ruled face resolved");
        let BrepSurface::Ruled { first, second } = face.surface else {
            panic!("expected a ruled surface")
        };
        assert_eq!(
            first,
            BrepRuling::Curve {
                profile: BrepProfile::Arc {
                    center: [0.0, 0.0, 0.0],
                    x_axis: [1.0, 0.0, 0.0],
                    y_axis: [0.0, 1.0, 0.0],
                    radius: 1.0,
                },
                start: 0.0,
                end: quarter,
            }
        );
        assert!(matches!(second, BrepRuling::Curve { .. }));

        let edges = &face.loops[0];
        assert_eq!(edges.len(), 4);
        // The profile edges keep their own radii, which is what says `u` was
        // normalised onto each curve's interval rather than taken raw.
        match &edges[0].curve {
            BrepCurve::Arc(arc) => {
                assert!((arc.radius - 1.0).abs() < 1.0e-9);
                assert!((arc.start_angle - 0.0).abs() < 1.0e-9);
                assert!((arc.end_angle - quarter).abs() < 1.0e-9);
            }
            other => panic!("the v = 0 edge should be the first profile, got {other:?}"),
        }
        match &edges[2].curve {
            BrepCurve::Arc(arc) => {
                assert!((arc.radius - 2.0).abs() < 1.0e-9);
                assert!((arc.start_angle - quarter).abs() < 1.0e-9);
                assert!((arc.end_angle - 0.0).abs() < 1.0e-9);
            }
            other => panic!("the v = 1 edge should be the second profile, got {other:?}"),
        }
        assert_eq!(edges[1].curve, BrepCurve::Line);
        assert_eq!(edges[3].curve, BrepCurve::Line);
        assert!(distance(edges[0].start, [1.0, 0.0, 0.0]) < 1.0e-9);
        assert!(distance(edges[1].end, [0.0, 2.0, 1.0]) < 1.0e-9);
    }

    /// A null profile reference states that the profile has collapsed to the
    /// matching point, which is the only shape the corpus ever shows for a
    /// null reference. The surface is then a cone over the live profile.
    #[test]
    fn a_null_profile_reference_collapses_to_its_point() {
        let quarter = std::f64::consts::FRAC_PI_2;
        let apex = [0.0, 0.0, 2.0];
        let rim = g_arc_over(301, [0.0, 0.0, 0.0], 1.0, 0.0, quarter);
        let surface = ruled_object(
            reference(0, 0),
            reference(301, G_ARC),
            apex,
            [0.0, 0.0, 0.0],
        );
        let mut objects = ruled_face(
            surface,
            &[
                ((0.0, 1.0), (1.0, 1.0)), // the live profile
                ((1.0, 1.0), (1.0, 0.0)), // a ruling into the apex
                ((0.0, 0.0), (0.0, 1.0)), // and back out of it
            ],
        );
        objects.push(rim);

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.excluded_faces.is_empty(), "{:?}", brep.excluded_faces);
        let face = brep.faces.first().expect("the ruled face resolved");
        let BrepSurface::Ruled { first, second } = face.surface else {
            panic!("expected a ruled surface")
        };
        assert_eq!(first, BrepRuling::Point(apex));
        assert!(matches!(second, BrepRuling::Curve { .. }));

        let edges = &face.loops[0];
        assert_eq!(edges.len(), 3);
        match &edges[0].curve {
            BrepCurve::Arc(arc) => assert!((arc.radius - 1.0).abs() < 1.0e-9),
            other => panic!("the v = 1 edge should be the live profile, got {other:?}"),
        }
        assert_eq!(edges[1].curve, BrepCurve::Line);
        assert!(distance(edges[1].end, apex) < 1.0e-9);
    }

    /// A profile class this does not read - `GEllipse`, `GHermiteSpline`,
    /// `GNurbSpline` - leaves the face unresolved instead of approximated.
    #[test]
    fn an_unread_profile_class_leaves_the_ruled_face_unresolved() {
        const G_HERMITE_SPLINE: u16 = 2086;
        let surface = ruled_object(
            reference(301, G_HERMITE_SPLINE),
            reference(302, G_HERMITE_SPLINE),
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        );
        let objects = ruled_face(
            surface,
            &[
                ((0.0, 0.0), (1.0, 0.0)),
                ((1.0, 0.0), (1.0, 1.0)),
                ((1.0, 1.0), (0.0, 1.0)),
                ((0.0, 1.0), (0.0, 0.0)),
            ],
        );

        let brep = assemble(&objects, &classes(), &[]);
        assert!(brep.faces.is_empty());
        assert_eq!(
            brep.excluded_faces
                .iter()
                .map(|exclusion| exclusion.reason)
                .collect::<Vec<_>>(),
            vec!["face has no supported surface"]
        );
    }
}
