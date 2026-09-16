//! Recognising the solids that are prisms: one planar profile, swept along a
//! straight line.
//!
//! A boundary representation states every face of a solid. This exporter
//! writes them all, where Revit's own export writes a wall, a floor or a
//! column as a profile and a depth and lets the reader build the rest. That
//! difference is most of what is left of the size gap between the two files.
//!
//! Nothing here simplifies a solid. A shell is written as an
//! `IfcExtrudedAreaSolid` only where the shell **is** that sweep, and four
//! tests establish it:
//!
//! 1. every face lies in a plane, and the shell **closes** - every edge is
//!    shared by exactly two faces;
//! 2. exactly two faces stand square to the direction, facing each other;
//! 3. every other face stands parallel to it;
//! 4. those two faces bound the same region.
//!
//! Together they say the solid is the prism. A line along the direction, drawn
//! through the region, crosses the boundary only where the boundary is not
//! parallel to it - by (3) only at the two caps of (2); by (1) it crosses each
//! of them once, and by (4) it meets both wherever it meets either. So the
//! solid is exactly what lies between them, which is the sweep. Nothing weaker
//! will do: without (1) a box with a wall missing reads as a box.
//!
//! What a recognised solid loses is the *partition* of its boundary into
//! faces: two coplanar faces the source stated apart become one side of the
//! profile. No attribute of this export hangs off a face, so what goes is the
//! source's bookkeeping rather than anything the file said.
//!
//! What arrives here is already polygonal: a caller with a cylindrical face or
//! an arc edge has nothing to hand this and keeps its boundary representation.
//! That leaves the rounded solids - a pipe fitting, a wall with a curved end -
//! on the long path, and they are counted so the next step can be measured
//! rather than guessed.

use std::collections::HashMap;

/// Two directions nearer than this are one direction, and a plane within this
/// of a vertex holds it.
///
/// A tenth of a micrometre: two orders below the `1e-5` metre precision the
/// exported file's own `IfcGeometricRepresentationContext` declares, and well
/// above the arithmetic noise of the matrix chains that place a body, which
/// runs in the last bits of a double. A shell that is a prism to a tenth of a
/// micrometre is a prism; one that misses by more is stating something, and is
/// written as the boundary representation it is.
pub const TOLERANCE: f64 = 1.0e-7;

/// One boundary loop of a face, as the polygon it closes: consecutive points
/// joined by straight lines, the last joined back to the first, and the first
/// not repeated at the end.
pub type Polygon = Vec<[f64; 3]>;

/// One face: its outer boundary first, then one polygon per hole.
pub type Face = Vec<Polygon>;

/// A solid recognised as the sweep of a planar profile along a straight line.
///
/// The frame is the profile's own: `origin` is a point of the profile plane,
/// `x_axis` and `cross(direction, x_axis)` span it, and the sweep runs from
/// that plane along `direction` for `depth`. All of it is in whatever
/// coordinates the caller handed in, which for this exporter is the element's
/// local system.
#[derive(Clone, Debug, PartialEq)]
pub struct Prism {
    pub origin: [f64; 3],
    pub x_axis: [f64; 3],
    pub direction: [f64; 3],
    pub depth: f64,
    /// The profile's outer boundary in the plane's own coordinates, wound
    /// counter-clockwise about `direction`.
    pub outer: Vec<[f64; 2]>,
    /// Its holes, wound the other way. IFC states this of
    /// `IfcArbitraryProfileDefWithVoids` as an informal proposition, and a
    /// reader that takes the winding for the sign of the area needs it true.
    pub voids: Vec<Vec<[f64; 2]>>,
}

/// Why a shell was left as a boundary representation.
///
/// One variant per test, because the interesting question is not how many
/// solids are prisms but which test the rest fail: `CapsInPieces` says the
/// source partitioned a face and the solid may be a prism anyway, where
/// `FaceNotSquareOrParallel` says this one is not. The first four are the
/// caller's - what it found before it could hand over polygons at all - and
/// are kept in the same list so that one tally covers every solid.
///
/// The order is the order the tests run in, which is what makes a tally of
/// them read as a funnel.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NotAPrism {
    /// A shell the decoder could not complete: faces its source declared were
    /// not resolved, so what this holds is part of a boundary. A sweep is a
    /// closed solid and half a boundary is not one.
    ShellIncomplete,
    /// A face on something other than a plane - a cylinder, a cone, a surface
    /// of revolution. Its boundary would put a curve in the profile, and this
    /// writes a polyline.
    SurfaceIsCurved,
    /// An edge that is not a line: an arc, or a sampled curve. The same.
    EdgeIsCurved,
    /// A point stated in a unit, or a frame, this cannot read.
    PointNotReadable,
    /// Fewer faces than a prism has, or a loop that is not a polygon.
    NotASolid,
    /// A face whose points do not lie in one plane.
    FaceNotPlanar,
    /// An edge that is not shared by exactly two faces: the shell has a hole
    /// in it, or states something twice, and what it bounds is not
    /// established.
    ShellNotClosed,
    /// No direction has two faces square to it facing each other.
    NoDirection,
    /// More than two faces stand square to the direction: the caps of this
    /// solid were stated in pieces, or it has a step in it.
    CapsInPieces,
    /// A face neither square to the direction nor parallel to it, so the solid
    /// leans or tapers.
    FaceNotSquareOrParallel,
    /// The two caps do not bound the same region.
    CapsDiffer,
    /// A sweep of no length.
    NoDepth,
}

