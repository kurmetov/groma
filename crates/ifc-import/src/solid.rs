//! Turning the representation items a file states into `bim-core` bodies.
//!
//! Everything here ends as planar faces or a swept disk, in world coordinates
//! and in metres. An item form this does not read is counted in
//! [`Bodies::unread_items`] and contributes nothing, so an element is never
//! given a shape the file did not state. The one place that rule bends is a
//! boolean result, which is reported separately in [`Bodies::approximated`]:
//! the cut solid is drawn, the cut is not.

use bim_core::{
    BimBoundingBox, BimBrep, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepSurface, BimGeometry,
    BimLineSegment, BimNumber, BimPoint3, BimSweptDisk, BimUnit,
};
use bim_mesh::METRES;

use crate::curve::{Sampler, arc_steps, close_enough};
use crate::place::{
    Affine, IDENTITY, Vec3, add, axis_placement, cartesian_point, coordinates, cross, direction,
    dot, length, normalize, scale, subtract, transformation_operator,
};
use crate::step::{Entity, Parsed, Value};

#[must_use]
pub fn metres() -> BimUnit {
    BimUnit {
        id: METRES.to_owned(),
        name: "Meters".to_owned(),
    }
}

#[must_use]
pub fn point3(at: Vec3) -> BimPoint3 {
    BimPoint3 {
        coordinates: at,
        unit: metres(),
    }
}

/// What one element's representation came to.
#[derive(Debug, Default)]
pub struct Bodies {
    /// Shells, each `complete` only where the file vouched for it as closed.
    pub shells: Vec<BimBrep>,
    /// Swept disks whose directrix is one straight run, which `bim-core`
    /// states exactly rather than as facets.
    pub disks: Vec<BimSweptDisk>,
    /// A stated extent, used only where nothing else was read.
    pub boxes: Vec<BimBoundingBox>,
    /// Items whose form this does not read.
    pub unread_items: usize,
    /// Items drawn without a cut the file states - the operands of a boolean
    /// result.
    pub approximated: usize,
}

impl Bodies {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shells.is_empty() && self.disks.is_empty()
    }

    /// The one geometry these bodies make, in the form that keeps the most of
    /// what was read.
    #[must_use]
    pub fn finish(mut self, tolerance: f64) -> Option<BimGeometry> {
        if self.shells.is_empty() && self.disks.len() == 1 {
            return Some(BimGeometry::SweptDisk(self.disks.remove(0)));
        }
        // A disk alongside other bodies cannot stay analytic: `bim-core` has
        // one geometry per element, so the disk is faceted into a shell like
        // everything else.
        for disk in std::mem::take(&mut self.disks) {
            let radius = disk.radius.value;
            if let Some(shell) = tube(
                &[
                    disk.directrix.start.coordinates,
                    disk.directrix.end.coordinates,
                ],
                radius,
                tolerance,
            ) {
                self.shells.push(shell);
            }
        }
        match self.shells.len() {
            0 => self.boxes.into_iter().next().map(BimGeometry::BoundingBox),
            1 => Some(BimGeometry::Brep(self.shells.remove(0))),
            // Every member of an assembly is a closed solid in its own right.
            // Where one is not, the faces are one open shell instead, which is
            // the honest reading of what was recovered.
            _ if self.shells.iter().all(|shell| shell.complete) => {
                Some(BimGeometry::Assembly(self.shells))
            }
            _ => Some(BimGeometry::Brep(BimBrep {
                faces: self
                    .shells
                    .into_iter()
                    .flat_map(|shell| shell.faces)
                    .collect(),
                complete: false,
            })),
        }
    }
}

/// Reads representation items out of one file.
pub struct Builder<'parsed> {
    pub sampler: Sampler<'parsed>,
    pub bodies: Bodies,
}

impl<'parsed> Builder<'parsed> {
    #[must_use]
    pub fn new(sampler: Sampler<'parsed>) -> Self {
        Self {
            sampler,
            bodies: Bodies::default(),
        }
    }