impl NotAPrism {
    /// What to call this in a report, in the words the doc comments use.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ShellIncomplete => "the shell was incomplete",
            Self::SurfaceIsCurved => "a face was curved",
            Self::EdgeIsCurved => "an edge was curved",
            Self::PointNotReadable => "a point was unreadable",
            Self::NotASolid => "too few faces for a prism",
            Self::FaceNotPlanar => "a face was not flat",
            Self::ShellNotClosed => "the shell did not close",
            Self::NoDirection => "no two faces faced each other",
            Self::CapsInPieces => "the caps were stated in pieces",
            Self::FaceNotSquareOrParallel => "a face leaned",
            Self::CapsDiffer => "the two caps differed",
            Self::NoDepth => "the sweep had no length",
        }
    }
}

/// What a whole export made of the solids it wrote.
///
/// A funnel rather than a score: `swept` against the refusals says not only
/// how much of a file is written as sweeps but what the rest of it would take.
/// The counts are of solids - a member of an assembly is one - because that is
/// what the choice is made about.
#[derive(Clone, Debug, Default)]
pub struct SolidReport {
    /// Solids written as `IfcExtrudedAreaSolid`, and the faces they would have
    /// been written as.
    pub swept: Tally,
    /// The rest, by the test that refused them.
    pub refused: std::collections::BTreeMap<NotAPrism, Tally>,
    /// Of the solids refused for a curve, those whose every curve turns about
    /// one axis - the ones a profile that could hold an arc might reach.
    ///
    /// A necessary condition and not a sufficient one, so an upper bound: a
    /// solid whose cylinders and arcs do not share an axis cannot be a sweep
    /// whatever the profile can hold, and one whose do may still fail every
    /// test above. It is here because the alternative to measuring it is
    /// guessing whether the arc is worth writing.
    pub curves_about_one_axis: Tally,
    /// Elements left out of the file for carrying no body. See
    /// `ExportSettings::elements_without_a_body`, which decides whether they
    /// are; this only counts what that decided, so a reader of the report can
    /// see what the file is missing and why.
    pub bodiless_elements: usize,
    /// Placements of a body this file already holds: an element whose
    /// geometry is another element's, written once and mapped rather than
    /// repeated. See the note on `BodyWriter` in `metadata`.
    pub mapped_bodies: usize,
}

/// Some solids, and the faces they hold.
///
/// Both, because they answer different questions: the solids say how much of a
/// model is read, and the faces say how much of a file it is. A model is
/// mostly simple solids and mostly curved faces, so the two counts disagree by
/// design and a report that gave one would mislead.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tally {
    pub solids: usize,
    pub faces: usize,
}

impl Tally {
    /// Record one solid and the faces it holds.
    pub fn saw(&mut self, faces: usize) {
        self.solids += 1;
        self.faces += faces;
    }
}

impl SolidReport {
    /// Every solid the export offered this, swept or not.
    #[must_use]
    pub fn solids(&self) -> usize {
        self.swept.solids
            + self
                .refused
                .values()
                .map(|tally| tally.solids)
                .sum::<usize>()
    }

    /// Every face those solids hold.
    #[must_use]
    pub fn faces(&self) -> usize {
        self.swept.faces
            + self
                .refused
                .values()
                .map(|tally| tally.faces)
                .sum::<usize>()
    }

    /// Record one element held back for carrying no body.
    pub fn count_bodiless(&mut self) {
        self.bodiless_elements += 1;
    }

    /// Record one element placing a body the file already holds.
    pub fn count_mapped_body(&mut self) {
        self.mapped_bodies += 1;
    }

    /// Record one solid's outcome, and the size of the shell it came from.
    pub fn saw(&mut self, outcome: Result<&Prism, NotAPrism>, faces: usize) {
        match outcome {
            Ok(_) => self.swept.saw(faces),
            Err(refusal) => self.refused.entry(refusal).or_default().saw(faces),
        }
    }
}

/// Read a shell as a prism, or say which test it failed.
///
/// # Errors
///
/// Returns the test that refused it; every refusal leaves the caller writing
/// the shell as it stands.
pub fn recognise(faces: &[Face]) -> Result<Prism, NotAPrism> {
    // Five faces is the fewest a prism has: two caps over a triangle.
    if faces.len() < 5 {
        return Err(NotAPrism::NotASolid);
    }
    let mut normals = Vec::with_capacity(faces.len());
    for face in faces {
        normals.push(normal_of(face)?);
    }
    if !closes(faces) {
        return Err(NotAPrism::ShellNotClosed);
    }
    // A box is a prism three ways over and a reader should see the one its
    // author meant, so the element's own vertical is tried first: a wall, a
    // column and a floor are all stated upright in the frame this exporter
    // writes them in. After that, the face normals in the order the source
    // stated them, which keeps the choice a function of the file.
    let mut directions = vec![[0.0, 0.0, 1.0]];
    for normal in &normals {
        if !directions.iter().any(|held| parallel(*held, *normal)) {
            directions.push(*normal);
        }
    }
    // Every candidate is tried, because a direction can have two faces square
    // to it and still not be the one the solid sweeps along: a wedge has a top
    // and a bottom facing each other and a wall leaning between them, and is a
    // prism along neither. What is reported when none works is the first
    // refusal that got as far as finding its caps, which is the one that says
    // something about the solid rather than about the direction tried.
    let mut refusal = NotAPrism::NoDirection;
    for direction in directions {
        match along(faces, &normals, direction) {
            Ok(prism) => return Ok(prism),
            Err(NotAPrism::NoDirection) => {}
            Err(other) if refusal == NotAPrism::NoDirection => refusal = other,
            Err(_) => {}
        }
    }
    Err(refusal)
}

/// One face's plane, as its unit normal, refusing a boundary that is not a
/// planar polygon.
///
/// The normal comes from Newell's formula rather than from three chosen
/// points: it is a sum over the whole boundary, so a nearly-collinear corner
/// cannot decide which way a face faces on its own.
fn normal_of(face: &Face) -> Result<[f64; 3], NotAPrism> {
    let outer = face.first().ok_or(NotAPrism::NotASolid)?;
    if outer.len() < 3 {
        return Err(NotAPrism::NotASolid);
    }
    let mut normal = [0.0_f64; 3];
    for (at, point) in outer.iter().enumerate() {
        let next = outer[(at + 1) % outer.len()];
        normal[0] += (point[1] - next[1]) * (point[2] + next[2]);
        normal[1] += (point[2] - next[2]) * (point[0] + next[0]);
        normal[2] += (point[0] - next[0]) * (point[1] + next[1]);
    }
    let normal = unit(normal).ok_or(NotAPrism::NotASolid)?;
    let offset = dot(normal, outer[0]);
    for boundary in face {
        for point in boundary {
            if (dot(normal, *point) - offset).abs() > TOLERANCE {
                return Err(NotAPrism::FaceNotPlanar);
            }
        }
    }
    Ok(normal)
}

/// How near two faces must state a shared corner for it to be one corner.
///
/// A nanometre, and measured rather than chosen. Pairing the edges of the
/// 12 939 solids of the 231 MB architectural model by the *bits* of their
/// coordinates leaves 3 196 shells that do not close - a quarter of the model,
/// refused for corners that two faces state differently in the last few bits
/// of a double, because each face's points come out of its own chain of
/// arithmetic. Welding the corners first, the count falls to 293 at a
/// picometre and 199 at a nanometre, and then stops: a micrometre finds the
/// same 199, so between those two nothing else meets that was apart. A
/// nanometre is four orders below the `1e-5` metre precision the file itself
/// declares, so nothing the model distinguishes is being merged here.
const WELD: f64 = 1.0e-9;

/// Does this shell close, each edge used once from either side, all in one
/// piece?
///
/// This is the test the rest of the recognition stands on, and the one that
/// cannot be skipped: a box with a wall missing passes every other test here
/// and is not a box.
///
/// The edges are **directed**, and each must be met exactly once each way
/// round. Counting them undirected would take two uses for a pair, and a use
/// is not a pair: this corpus holds shells that are a closed box *plus* an
/// interior sheet stated twice over, whose duplicate faces use every one of
/// their edges twice in the same direction. Undirected, such a shell counts as
/// closed and its sweep quietly drops the sheet - which on one 10 mm wall of
/// the architectural model is 8.05 m² of stated surface out of 8.18. Directed,
/// it is refused and keeps the boundary representation it came with, sheet and
/// all: what this exporter writes is what it read.
///
/// Paired edges are still not enough on their own. A sheet stated twice, once
/// each way round, pairs every edge of itself perfectly: it is a closed
/// surface enclosing nothing, and a box beside one reads as closed. So the
/// faces must also hang together - one surface, not a solid and a ghost - and
/// they are joined into one through the edges they share.
///
/// Corners are welded within [`WELD`] first, because two faces meeting at one
/// corner do not state it to the bit - see the measurement there.
fn closes(faces: &[Face]) -> bool {
    let mut corners = Corners::default();
    // Per unordered pair of corners: how often it was used, and the sum of
    // the directions it was used in. A shell closes when every edge was used
    // exactly twice and those two uses ran opposite ways. Both halves are
    // needed: the count alone takes a sheet stated twice over for a closed
    // surface, and the directions alone take it for one stated four times.
    let mut edges: HashMap<[u32; 2], (u32, i32, usize)> = HashMap::new();
    let mut pieces = Pieces::new(faces.len());
    for (index, face) in faces.iter().enumerate() {
        for boundary in face {
            let ids: Vec<u32> = boundary.iter().map(|point| corners.id(*point)).collect();
            for (at, id) in ids.iter().enumerate() {
                let next = ids[(at + 1) % ids.len()];
                let (key, step) = if *id <= next {
                    ([*id, next], 1)
                } else {
                    ([next, *id], -1)
                };
                let held = edges.entry(key).or_insert((0, 0, index));
                held.0 += 1;
                held.1 += step;
                pieces.join(held.2, index);
            }
        }
    }
    edges
        .values()
        .all(|(uses, balance, _)| *uses == 2 && *balance == 0)
        && pieces.one_piece()
}

/// Which faces hang together, as the faces are read.
struct Pieces {
    of: Vec<usize>,
}

impl Pieces {
    fn new(faces: usize) -> Self {
        Self {
            of: (0..faces).collect(),
        }
    }

    fn root(&mut self, mut face: usize) -> usize {
        while self.of[face] != face {
            self.of[face] = self.of[self.of[face]];
            face = self.of[face];
        }
        face
    }