    fn parsed(&self) -> &'parsed Parsed {
        self.sampler.parsed
    }

    /// Read one representation item, placed by `place`.
    pub fn item(&mut self, entity: &Entity, place: &Affine, depth: u8) {
        if depth > 12 {
            self.bodies.unread_items += 1;
            return;
        }
        match entity.type_name.as_str() {
            "IFCEXTRUDEDAREASOLID" | "IFCEXTRUDEDAREASOLIDTAPERED" => self.extruded(entity, place),
            "IFCFACETEDBREP" | "IFCADVANCEDBREP" => {
                if let Some(shell) = self.parsed().follow(entity.attribute(0)) {
                    self.shell(shell, place, true);
                } else {
                    self.bodies.unread_items += 1;
                }
            }
            "IFCFACETEDBREPWITHVOIDS" | "IFCADVANCEDBREPWITHVOIDS" => {
                // The voids are a cut this does not make; the outer shell is
                // read and the cut reported.
                self.bodies.approximated += 1;
                if let Some(shell) = self.parsed().follow(entity.attribute(0)) {
                    self.shell(shell, place, true);
                }
            }
            "IFCCLOSEDSHELL" => self.shell(entity, place, true),
            "IFCOPENSHELL" | "IFCCONNECTEDFACESET" => self.shell(entity, place, false),
            "IFCSHELLBASEDSURFACEMODEL" | "IFCFACEBASEDSURFACEMODEL" => {
                let Some(members) = entity.attribute(0).and_then(Value::as_list) else {
                    self.bodies.unread_items += 1;
                    return;
                };
                for member in members {
                    if let Some(shell) = self.parsed().follow(Some(member)) {
                        let closed = shell.type_name == "IFCCLOSEDSHELL";
                        self.shell(shell, place, closed);
                    }
                }
            }
            "IFCPOLYGONALFACESET" => self.polygonal_face_set(entity, place),
            "IFCTRIANGULATEDFACESET" => self.triangulated_face_set(entity, place),
            "IFCSWEPTDISKSOLID" | "IFCSWEPTDISKSOLIDPOLYGONAL" => self.swept_disk(entity, place),
            "IFCMAPPEDITEM" => self.mapped(entity, place, depth),
            // A boolean result is drawn as the solid it cuts from. The cut
            // itself is not made, which is what `approximated` reports.
            "IFCBOOLEANRESULT" | "IFCBOOLEANCLIPPINGRESULT" => {
                self.bodies.approximated += 1;
                if let Some(first) = self.parsed().follow(entity.attribute(1)) {
                    self.item(first, place, depth + 1);
                }
            }
            "IFCCSGSOLID" => {
                if let Some(root) = self.parsed().follow(entity.attribute(0)) {
                    self.item(root, place, depth + 1);
                } else {
                    self.bodies.unread_items += 1;
                }
            }
            "IFCBOUNDINGBOX" => self.bounding_box(entity, place),
            _ => self.bodies.unread_items += 1,
        }
    }

    /// Every item of a representation.
    pub fn representation(&mut self, representation: &Entity, place: &Affine, depth: u8) {
        // `Items` is the fourth attribute of `IfcRepresentation`.
        let Some(items) = representation.attribute(3).and_then(Value::as_list) else {
            return;
        };
        for item in items {
            if let Some(entity) = self.parsed().follow(Some(item)) {
                self.item(entity, place, depth);
            }
        }
    }

    fn mapped(&mut self, entity: &Entity, place: &Affine, depth: u8) {
        let Some(source) = self.parsed().follow(entity.attribute(0)) else {
            self.bodies.unread_items += 1;
            return;
        };
        let origin = self
            .parsed()
            .follow(source.attribute(0))
            .and_then(|placement| axis_placement(self.parsed(), placement, &self.sampler.units))
            .unwrap_or(IDENTITY);
        let target = self
            .parsed()
            .follow(entity.attribute(1))
            .and_then(|operator| {
                transformation_operator(self.parsed(), operator, &self.sampler.units)
            })
            .unwrap_or(IDENTITY);
        let Some(mapped) = self.parsed().follow(source.attribute(1)) else {
            self.bodies.unread_items += 1;
            return;
        };
        let inner = place.then(&target).then(&origin);
        self.representation(mapped, &inner, depth + 1);
    }

    fn extruded(&mut self, entity: &Entity, place: &Affine) {
        let Some(regions) = self
            .parsed()
            .follow(entity.attribute(0))
            .and_then(|profile| self.profile(profile, 0))
        else {
            self.bodies.unread_items += 1;
            return;
        };
        let position = self
            .parsed()
            .follow(entity.attribute(1))
            .and_then(|placement| axis_placement(self.parsed(), placement, &self.sampler.units))
            .unwrap_or(IDENTITY);
        let Some(heading) = self
            .parsed()
            .follow(entity.attribute(2))
            .and_then(direction)
            .and_then(normalize)
        else {
            self.bodies.unread_items += 1;
            return;
        };
        let Some(depth) = entity.attribute(3).and_then(Value::as_number) else {
            self.bodies.unread_items += 1;
            return;
        };
        let sweep = scale(heading, depth * self.sampler.units.length);
        if length(sweep) <= 1e-12 {
            self.bodies.unread_items += 1;
            return;
        }
        // The profile is stated in the position's own frame, and so is the
        // direction it is swept in, so one map carries both into the world.
        let into_world = place.then(&position);
        for region in regions {
            if let Some(shell) = prism(&region, sweep, &into_world) {
                self.bodies.shells.push(shell);
            } else {
                self.bodies.unread_items += 1;
            }
        }
    }

    fn swept_disk(&mut self, entity: &Entity, place: &Affine) {
        let Some(path) = self
            .parsed()
            .follow(entity.attribute(0))
            .and_then(|curve| self.sampler.curve(curve))
        else {
            self.bodies.unread_items += 1;
            return;
        };
        let Some(radius) = entity.attribute(1).and_then(Value::as_number) else {
            self.bodies.unread_items += 1;
            return;
        };
        // A hollow sweep is drawn solid; the bore is a cut this does not make.
        if entity.attribute(2).and_then(Value::as_number).is_some() {
            self.bodies.approximated += 1;
        }
        let mut radius = radius * self.sampler.units.length;
        let placed: Vec<Vec3> = path.iter().map(|at| place.point(*at)).collect();
        // A placement that stretches one direction more than another does not
        // carry a radius, so the tube is not drawn rather than drawn at a size
        // the file never stated.
        let Some(factor) = place.uniform_scale() else {
            self.bodies.unread_items += 1;
            return;
        };
        radius *= factor;
        if placed.len() == 2 {
            self.bodies.disks.push(BimSweptDisk {
                directrix: BimLineSegment {
                    start: point3(placed[0]),
                    end: point3(placed[1]),
                },
                radius: BimNumber {
                    value: radius,
                    unit: Some(metres()),
                },
            });
        } else if let Some(shell) = tube(&placed, radius, self.sampler.tolerance) {
            self.bodies.shells.push(shell);
        } else {
            self.bodies.unread_items += 1;
        }
    }

    fn bounding_box(&mut self, entity: &Entity, place: &Affine) {
        let Some(corner) = self
            .parsed()
            .follow(entity.attribute(0))
            .and_then(cartesian_point)
        else {
            return;
        };
        let scale_of = |at: usize| {
            entity
                .attribute(at)
                .and_then(Value::as_number)
                .unwrap_or_default()
                * self.sampler.units.length
        };
        let low = scale(corner, self.sampler.units.length);
        let high = add(low, [scale_of(1), scale_of(2), scale_of(3)]);
        // The box is stated axis-aligned in its own frame; placing its two
        // corners keeps it a box only where the placement does not turn it,
        // so both are placed and the extent taken over all eight corners.
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for index in 0..8 {
            let corner = place.point([
                if index & 1 == 0 { low[0] } else { high[0] },
                if index & 2 == 0 { low[1] } else { high[1] },
                if index & 4 == 0 { low[2] } else { high[2] },
            ]);
            for axis in 0..3 {
                min[axis] = min[axis].min(corner[axis]);
                max[axis] = max[axis].max(corner[axis]);
            }
        }
        self.bodies.boxes.push(BimBoundingBox {
            min: point3(min),
            max: point3(max),
        });
    }

    /// An `IfcClosedShell` or `IfcOpenShell`: a set of faces, each stated
    /// wound counter-clockwise seen from outside the shell.
    fn shell(&mut self, entity: &Entity, place: &Affine, closed: bool) {
        let Some(members) = entity.attribute(0).and_then(Value::as_list) else {
            self.bodies.unread_items += 1;
            return;
        };
        let mut faces = Vec::with_capacity(members.len());
        let mut unread = 0;
        for member in members {
            let Some(entity) = self.parsed().follow(Some(member)) else {
                unread += 1;
                continue;
            };
            match self.face(entity, place) {
                Some(face) => faces.push(face),
                None => unread += 1,
            }
        }
        if faces.is_empty() {
            self.bodies.unread_items += 1;
            return;
        }
        self.bodies.shells.push(BimBrep {
            faces,
            // A shell missing a face is no longer a claim of a closed solid,
            // whatever the file called it.
            complete: closed && unread == 0,
        });
    }

    fn face(&mut self, entity: &Entity, place: &Affine) -> Option<BimBrepFace> {
        if entity.type_name != "IFCFACE" && entity.type_name != "IFCADVANCEDFACE" {
            return None;
        }
        let mut outer: Option<Vec<Vec3>> = None;
        let mut inner: Vec<Vec<Vec3>> = Vec::new();
        for member in entity.attribute(0)?.as_list()? {
            let bound = self.parsed().follow(Some(member))?;
            let ring = self.parsed().follow(bound.attribute(0))?;
            if ring.type_name != "IFCPOLYLOOP" {
                return None;
            }
            let mut points: Vec<Vec3> = Vec::new();
            for vertex in ring.attribute(0)?.as_list()? {
                let at = self
                    .parsed()
                    .follow(Some(vertex))
                    .and_then(cartesian_point)?;
                points.push(place.point(scale(at, self.sampler.units.length)));
            }
            // `Orientation` false means the loop runs against the face.
            if bound.attribute(1).and_then(Value::as_enumeration) == Some("F") {
                points.reverse();
            }
            if bound.type_name == "IFCFACEOUTERBOUND" && outer.is_none() {
                outer = Some(points);
            } else {
                inner.push(points);
            }
        }
        // A face stating no bound as its outer one is still readable: the
        // largest ring is the one the others are cut out of.
        let outer = if let Some(points) = outer {
            points
        } else {
            let widest = inner
                .iter()
                .enumerate()
                .max_by(|left, right| {
                    ring_extent(left.1)
                        .partial_cmp(&ring_extent(right.1))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(index, _)| index)?;
            inner.remove(widest)
        };
        planar_face(&outer, &inner)
    }

    fn polygonal_face_set(&mut self, entity: &Entity, place: &Affine) {
        let Some((points, index_map)) = self.tessellated_points(entity, place, 3) else {
            self.bodies.unread_items += 1;
            return;
        };
        let Some(members) = entity.attribute(2).and_then(Value::as_list) else {
            self.bodies.unread_items += 1;
            return;
        };
        let at = |index: &Value| -> Option<Vec3> {
            let stated = usize::try_from(index.as_integer()?).ok()?.checked_sub(1)?;
            let resolved = match &index_map {
                Some(map) => *map.get(stated)?,
                None => stated,
            };
            points.get(resolved).copied()
        };
        let ring = |indices: &Value| -> Option<Vec<Vec3>> {
            indices
                .as_list()?
                .iter()
                .map(at)
                .collect::<Option<Vec<_>>>()
        };
        let mut faces = Vec::with_capacity(members.len());
        let mut unread = 0;
        for member in members {
            let Some(face) = self.parsed().follow(Some(member)) else {
                unread += 1;
                continue;
            };
            let Some(outer) = face.attribute(0).and_then(&ring) else {
                unread += 1;
                continue;
            };
            let holes: Vec<Vec<Vec3>> = face
                .attribute(1)
                .and_then(Value::as_list)
                .map(|inner| inner.iter().filter_map(&ring).collect())
                .unwrap_or_default();
            match planar_face(&outer, &holes) {
                Some(face) => faces.push(face),
                None => unread += 1,
            }
        }
        if faces.is_empty() {
            self.bodies.unread_items += 1;
            return;
        }
        let closed = entity.attribute(1).and_then(Value::as_enumeration) == Some("T");
        self.bodies.shells.push(BimBrep {
            faces,
            complete: closed && unread == 0,
        });
    }

    fn triangulated_face_set(&mut self, entity: &Entity, place: &Affine) {
        let Some((points, index_map)) = self.tessellated_points(entity, place, 4) else {
            self.bodies.unread_items += 1;
            return;
        };
        let Some(members) = entity.attribute(3).and_then(Value::as_list) else {
            self.bodies.unread_items += 1;
            return;
        };
        let mut faces = Vec::with_capacity(members.len());
        let mut unread = 0;
        for member in members {
            let corners: Option<Vec<Vec3>> = member.as_list().and_then(|indices| {
                indices
                    .iter()
                    .map(|index| {
                        let stated = usize::try_from(index.as_integer()?).ok()?.checked_sub(1)?;
                        let resolved = match &index_map {
                            Some(map) => *map.get(stated)?,
                            None => stated,
                        };
                        points.get(resolved).copied()
                    })
                    .collect()
            });
            match corners.and_then(|corners| planar_face(&corners, &[])) {
                Some(face) => faces.push(face),
                None => unread += 1,
            }
        }
        if faces.is_empty() {
            self.bodies.unread_items += 1;
            return;
        }
        let closed = entity.attribute(2).and_then(Value::as_enumeration) == Some("T");
        self.bodies.shells.push(BimBrep {
            faces,
            complete: closed && unread == 0,
        });
    }

    /// The coordinates of a tessellated face set, placed, and the `PnIndex`
    /// indirection where the file states one.
    fn tessellated_points(
        &self,
        entity: &Entity,
        place: &Affine,
        index_at: usize,
    ) -> Option<(Vec<Vec3>, Option<Vec<usize>>)> {
        let list = self.parsed().follow(entity.attribute(0))?;
        let mut points = Vec::new();
        for value in list.attribute(0)?.as_list()? {
            points.push(place.point(scale(coordinates(value)?, self.sampler.units.length)));
        }
        let index_map = entity
            .attribute(index_at)
            .and_then(Value::as_list)
            .map(|indices| {
                indices
                    .iter()
                    .map(|index| {
                        index
                            .as_integer()
                            .and_then(|value| usize::try_from(value).ok())
                            .and_then(|value| value.checked_sub(1))
                            .unwrap_or(usize::MAX)
                    })
                    .collect::<Vec<usize>>()
            });
        Some((points, index_map))
    }

    /// A profile's boundary rings, in the profile's own plane.
    ///
    /// A composite profile is several regions at once, which is why this
    /// returns a list rather than one.
    #[allow(clippy::too_many_lines)]
    // Every profile form the corpus states is one arm of this match, and
    // splitting them apart would only move the same reading somewhere else.
    fn profile(&self, entity: &Entity, depth: u8) -> Option<Vec<Region>> {
        if depth > 6 {
            return None;
        }
        let position = || {
            self.parsed()
                .follow(entity.attribute(2))
                .and_then(|placement| axis_placement(self.parsed(), placement, &self.sampler.units))
                .unwrap_or(IDENTITY)
        };
        let number = |at: usize| {
            entity
                .attribute(at)
                .and_then(Value::as_number)
                .map(|value| value * self.sampler.units.length)
        };
        match entity.type_name.as_str() {
            "IFCARBITRARYCLOSEDPROFILEDEF" | "IFCARBITRARYPROFILEDEFWITHVOIDS" => {
                let outer = self
                    .parsed()
                    .follow(entity.attribute(2))
                    .and_then(|curve| self.sampler.curve(curve))?;
                let holes = entity
                    .attribute(3)
                    .and_then(Value::as_list)
                    .map(|inner| {
                        inner
                            .iter()
                            .filter_map(|value| {
                                self.parsed()
                                    .follow(Some(value))
                                    .and_then(|curve| self.sampler.curve(curve))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(vec![Region { outer, holes }])
            }
            "IFCRECTANGLEPROFILEDEF" | "IFCRECTANGLEHOLLOWPROFILEDEF" => {
                let position = position();
                let (width, height) = (number(3)?, number(4)?);
                let outer = rectangle(&position, width, height);
                let holes = number(5)
                    .filter(|thickness| {
                        *thickness > 0.0 && width > 2.0 * thickness && height > 2.0 * thickness
                    })
                    .map(|thickness| {
                        vec![rectangle(
                            &position,
                            2.0f64.mul_add(-thickness, width),
                            2.0f64.mul_add(-thickness, height),
                        )]
                    })
                    .unwrap_or_default();
                Some(vec![Region { outer, holes }])
            }
            "IFCCIRCLEPROFILEDEF" | "IFCCIRCLEHOLLOWPROFILEDEF" => {
                let position = position();
                let radius = number(3)?;
                let outer = circle(&position, radius, self.sampler.tolerance);
                let holes = number(4)
                    .filter(|thickness| *thickness > 0.0 && *thickness < radius)
                    .map(|thickness| {
                        vec![circle(
                            &position,
                            radius - thickness,
                            self.sampler.tolerance,
                        )]
                    })
                    .unwrap_or_default();
                Some(vec![Region { outer, holes }])
            }
            "IFCDERIVEDPROFILEDEF" => {
                let parent = self.parsed().follow(entity.attribute(2))?;
                let operator = self
                    .parsed()
                    .follow(entity.attribute(3))
                    .and_then(|operator| {
                        transformation_operator(self.parsed(), operator, &self.sampler.units)
                    })
                    .unwrap_or(IDENTITY);
                let regions = self.profile(parent, depth + 1)?;
                Some(
                    regions
                        .into_iter()
                        .map(|region| Region {
                            outer: region.outer.iter().map(|at| operator.point(*at)).collect(),
                            holes: region
                                .holes
                                .iter()
                                .map(|hole| hole.iter().map(|at| operator.point(*at)).collect())
                                .collect(),
                        })
                        .collect(),
                )
            }
            "IFCCOMPOSITEPROFILEDEF" => {
                let mut regions = Vec::new();
                for value in entity.attribute(2)?.as_list()? {
                    let member = self.parsed().follow(Some(value))?;
                    regions.extend(self.profile(member, depth + 1)?);
                }
                (!regions.is_empty()).then_some(regions)
            }
            _ => None,
        }
    }
}

/// One closed region of a profile: an outer ring and the rings cut out of it.
pub struct Region {
    pub outer: Vec<Vec3>,
    pub holes: Vec<Vec<Vec3>>,
}

fn rectangle(position: &Affine, width: f64, height: f64) -> Vec<Vec3> {
    // A rectangle profile is centred on its position's origin.
    let (half_width, half_height) = (width / 2.0, height / 2.0);
    [
        [-half_width, -half_height, 0.0],
        [half_width, -half_height, 0.0],
        [half_width, half_height, 0.0],
        [-half_width, half_height, 0.0],
    ]
    .into_iter()
    .map(|at| position.point(at))
    .collect()
}

fn circle(position: &Affine, radius: f64, tolerance: f64) -> Vec<Vec3> {
    let steps = arc_steps(radius, std::f64::consts::TAU, tolerance).max(3);
    #[allow(clippy::cast_precision_loss)]
    (0..steps)
        .map(|step| {
            let angle = std::f64::consts::TAU * (step as f64 / steps as f64);
            position.point([radius * angle.cos(), radius * angle.sin(), 0.0])
        })
        .collect()
}

/// Twice the signed area of a ring in the plane `x`, `y` spans.
fn signed_area2(ring: &[Vec3], x: Vec3, y: Vec3) -> f64 {
    let mut total = 0.0;
    for index in 0..ring.len() {
        let current = ring[index];
        let next = ring[(index + 1) % ring.len()];
        total += dot(current, x).mul_add(dot(next, y), -(dot(next, x) * dot(current, y)));
    }
    total
}

/// The largest distance between any two of a ring's points, used only to pick
/// the widest of several rings.
fn ring_extent(ring: &[Vec3]) -> f64 {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for at in ring {
        for axis in 0..3 {
            min[axis] = min[axis].min(at[axis]);
            max[axis] = max[axis].max(at[axis]);
        }
    }
    length(subtract(max, min))
}

/// The plane a ring lies in, as an outward normal by Newell's method.
fn ring_normal(ring: &[Vec3]) -> Option<Vec3> {
    let mut normal = [0.0; 3];
    for index in 0..ring.len() {
        let current = ring[index];
        let next = ring[(index + 1) % ring.len()];
        normal[0] += (current[1] - next[1]) * (current[2] + next[2]);
        normal[1] += (current[2] - next[2]) * (current[0] + next[0]);
        normal[2] += (current[0] - next[0]) * (current[1] + next[1]);
    }
    normalize(normal)
}

/// One planar face, whose outward normal is the one `outer`'s own winding
/// gives it.
#[must_use]
pub fn planar_face(outer: &[Vec3], holes: &[Vec<Vec3>]) -> Option<BimBrepFace> {
    let outer = tidy(outer);
    if outer.len() < 3 {
        return None;
    }
    let normal = ring_normal(&outer)?;
    // Any direction in the plane will do for its first parameter; the longest
    // edge keeps the frame well conditioned.
    let along = (0..outer.len())
        .map(|index| subtract(outer[(index + 1) % outer.len()], outer[index]))
        .max_by(|left, right| {
            length(*left)
                .partial_cmp(&length(*right))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
    let x = normalize(subtract(along, scale(normal, dot(along, normal))))?;
    let y = cross(normal, x);
    let mut loops = vec![ring_edges(&outer)];
    for hole in holes {
        let hole = tidy(hole);
        if hole.len() >= 3 {
            loops.push(ring_edges(&hole));
        }
    }
    Some(BimBrepFace {
        surface: BimBrepSurface::Plane {
            origin: point3(outer[0]),
            x_axis: x,
            y_axis: y,
        },
        loops,
    })
}

/// Drop the repeated points a file states, which a plane fit cannot use.
fn tidy(ring: &[Vec3]) -> Vec<Vec3> {
    let mut out: Vec<Vec3> = Vec::with_capacity(ring.len());
    for at in ring {
        if out.last().is_none_or(|last| !close_enough(*last, *at)) {
            out.push(*at);
        }
    }
    while out.len() > 1 && close_enough(out[0], out[out.len() - 1]) {
        out.pop();
    }
    out
}

fn ring_edges(ring: &[Vec3]) -> Vec<BimBrepEdge> {
    (0..ring.len())
        .map(|index| BimBrepEdge {
            start: point3(ring[index]),
            end: point3(ring[(index + 1) % ring.len()]),
            curve: BimBrepCurve::Line,
        })
        .collect()
}

/// The solid a region sweeps out along `sweep`, in the coordinates `into_world`
/// maps the region's plane into.
#[must_use]
pub fn prism(region: &Region, sweep: Vec3, into_world: &Affine) -> Option<BimBrep> {
    let mut outer = tidy(&region.outer);
    if outer.len() < 3 {
        return None;
    }
    // The rings are stated in the profile's plane, so the sweep's own side of
    // that plane decides which cap faces out.
    let mut holes: Vec<Vec<Vec3>> = region.holes.iter().map(|hole| tidy(hole)).collect();
    holes.retain(|hole| hole.len() >= 3);
    let plane = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    if signed_area2(&outer, plane[0], plane[1]) < 0.0 {
        outer.reverse();
    }
    for hole in &mut holes {
        // A hole runs against the ring it is cut from, which is what makes the
        // wall it sweeps out face into the hole.
        if signed_area2(hole, plane[0], plane[1]) > 0.0 {
            hole.reverse();
        }
    }
    let upward = sweep[2] >= 0.0;
    let raise = |at: Vec3| add(at, sweep);
    let mut faces = Vec::with_capacity(2 + outer.len() + holes.iter().map(Vec::len).sum::<usize>());

    // The two caps. The one the sweep runs toward keeps the rings' own
    // winding; the one it runs from is the same rings reversed.
    let cap = |ring: &[Vec3], lifted: bool, reversed: bool| -> Vec<Vec3> {
        let mut points: Vec<Vec3> = ring
            .iter()
            .map(|at| into_world.point(if lifted { raise(*at) } else { *at }))
            .collect();
        if reversed {
            points.reverse();
        }
        points
    };
    let far_holes: Vec<Vec<Vec3>> = holes.iter().map(|hole| cap(hole, true, !upward)).collect();
    let near_holes: Vec<Vec<Vec3>> = holes.iter().map(|hole| cap(hole, false, upward)).collect();
    faces.push(planar_face(&cap(&outer, true, !upward), &far_holes)?);
    faces.push(planar_face(&cap(&outer, false, upward), &near_holes)?);

    // One wall per edge of every ring, wound so that it faces the way the
    // ring it came from does.
    for ring in std::iter::once(&outer).chain(holes.iter()) {
        for index in 0..ring.len() {
            let from = ring[index];
            let to = ring[(index + 1) % ring.len()];
            let mut wall = vec![
                into_world.point(from),
                into_world.point(to),
                into_world.point(raise(to)),
                into_world.point(raise(from)),
            ];
            if !upward {
                wall.reverse();
            }
            if let Some(face) = planar_face(&wall, &[]) {
                faces.push(face);
            }
        }
    }
    Some(BimBrep {
        faces,
        complete: true,
    })
}

/// A round tube along a path, as facets.
///
/// `bim-core` states a swept disk exactly only along one straight run, so a
/// path that bends is built here instead. The facet count comes from the same
/// tolerance every other curve is sampled at.
#[must_use]
pub fn tube(path: &[Vec3], radius: f64, tolerance: f64) -> Option<BimBrep> {
    let mut stations: Vec<Vec3> = Vec::with_capacity(path.len());
    for at in path {
        if stations.last().is_none_or(|last| !close_enough(*last, *at)) {
            stations.push(*at);
        }
    }
    if stations.len() < 2 || radius <= 0.0 {
        return None;
    }
    let sides = arc_steps(radius, std::f64::consts::TAU, tolerance).max(3);
    let heading = |index: usize| -> Vec3 {
        let before = stations[index.saturating_sub(1)];
        let after = stations[(index + 1).min(stations.len() - 1)];
        normalize(subtract(after, before)).unwrap_or([0.0, 0.0, 1.0])
    };
    // One frame is carried along the path rather than rebuilt at each station,
    // so neighbouring rings line up and the tube does not twist.
    let first = heading(0);
    let seed = if first[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let mut x = normalize(subtract(seed, scale(first, dot(seed, first))))?;
    let mut rings: Vec<Vec<Vec3>> = Vec::with_capacity(stations.len());
    for (index, station) in stations.iter().enumerate() {
        let along = heading(index);
        // Carry the reference direction across the bend, then square it up.
        x = normalize(subtract(x, scale(along, dot(x, along)))).unwrap_or(x);
        let y = cross(along, x);
        #[allow(clippy::cast_precision_loss)]
        rings.push(
            (0..sides)
                .map(|side| {
                    let angle = std::f64::consts::TAU * (side as f64 / sides as f64);
                    add(
                        *station,
                        add(
                            scale(x, radius * angle.cos()),
                            scale(y, radius * angle.sin()),
                        ),
                    )
                })
                .collect(),
        );
    }
    let mut faces = Vec::with_capacity(rings.len() * sides + 2);
    for pair in rings.windows(2) {
        for side in 0..sides {
            let next = (side + 1) % sides;
            let wall = [pair[0][side], pair[0][next], pair[1][next], pair[1][side]];
            if let Some(face) = planar_face(&wall, &[]) {
                faces.push(face);
            }
        }
    }
    // The caps close the tube, each wound away from the run.
    let mut start: Vec<Vec3> = rings[0].clone();
    start.reverse();
    faces.push(planar_face(&start, &[])?);
    faces.push(planar_face(&rings[rings.len() - 1], &[])?);
    Some(BimBrep {
        faces,
        complete: true,
    })
}