    fn join(&mut self, left: usize, right: usize) {
        let (left, right) = (self.root(left), self.root(right));
        if left != right {
            self.of[left] = right;
        }
    }

    fn one_piece(&mut self) -> bool {
        if self.of.is_empty() {
            return false;
        }
        let first = self.root(0);
        (0..self.of.len()).all(|face| self.root(face) == first)
    }
}

/// The corners of one shell, each given a number the first time it is seen, so
/// that two faces stating one corner state one number.
///
/// A grid of [`WELD`]-sized cells, and a lookup that reads the twenty-seven
/// cells around a point: two points within a weld of each other are in the
/// same cell or a neighbouring one, whichever side of a cell edge they fall.
#[derive(Default)]
struct Corners {
    cells: HashMap<[i64; 3], Vec<u32>>,
    points: Vec<[f64; 3]>,
}

impl Corners {
    fn id(&mut self, point: [f64; 3]) -> u32 {
        let cell = Self::cell(point);
        for x in -1..=1 {
            for y in -1..=1 {
                for z in -1..=1 {
                    let Some(held) = self.cells.get(&[cell[0] + x, cell[1] + y, cell[2] + z])
                    else {
                        continue;
                    };
                    for id in held {
                        let corner = self.points[*id as usize];
                        if corner
                            .into_iter()
                            .zip(point)
                            .all(|(held, asked)| (held - asked).abs() <= WELD)
                        {
                            return *id;
                        }
                    }
                }
            }
        }
        // A shell with more corners than this is not one this exporter will
        // see: the largest solid in the corpus states some thousands.
        let id = u32::try_from(self.points.len()).unwrap_or(u32::MAX);
        self.points.push(point);
        self.cells.entry(cell).or_default().push(id);
        id
    }

    #[allow(clippy::cast_possible_truncation)]
    // A coordinate in metres over a nanometre: a model larger than 9e9 metres
    // would be needed to reach the end of an i64, and its cells would be the
    // least of it.
    fn cell(point: [f64; 3]) -> [i64; 3] {
        point.map(|value| (value / WELD).floor() as i64)
    }
}

/// Test one direction: are two of these faces the caps of a sweep along it,
/// and is every other face a wall of that sweep?
fn along(faces: &[Face], normals: &[[f64; 3]], direction: [f64; 3]) -> Result<Prism, NotAPrism> {
    let mut caps = Vec::new();
    for (at, normal) in normals.iter().enumerate() {
        if parallel(*normal, direction) {
            caps.push(at);
        }
    }
    let [first, second] = caps[..] else {
        return Err(if caps.len() > 2 {
            NotAPrism::CapsInPieces
        } else {
            NotAPrism::NoDirection
        });
    };
    // Facing each other. Two faces square to the direction and facing the same
    // way are one cap the source stated in pieces, with the other cap among
    // the faces this would then refuse for leaning.
    if dot(normals[first], normals[second]) > 0.0 {
        return Err(NotAPrism::CapsInPieces);
    }
    for (at, normal) in normals.iter().enumerate() {
        if at != first && at != second && dot(*normal, direction).abs() > TOLERANCE {
            return Err(NotAPrism::FaceNotSquareOrParallel);
        }
    }

    // The sweep runs from the lower cap to the upper one, measured along the
    // direction rather than taken from either face's own normal.
    let (bottom, top) = if height(&faces[first], direction) <= height(&faces[second], direction) {
        (first, second)
    } else {
        (second, first)
    };
    let depth = height(&faces[top], direction) - height(&faces[bottom], direction);
    if depth <= TOLERANCE {
        return Err(NotAPrism::NoDepth);
    }

    let frame = Frame::about(direction);
    if !same_region(&faces[bottom], &faces[top], &frame) {
        return Err(NotAPrism::CapsDiffer);
    }

    let origin = faces[bottom][0][0];
    let mut boundaries = faces[bottom]
        .iter()
        .map(|boundary| frame.flatten(boundary, origin))
        .collect::<Vec<_>>();
    let outer = wound(boundaries.remove(0), true);
    let voids = boundaries
        .into_iter()
        .map(|boundary| wound(boundary, false))
        .collect();
    Ok(Prism {
        origin,
        x_axis: frame.x_axis,
        direction,
        depth,
        outer,
        voids,
    })
}

/// The plane's two spanning directions, so that a point of it can be named by
/// two numbers.
struct Frame {
    x_axis: [f64; 3],
    y_axis: [f64; 3],
}

impl Frame {
    /// A right-handed frame about `direction`, built from whichever coordinate
    /// axis is furthest from it. Any choice spans the same plane; this one is
    /// a function of the direction alone, so one solid exported twice states
    /// one profile.
    fn about(direction: [f64; 3]) -> Self {
        let mut smallest = 0;
        for axis in 1..3 {
            if direction[axis].abs() < direction[smallest].abs() {
                smallest = axis;
            }
        }
        let mut seed = [0.0; 3];
        seed[smallest] = 1.0;
        let x_axis = unit(cross(seed, direction)).unwrap_or([1.0, 0.0, 0.0]);
        Self {
            y_axis: cross(direction, x_axis),
            x_axis,
        }
    }

    /// Where a point of the plane sits in it.
    fn at(&self, point: [f64; 3], origin: [f64; 3]) -> [f64; 2] {
        let delta = subtract(point, origin);
        [dot(delta, self.x_axis), dot(delta, self.y_axis)]
    }

    fn flatten(&self, boundary: &Polygon, origin: [f64; 3]) -> Vec<[f64; 2]> {
        boundary
            .iter()
            .map(|point| self.at(*point, origin))
            .collect()
    }
}

/// Do two caps bound the same region?
///
/// Their edges, projected onto the plane and compared as undirected segments.
/// Comparing their corners alone would accept two different polygons through
/// the same points, which is a solid whose two ends really do differ.
fn same_region(bottom: &Face, top: &Face, frame: &Frame) -> bool {
    if bottom.len() != top.len() {
        return false;
    }
    let origin = bottom[0][0];
    let mut ours = edges_of(bottom, frame, origin);
    let mut theirs = edges_of(top, frame, origin);
    if ours.len() != theirs.len() {
        return false;
    }
    // Sorted, then compared in step. Two coordinates the model really does
    // state apart are further apart than the tolerance, so what the sort reads
    // is the model's order rather than its noise's, and an edge of one cap
    // lands beside the edge of the other it has to match.
    ours.sort_by(compare_edges);
    theirs.sort_by(compare_edges);
    ours.iter()
        .zip(&theirs)
        .all(|(ours, theirs)| same_point(ours[0], theirs[0]) && same_point(ours[1], theirs[1]))
}

/// One face's boundary edges, projected, each stated in a fixed order so that
/// the same edge read from either cap reads the same way.
fn edges_of(face: &Face, frame: &Frame, origin: [f64; 3]) -> Vec<[[f64; 2]; 2]> {
    let mut edges = Vec::new();
    for boundary in face {
        let flat = frame.flatten(boundary, origin);
        for (at, point) in flat.iter().enumerate() {
            let next = flat[(at + 1) % flat.len()];
            edges.push(if compare_points(*point, next).is_le() {
                [*point, next]
            } else {
                [next, *point]
            });
        }
    }
    edges
}

fn compare_edges(left: &[[f64; 2]; 2], right: &[[f64; 2]; 2]) -> std::cmp::Ordering {
    compare_points(left[0], right[0]).then_with(|| compare_points(left[1], right[1]))
}

fn compare_points(left: [f64; 2], right: [f64; 2]) -> std::cmp::Ordering {
    left[0]
        .total_cmp(&right[0])
        .then(left[1].total_cmp(&right[1]))
}

fn same_point(left: [f64; 2], right: [f64; 2]) -> bool {
    (left[0] - right[0]).abs() <= TOLERANCE && (left[1] - right[1]).abs() <= TOLERANCE
}

/// A boundary wound the way IFC asks for it: the outer profile
/// counter-clockwise about the sweep, its voids the other way.
fn wound(boundary: Vec<[f64; 2]>, counter_clockwise: bool) -> Vec<[f64; 2]> {
    let mut twice_area = 0.0;
    for (at, point) in boundary.iter().enumerate() {
        let next = boundary[(at + 1) % boundary.len()];
        twice_area += point[0].mul_add(next[1], -(next[0] * point[1]));
    }
    if (twice_area > 0.0) == counter_clockwise {
        boundary
    } else {
        boundary.into_iter().rev().collect()
    }
}

/// Where a face sits along a direction, measured at its first point. A face
/// this is asked of stands square to the direction, so its every point is
/// there.
fn height(face: &Face, direction: [f64; 3]) -> f64 {
    dot(direction, face[0][0])
}

fn parallel(left: [f64; 3], right: [f64; 3]) -> bool {
    length(cross(left, right)) <= TOLERANCE
}

fn unit(vector: [f64; 3]) -> Option<[f64; 3]> {
    let length = length(vector);
    (length > f64::MIN_POSITIVE).then(|| vector.map(|value| value / length))
}

fn length(vector: [f64; 3]) -> f64 {
    dot(vector, vector).sqrt()
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter().zip(right).map(|(a, b)| a * b).sum()
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1].mul_add(right[2], -(left[2] * right[1])),
        left[2].mul_add(right[0], -(left[0] * right[2])),
        left[0].mul_add(right[1], -(left[1] * right[0])),
    ]
}

fn subtract(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

#[cfg(test)]
mod tests {
    use super::{Face, NotAPrism, Polygon, Prism, recognise};

    /// The faces of a right prism over `profile`, wound so that every normal
    /// points out of the solid: the bottom cap the other way round from the
    /// profile, the top the same way, and one wall per side of it.
    ///
    /// A test that hands in a shell has to hand in a correct one: the
    /// recognition reads the winding to tell the two caps apart, and pairs the
    /// edges to know the shell closes.
    fn prism_faces(profile: &[[f64; 2]], base: f64, top: f64) -> Vec<Face> {
        let at = |point: [f64; 2], height: f64| [point[0], point[1], height];
        let bottom: Polygon = profile.iter().rev().map(|point| at(*point, base)).collect();
        let upper: Polygon = profile.iter().map(|point| at(*point, top)).collect();
        let mut faces = vec![vec![bottom], vec![upper]];
        for (index, point) in profile.iter().enumerate() {
            let next = profile[(index + 1) % profile.len()];
            faces.push(vec![vec![
                at(*point, base),
                at(next, base),
                at(next, top),
                at(*point, top),
            ]]);
        }
        faces
    }

    fn area(boundary: &[[f64; 2]]) -> f64 {
        let mut twice = 0.0;
        for (at, point) in boundary.iter().enumerate() {
            let next = boundary[(at + 1) % boundary.len()];
            twice += point[0].mul_add(next[1], -(next[0] * point[1]));
        }
        twice / 2.0
    }

    const RECTANGLE: [[f64; 2]; 4] = [[0.0, 0.0], [2.0, 0.0], [2.0, 3.0], [0.0, 3.0]];

    #[test]
    fn reads_a_box_as_the_sweep_of_its_footprint() {
        let prism = recognise(&prism_faces(&RECTANGLE, 0.0, 4.0)).expect("a prism");
        assert!(
            prism.direction[2] > 0.999,
            "swept upright, not {:?}",
            prism.direction
        );
        assert!((prism.depth - 4.0).abs() < 1e-12, "{}", prism.depth);
        assert_eq!(
            prism.outer.len(),
            4,
            "four sides, where the shell had six faces"
        );
        assert!(prism.voids.is_empty());
        // The profile is the footprint: the same area, wound as IFC asks.
        assert!(
            (area(&prism.outer) - 6.0).abs() < 1e-12,
            "{:?}",
            prism.outer
        );
    }

    /// The profile is stated in the plane's own two directions, so a reader
    /// rebuilding a point from the placement must land back on the shell's.
    #[test]
    fn the_profile_rebuilds_the_points_it_came_from() {
        let prism = recognise(&prism_faces(&RECTANGLE, 1.5, 2.5)).expect("a prism");
        let y_axis = super::cross(prism.direction, prism.x_axis);
        let rebuilt: Vec<[f64; 3]> = prism
            .outer
            .iter()
            .map(|point| {
                [0, 1, 2].map(|axis| {
                    prism.x_axis[axis]
                        .mul_add(point[0], y_axis[axis].mul_add(point[1], prism.origin[axis]))
                })
            })
            .collect();
        for corner in RECTANGLE {
            assert!(
                rebuilt
                    .iter()
                    .any(|point| (point[0] - corner[0]).abs() < 1e-12
                        && (point[1] - corner[1]).abs() < 1e-12
                        && (point[2] - 1.5).abs() < 1e-12),
                "{corner:?} is not among {rebuilt:?}"
            );
        }
    }

    #[test]
    fn reads_a_profile_that_is_not_convex() {
        let ell = [
            [0.0, 0.0],
            [3.0, 0.0],
            [3.0, 1.0],
            [1.0, 1.0],
            [1.0, 2.0],
            [0.0, 2.0],
        ];
        let prism = recognise(&prism_faces(&ell, 0.0, 0.5)).expect("a prism");
        assert_eq!(prism.outer.len(), 6);
        assert!(
            (area(&prism.outer) - 4.0).abs() < 1e-12,
            "{:?}",
            prism.outer
        );
    }

    /// A shaft through the solid is a void of the profile, wound against it,
    /// which is how a reader tells the two apart.
    #[test]
    fn reads_a_shaft_through_the_solid_as_a_void() {
        let hole = [[0.5, 0.5], [0.5, 1.5], [1.5, 1.5], [1.5, 0.5]];
        let mut faces = prism_faces(&RECTANGLE, 0.0, 1.0);
        // The shaft's walls face into it, so its loops run against the outer
        // boundary's on both caps - which is what makes the shell orientable,
        // and what the closure test reads.
        faces[0].push(hole.iter().rev().map(|at| [at[0], at[1], 0.0]).collect());
        faces[1].push(hole.iter().map(|at| [at[0], at[1], 1.0]).collect());
        for (index, point) in hole.iter().enumerate() {
            let next = hole[(index + 1) % hole.len()];
            faces.push(vec![vec![
                [point[0], point[1], 0.0],
                [next[0], next[1], 0.0],
                [next[0], next[1], 1.0],
                [point[0], point[1], 1.0],
            ]]);
        }
        let prism = recognise(&faces).expect("a prism");
        assert_eq!(prism.voids.len(), 1);
        assert!(area(&prism.outer) > 0.0, "the outer boundary winds forward");
        assert!(
            area(&prism.voids[0]) < 0.0,
            "the void winds against it: {:?}",
            prism.voids[0]
        );
        assert!((area(&prism.voids[0]).abs() - 1.0).abs() < 1e-12);
    }

    /// A solid swept sideways is read sideways. The upright direction is only
    /// tried first; it is not assumed.
    #[test]
    fn reads_a_solid_swept_along_another_direction() {
        let triangle = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        // Lay the triangle in the XZ plane, so the sweep runs along Y.
        let laid: Vec<Face> = prism_faces(&triangle, 0.0, 2.0)
            .into_iter()
            .map(|face| {
                face.into_iter()
                    .map(|boundary| {
                        boundary
                            .into_iter()
                            .map(|point| [point[0], point[2], point[1]])
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let prism = recognise(&laid).expect("a prism");
        assert!(
            prism.direction[1].abs() > 0.999,
            "swept along Y: {:?}",
            prism.direction
        );
        assert!((prism.depth - 2.0).abs() < 1e-12);
        assert!((area(&prism.outer) - 0.5).abs() < 1e-12);
    }

    /// A box whose top the source stated in two halves is still a prism - on
    /// its side, where the two faces square to the sweep are whole. What the
    /// recognition gives up on a split cap is that one direction, not the
    /// solid.
    #[test]
    fn reads_a_box_whose_cap_was_stated_in_pieces_the_other_way_round() {
        // The split reaches the walls it runs into: a shell states a corner
        // wherever it has one, so the two walls it crosses carry it too.
        let mut faces = vec![
            vec![vec![
                [0.0, 3.0, 0.0],
                [2.0, 3.0, 0.0],
                [2.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
            ]],
            vec![vec![
                [0.0, 0.0, 1.0],
                [2.0, 0.0, 1.0],
                [2.0, 1.5, 1.0],
                [0.0, 1.5, 1.0],
            ]],
            vec![vec![
                [0.0, 1.5, 1.0],
                [2.0, 1.5, 1.0],
                [2.0, 3.0, 1.0],
                [0.0, 3.0, 1.0],
            ]],
            vec![vec![
                [0.0, 0.0, 0.0],
                [2.0, 0.0, 0.0],
                [2.0, 0.0, 1.0],
                [0.0, 0.0, 1.0],
            ]],
            vec![vec![
                [2.0, 0.0, 0.0],
                [2.0, 3.0, 0.0],
                [2.0, 3.0, 1.0],
                [2.0, 1.5, 1.0],
                [2.0, 0.0, 1.0],
            ]],
            vec![vec![
                [2.0, 3.0, 0.0],
                [0.0, 3.0, 0.0],
                [0.0, 3.0, 1.0],
                [2.0, 3.0, 1.0],
            ]],
            vec![vec![
                [0.0, 3.0, 0.0],
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 1.5, 1.0],
                [0.0, 3.0, 1.0],
            ]],
        ];
        let _ = &mut faces;
        // The split runs across the box, so the two walls at y=0 and y=3 are
        // whole and the sweep is read between them.
        let prism = recognise(&faces).expect("a prism");
        assert!(
            prism.direction[1].abs() > 0.999,
            "{:?} is not the Y the whole faces leave",
            prism.direction
        );
        assert!((prism.depth - 3.0).abs() < 1e-12);
    }

    /// The same split on a solid that is a prism one way only. Nothing is
    /// written, and what is reported is the split rather than a shrug: this is
    /// the count that says what merging coplanar faces would be worth.
    #[test]
    fn says_when_a_cap_stated_in_pieces_is_the_only_direction() {
        let ell = [
            [0.0, 0.0],
            [3.0, 0.0],
            [3.0, 1.0],
            [1.0, 1.0],
            [1.0, 2.0],
            [0.0, 2.0],
        ];
        let mut faces = prism_faces(&ell, 0.0, 1.0);
        // The top in two pieces, and the wall the split runs into carrying the
        // corner it makes there.
        faces.remove(1);
        faces.push(vec![vec![
            [0.0, 0.0, 1.0],
            [3.0, 0.0, 1.0],
            [3.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [0.0, 1.0, 1.0],
        ]]);
        faces.push(vec![vec![
            [0.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 2.0, 1.0],
            [0.0, 2.0, 1.0],
        ]]);
        let last = faces.len() - 3;
        faces[last] = vec![vec![
            [0.0, 2.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
            [0.0, 2.0, 1.0],
        ]];
        assert_eq!(recognise(&faces), Err(NotAPrism::CapsInPieces));
    }

    #[test]
    fn refuses_a_solid_that_tapers() {
        let base = [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        let top = [[0.5, 0.5], [1.5, 0.5], [1.5, 1.5], [0.5, 1.5]];
        let mut faces = vec![
            vec![
                base.iter()
                    .rev()
                    .map(|at| [at[0], at[1], 0.0])
                    .collect::<Polygon>(),
            ],
            vec![top.iter().map(|at| [at[0], at[1], 1.0]).collect()],
        ];
        for (index, point) in base.iter().enumerate() {
            let next = base[(index + 1) % base.len()];
            faces.push(vec![vec![
                [point[0], point[1], 0.0],
                [next[0], next[1], 0.0],
                [
                    top[(index + 1) % top.len()][0],
                    top[(index + 1) % top.len()][1],
                    1.0,
                ],
                [top[index][0], top[index][1], 1.0],
            ]]);
        }
        assert_eq!(
            recognise(&faces),
            Err(NotAPrism::FaceNotSquareOrParallel),
            "a leaning wall is not the wall of a sweep"
        );
    }

    /// Two caps that match are not enough: an antiprism has the same square at
    /// both ends and triangles between them.
    #[test]
    fn refuses_an_antiprism_whose_caps_match() {
        let square = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let mut faces = vec![
            vec![
                square
                    .iter()
                    .rev()
                    .map(|at| [at[0], at[1], 0.0])
                    .collect::<Polygon>(),
            ],
            vec![square.iter().map(|at| [at[0], at[1], 1.0]).collect()],
        ];
        for (index, point) in square.iter().enumerate() {
            let next = square[(index + 1) % square.len()];
            let above = square[(index + 2) % square.len()];
            faces.push(vec![vec![
                [point[0], point[1], 0.0],
                [next[0], next[1], 0.0],
                [above[0], above[1], 1.0],
            ]]);
            faces.push(vec![vec![
                [point[0], point[1], 0.0],
                [above[0], above[1], 1.0],
                [
                    square[(index + 1) % square.len()][0],
                    square[(index + 1) % square.len()][1],
                    1.0,
                ],
            ]]);
        }
        assert_eq!(recognise(&faces), Err(NotAPrism::FaceNotSquareOrParallel));
    }

    #[test]
    fn refuses_a_solid_with_no_two_faces_facing_each_other() {
        let pyramid = vec![
            vec![vec![
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [1.0, 1.0, 0.0],
                [1.0, 0.0, 0.0],
            ]],
            vec![vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.5, 0.5, 1.0]]],
            vec![vec![[1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.5, 0.5, 1.0]]],
            vec![vec![[1.0, 1.0, 0.0], [0.0, 1.0, 0.0], [0.5, 0.5, 1.0]]],
            vec![vec![[0.0, 1.0, 0.0], [0.0, 0.0, 0.0], [0.5, 0.5, 1.0]]],
        ];
        assert_eq!(recognise(&pyramid), Err(NotAPrism::NoDirection));
    }

    /// The test the rest of the recognition stands on: a box with a wall
    /// missing has two matching caps and no leaning face, and is not a box.
    #[test]
    fn refuses_a_shell_with_a_wall_missing() {
        let mut faces = prism_faces(&RECTANGLE, 0.0, 1.0);
        faces.pop();
        assert_eq!(recognise(&faces), Err(NotAPrism::ShellNotClosed));
    }

    /// A shell the corpus taught this: a closed box with an interior sheet
    /// stated twice over, each copy wound the same way. Every edge of the
    /// sheet is used twice, so counting uses would call the shell closed and
    /// sweep the box - dropping 8.05 m² of the 8.18 m² one such wall states.
    /// Counted as directions, the two copies use their edges the same way
    /// round and the shell is refused.
    #[test]
    fn refuses_a_box_with_an_interior_sheet_stated_twice() {
        let mut faces = prism_faces(&RECTANGLE, 0.0, 1.0);
        let sheet: Polygon = vec![
            [1.0, 0.0, 0.0],
            [1.0, 3.0, 0.0],
            [1.0, 3.0, 1.0],
            [1.0, 0.0, 1.0],
        ];
        faces.push(vec![sheet.clone()]);
        faces.push(vec![sheet]);
        assert_eq!(recognise(&faces), Err(NotAPrism::ShellNotClosed));
    }

    /// The other shape the corpus taught this: a box beside a sheet stated
    /// twice, once each way round. Those two copies pair every edge of
    /// themselves, in opposite directions, and enclose nothing - so the pairing
    /// alone calls the shell closed. They are a second piece, and a shell is
    /// one piece.
    #[test]
    fn refuses_a_box_beside_a_sheet_facing_both_ways() {
        let mut faces = prism_faces(&RECTANGLE, 0.0, 1.0);
        let sheet: Polygon = vec![
            [5.0, 0.0, 0.0],
            [5.0, 3.0, 0.0],
            [5.0, 3.0, 1.0],
            [5.0, 0.0, 1.0],
        ];
        faces.push(vec![sheet.iter().copied().rev().collect()]);
        faces.push(vec![sheet]);
        assert_eq!(recognise(&faces), Err(NotAPrism::ShellNotClosed));
    }

    #[test]
    fn refuses_a_face_that_is_not_flat() {
        let mut faces = prism_faces(&RECTANGLE, 0.0, 1.0);
        // A wall of the y=0 plane, with one corner pulled out of it - in every
        // face that states the corner, so the shell still closes.
        for face in &mut faces {
            for boundary in face {
                for point in boundary {
                    let corner = [2.0, 0.0, 1.0];
                    if point
                        .iter()
                        .zip(corner)
                        .all(|(at, want)| (at - want).abs() < 1e-12)
                    {
                        point[1] = 0.001;
                    }
                }
            }
        }
        assert_eq!(recognise(&faces), Err(NotAPrism::FaceNotPlanar));
    }

    /// Noise in the last bits of a double is what two chains of matrix
    /// multiplications leave behind, and it must not decide whether a wall is
    /// a wall. One vertex is nudged the same way wherever it is stated, which
    /// is how a body carries the noise of its own placement.
    #[test]
    fn reads_a_prism_through_the_noise_of_its_arithmetic() {
        let mut faces = prism_faces(&RECTANGLE, 0.0, 4.0);
        for face in &mut faces {
            for boundary in face {
                for point in boundary {
                    let key = point[0].mul_add(31.0, point[1].mul_add(17.0, point[2] * 7.0));
                    for (turn, value) in [1.0, 2.0, 3.0].into_iter().zip(point.iter_mut()) {
                        *value += 1e-12 * (key * turn).sin();
                    }
                }
            }
        }
        let prism = recognise(&faces).expect("a prism");
        assert!((prism.depth - 4.0).abs() < 1e-9, "{}", prism.depth);
        assert!((area(&prism.outer) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn a_prism_states_in_one_polygon_what_a_shell_states_in_many_faces() {
        let faces = prism_faces(&RECTANGLE, 0.0, 1.0);
        let prism: Prism = recognise(&faces).expect("a prism");
        let shell_points: usize = faces
            .iter()
            .flat_map(|face| face.iter().map(Vec::len))
            .sum();
        assert!(prism.outer.len() < shell_points / 3, "{shell_points}");
    }
}
