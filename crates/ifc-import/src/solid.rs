//! Turning the representation items a file states into `bim-core` bodies.
//!
//! Everything here ends as planar faces or a swept disk, in world coordinates
//! and in metres. An item form this does not read is counted in
//! [`Bodies::unread_items`] and contributes nothing, so an element is never
//! given a shape the file did not state. The one place that rule bends is a
//! boolean result, which is reported separately in [`Bodies::approximated`]:
//! the cut solid is drawn, the cut is not.

use bim_core::{
    BimBoundingBox, BimBrep, BimBrepArc, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepProfile,
    BimBrepSurface, BimGeometry, BimLineSegment, BimNumber, BimPoint3, BimSweptDisk, BimUnit,
};

use crate::curve::{Sampler, arc_steps, close_enough};
use crate::place::{
    Affine, IDENTITY, Vec3, add, axis_placement, cartesian_point, coordinates, cross, direction,
    dot, length, normalize, scale, subtract, transformation_operator,
};
use crate::step::{Entity, Parsed, Value};

#[must_use]
pub fn metres() -> BimUnit {
    // Shared, not built: this is called for every coordinate of every body.
    BimUnit::metres()
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
        if entity.type_name == "IFCADVANCEDFACE" {
            return self.advanced_face(entity, place);
        }
        if entity.type_name != "IFCFACE" {
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

    /// An `IfcAdvancedFace`: exact surface plus topological edge loops.
    ///
    /// Unlike a faceted face, its bounds are `IfcEdgeLoop`s and may contain
    /// arcs. The two orientation flags on an edge are composed here, and the
    /// face-bound flag is applied once to the completed loop.
    fn advanced_face(&self, entity: &Entity, place: &Affine) -> Option<BimBrepFace> {
        let same_sense = truth(entity.attribute(2)?)?;
        let surface = self
            .parsed()
            .follow(entity.attribute(1))
            .and_then(|surface| self.advanced_surface(surface, place, same_sense))?;
        let mut outer: Option<Vec<BimBrepEdge>> = None;
        let mut inner = Vec::new();
        for member in entity.attribute(0)?.as_list()? {
            let bound = self.parsed().follow(Some(member))?;
            let loop_entity = self.parsed().follow(bound.attribute(0))?;
            let mut edges = self.edge_loop(loop_entity, place)?;
            if !truth(bound.attribute(1)?)? {
                reverse_loop(&mut edges);
            }
            if bound.type_name == "IFCFACEOUTERBOUND" && outer.is_none() {
                outer = Some(edges);
            } else {
                inner.push(edges);
            }
        }
        // `IfcFace` permits bounds without naming one `IfcFaceOuterBound`.
        // Preserve that valid form; the mesher independently picks the loop
        // with the widest parameter-space extent as the outer boundary.
        let outer = outer.or_else(|| (!inner.is_empty()).then(|| inner.remove(0)))?;
        let mut loops = Vec::with_capacity(inner.len() + 1);
        loops.push(outer);
        loops.extend(inner);
        Some(BimBrepFace { surface, loops })
    }

    /// The analytic surface an advanced face lies on, in world coordinates.
    fn advanced_surface(
        &self,
        entity: &Entity,
        place: &Affine,
        same_sense: bool,
    ) -> Option<BimBrepSurface> {
        match entity.type_name.as_str() {
            "IFCPLANE" => {
                let frame = self.placed_axis(entity.attribute(0), place)?;
                let x = normalize(frame.basis[0])?;
                let mut y = normalize(frame.basis[1])?;
                if !same_sense {
                    y = scale(y, -1.0);
                }
                Some(BimBrepSurface::Plane {
                    origin: point3(frame.origin),
                    x_axis: x,
                    y_axis: y,
                })
            }
            "IFCCYLINDRICALSURFACE" => {
                let frame = self.placed_axis(entity.attribute(0), place)?;
                let factor = frame.uniform_scale()?;
                let radius = entity.attribute(1)?.as_number()? * self.sampler.units.length * factor;
                if !(radius.is_finite() && radius > 0.0) {
                    return None;
                }
                let x = normalize(frame.basis[0])?;
                let mut y = normalize(frame.basis[1])?;
                if !same_sense {
                    y = scale(y, -1.0);
                }
                Some(BimBrepSurface::Cylinder {
                    center: point3(frame.origin),
                    x_axis: x,
                    y_axis: y,
                    z_axis: normalize(frame.basis[2])?,
                    radius: number_in_metres(radius),
                })
            }
            "IFCSPHERICALSURFACE" | "IFCTOROIDALSURFACE" => {
                let frame = self.placed_axis(entity.attribute(0), place)?;
                let factor = frame.uniform_scale()?;
                let (major, minor) = if entity.type_name == "IFCSPHERICALSURFACE" {
                    (0.0, entity.attribute(1)?.as_number()?)
                } else {
                    (
                        entity.attribute(1)?.as_number()?,
                        entity.attribute(2)?.as_number()?,
                    )
                };
                let major = major * self.sampler.units.length * factor;
                let minor = minor * self.sampler.units.length * factor;
                if !(major.is_finite() && minor.is_finite() && minor > 0.0 && major >= 0.0) {
                    return None;
                }
                let x = normalize(frame.basis[0])?;
                let mut y = normalize(frame.basis[1])?;
                if !same_sense {
                    y = scale(y, -1.0);
                }
                Some(BimBrepSurface::Revolution {
                    center: point3(frame.origin),
                    x_axis: x,
                    y_axis: y,
                    z_axis: normalize(frame.basis[2])?,
                    profile: BimBrepProfile::Arc {
                        center: point3([major, 0.0, 0.0]),
                        x_axis: [1.0, 0.0, 0.0],
                        y_axis: [0.0, 0.0, 1.0],
                        radius: number_in_metres(minor),
                    },
                })
            }
            "IFCSURFACEOFREVOLUTION" => self.surface_of_revolution(entity, place, same_sense),
            _ => None,
        }
    }

    fn placed_axis(&self, value: Option<&Value>, place: &Affine) -> Option<Affine> {
        let local = self
            .parsed()
            .follow(value)
            .and_then(|axis| axis_placement(self.parsed(), axis, &self.sampler.units))?;
        Some(place.then(&local))
    }

    /// `IfcSurfaceOfRevolution`, whose profile is stated in `Position` and
    /// whose revolution axis is independently stated by `AxisPosition`.
    fn surface_of_revolution(
        &self,
        entity: &Entity,
        place: &Affine,
        same_sense: bool,
    ) -> Option<BimBrepSurface> {
        // A non-conformal outer placement turns circles of revolution into
        // shapes the canonical analytic surface cannot state.
        place.uniform_scale()?;
        let position = self.placed_axis(entity.attribute(1), place)?;
        let axis = self.parsed().follow(entity.attribute(2))?;
        if axis.type_name != "IFCAXIS1PLACEMENT" {
            return None;
        }
        let center = self
            .parsed()
            .follow(axis.attribute(0))
            .and_then(cartesian_point)
            .map(|point| place.point(scale(point, self.sampler.units.length)))?;
        let z = self
            .parsed()
            .follow(axis.attribute(1))
            .and_then(direction)
            .map(|axis| place.direction(axis))
            .and_then(normalize)?;
        // Position's first direction is the radial zero. Square it to the
        // separately stated axis: the schema requires that relation, but
        // doing it explicitly prevents numeric drift from skewing the frame.
        let radial = position.basis[0];
        let x = normalize(subtract(radial, scale(z, dot(radial, z))))?;
        let mut y = cross(z, x);
        let profile_entity = self.parsed().follow(entity.attribute(0))?;
        let profile = self.revolved_profile(profile_entity, &position, center, [x, y, z])?;
        if !same_sense {
            y = scale(y, -1.0);
        }
        Some(BimBrepSurface::Revolution {
            center: point3(center),
            x_axis: x,
            y_axis: y,
            z_axis: z,
            profile,
        })
    }

    fn revolved_profile(
        &self,
        profile: &Entity,
        position: &Affine,
        center: Vec3,
        axes: [Vec3; 3],
    ) -> Option<BimBrepProfile> {
        if profile.type_name != "IFCARBITRARYOPENPROFILEDEF" {
            return None;
        }
        let curve = self.parsed().follow(profile.attribute(2))?;
        let basis = if curve.type_name == "IFCTRIMMEDCURVE" {
            self.parsed().follow(curve.attribute(0))?
        } else {
            curve
        };
        if basis.type_name == "IFCCIRCLE" {
            return self.revolved_circle_profile(basis, position, center, axes);
        }
        let sampled = self.sampler.curve(curve)?;
        let first = position.point(*sampled.first()?);
        let last = position.point(*sampled.last()?);
        let local_first = local_coordinates(first, center, axes);
        let local_last = local_coordinates(last, center, axes);
        Some(BimBrepProfile::Line {
            origin: point3(local_first),
            direction: normalize(subtract(local_last, local_first))?,
        })
    }

    fn revolved_circle_profile(
        &self,
        circle: &Entity,
        position: &Affine,
        center: Vec3,
        axes: [Vec3; 3],
    ) -> Option<BimBrepProfile> {
        let circle_axis = self
            .parsed()
            .follow(circle.attribute(0))
            .and_then(|axis| axis_placement(self.parsed(), axis, &self.sampler.units))?;
        let world = position.then(&circle_axis);
        let factor = world.uniform_scale()?;
        let radius = circle.attribute(1)?.as_number()? * self.sampler.units.length * factor;
        if !(radius.is_finite() && radius > 0.0) {
            return None;
        }
        Some(BimBrepProfile::Arc {
            center: point3(local_coordinates(world.origin, center, axes)),
            x_axis: local_direction(normalize(world.basis[0])?, axes),
            y_axis: local_direction(normalize(world.basis[1])?, axes),
            radius: number_in_metres(radius),
        })
    }

    fn edge_loop(&self, entity: &Entity, place: &Affine) -> Option<Vec<BimBrepEdge>> {
        if entity.type_name != "IFCEDGELOOP" {
            return None;
        }
        let edges = entity
            .attribute(0)?
            .as_list()?
            .iter()
            .map(|value| {
                self.parsed()
                    .follow(Some(value))
                    .and_then(|edge| self.oriented_edge(edge, place))
            })
            .collect::<Option<Vec<_>>>()?;
        (!edges.is_empty()).then_some(edges)
    }

    fn oriented_edge(&self, entity: &Entity, place: &Affine) -> Option<BimBrepEdge> {
        if entity.type_name != "IFCORIENTEDEDGE" {
            return None;
        }
        let orientation = truth(entity.attribute(3)?)?;
        let edge = self.parsed().follow(entity.attribute(2))?;
        if edge.type_name != "IFCEDGECURVE" {
            return None;
        }
        let same_sense = truth(edge.attribute(3)?)?;
        let first = self.vertex(edge.attribute(0), place)?;
        let second = self.vertex(edge.attribute(1), place)?;
        let (start, end) = if orientation {
            (first, second)
        } else {
            (second, first)
        };
        let geometry = self.parsed().follow(edge.attribute(2))?;
        let curve = self.edge_curve(geometry, start, end, same_sense == orientation, place)?;
        Some(BimBrepEdge {
            start: point3(start),
            end: point3(end),
            curve,
        })
    }

    fn vertex(&self, value: Option<&Value>, place: &Affine) -> Option<Vec3> {
        let vertex = self.parsed().follow(value)?;
        if vertex.type_name != "IFCVERTEXPOINT" {
            return None;
        }
        let point = self
            .parsed()
            .follow(vertex.attribute(0))
            .and_then(cartesian_point)?;
        Some(place.point(scale(point, self.sampler.units.length)))
    }

    fn edge_curve(
        &self,
        geometry: &Entity,
        start: Vec3,
        end: Vec3,
        forward: bool,
        place: &Affine,
    ) -> Option<BimBrepCurve> {
        match geometry.type_name.as_str() {
            "IFCLINE" => Some(BimBrepCurve::Line),
            "IFCCIRCLE" => self.circle_edge(geometry, start, end, forward, place),
            _ => {
                let mut points: Vec<Vec3> = self
                    .sampler
                    .curve(geometry)?
                    .into_iter()
                    .map(|point| place.point(point))
                    .collect();
                if !forward {
                    points.reverse();
                }
                if points.len() < 2 {
                    return None;
                }
                points[0] = start;
                let last = points.len() - 1;
                points[last] = end;
                if points.len() == 2 {
                    Some(BimBrepCurve::Line)
                } else {
                    Some(BimBrepCurve::Polyline(
                        points.into_iter().map(point3).collect(),
                    ))
                }
            }
        }
    }

    fn circle_edge(
        &self,
        circle: &Entity,
        start: Vec3,
        end: Vec3,
        mut forward: bool,
        place: &Affine,
    ) -> Option<BimBrepCurve> {
        let frame = self.placed_axis(circle.attribute(0), place)?;
        let factor = frame.uniform_scale()?;
        let radius = circle.attribute(1)?.as_number()? * self.sampler.units.length * factor;
        if !(radius.is_finite() && radius > 0.0) {
            return None;
        }
        let x = normalize(frame.basis[0])?;
        let z = normalize(frame.basis[2])?;
        let y = cross(z, x);
        // A reflected conformal placement changes the source circle's
        // increasing parameter direction relative to `cross(z, x)`.
        if dot(y, normalize(frame.basis[1])?) < 0.0 {
            forward = !forward;
        }
        let angle_of = |point: Vec3| {
            let local = subtract(point, frame.origin);
            dot(local, y).atan2(dot(local, x))
        };
        let first = angle_of(start);
        let mut last = angle_of(end);
        if forward {
            while last <= first + f64::EPSILON {
                last += std::f64::consts::TAU;
            }
        } else {
            while last >= first - f64::EPSILON {
                last -= std::f64::consts::TAU;
            }
        }
        Some(BimBrepCurve::Arc(BimBrepArc {
            center: point3(frame.origin),
            x_axis: x,
            z_axis: z,
            radius: number_in_metres(radius),
            start_angle: first,
            end_angle: last,
        }))
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

fn truth(value: &Value) -> Option<bool> {
    match value.as_enumeration()? {
        "T" => Some(true),
        "F" => Some(false),
        _ => None,
    }
}

fn number_in_metres(value: f64) -> BimNumber {
    BimNumber {
        value,
        unit: Some(metres()),
    }
}

/// A world point written in an orthonormal frame's coordinates.
fn local_coordinates(point: Vec3, origin: Vec3, axes: [Vec3; 3]) -> Vec3 {
    let delta = subtract(point, origin);
    [
        dot(delta, axes[0]),
        dot(delta, axes[1]),
        dot(delta, axes[2]),
    ]
}

/// A world direction written in an orthonormal frame's coordinates.
fn local_direction(direction: Vec3, axes: [Vec3; 3]) -> Vec3 {
    [
        dot(direction, axes[0]),
        dot(direction, axes[1]),
        dot(direction, axes[2]),
    ]
}

fn reverse_loop(edges: &mut [BimBrepEdge]) {
    edges.reverse();
    for edge in edges {
        std::mem::swap(&mut edge.start, &mut edge.end);
        match &mut edge.curve {
            BimBrepCurve::Line => {}
            BimBrepCurve::Arc(arc) => {
                std::mem::swap(&mut arc.start_angle, &mut arc.end_angle);
            }
            BimBrepCurve::Polyline(points) => points.reverse(),
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

#[cfg(test)]
mod tests {
    use super::{Builder, IDENTITY};
    use crate::curve::Sampler;
    use crate::step::parse;
    use crate::units::Units;
    use bim_core::{BimBrepCurve, BimBrepProfile, BimBrepSurface};

    fn file(data: &str) -> crate::step::Parsed {
        let text = format!("ISO-10303-21;\nDATA;\n{data}ENDSEC;\nEND-ISO-10303-21;\n");
        parse(text.as_bytes()).expect("a STEP file")
    }

    /// A number the file states arrives unscaled, so these comparisons are
    /// about an exact value; they are still written to a tolerance, because a
    /// test that pins a float with `==` breaks the moment a metre conversion
    /// is introduced between the two sides.
    #[track_caller]
    fn same(found: f64, stated: f64) {
        assert!(
            (found - stated).abs() < 1e-12,
            "expected {stated}, found {found}"
        );
    }

    fn builder(parsed: &crate::step::Parsed) -> Builder<'_> {
        Builder::new(Sampler {
            parsed,
            units: Units::default(),
            tolerance: 0.004,
        })
    }

    #[test]
    fn reads_an_advanced_plane_with_its_edge_loop() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #2=IFCDIRECTION((0.,0.,1.));\n\
             #3=IFCDIRECTION((1.,0.,0.));\n\
             #4=IFCAXIS2PLACEMENT3D(#1,#2,#3);\n\
             #5=IFCPLANE(#4);\n\
             #10=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #11=IFCCARTESIANPOINT((2.,0.,0.));\n\
             #12=IFCCARTESIANPOINT((2.,1.,0.));\n\
             #13=IFCCARTESIANPOINT((0.,1.,0.));\n\
             #14=IFCVERTEXPOINT(#10);\n\
             #15=IFCVERTEXPOINT(#11);\n\
             #16=IFCVERTEXPOINT(#12);\n\
             #17=IFCVERTEXPOINT(#13);\n\
             #20=IFCLINE($,$);\n\
             #21=IFCEDGECURVE(#14,#15,#20,.T.);\n\
             #22=IFCEDGECURVE(#15,#16,#20,.T.);\n\
             #23=IFCEDGECURVE(#16,#17,#20,.T.);\n\
             #24=IFCEDGECURVE(#17,#14,#20,.T.);\n\
             #31=IFCORIENTEDEDGE(*,*,#21,.T.);\n\
             #32=IFCORIENTEDEDGE(*,*,#22,.T.);\n\
             #33=IFCORIENTEDEDGE(*,*,#23,.T.);\n\
             #34=IFCORIENTEDEDGE(*,*,#24,.T.);\n\
             #40=IFCEDGELOOP((#31,#32,#33,#34));\n\
             #41=IFCFACEOUTERBOUND(#40,.T.);\n\
             #42=IFCADVANCEDFACE((#41),#5,.T.);\n\
             #43=IFCCLOSEDSHELL((#42));\n\
             #44=IFCADVANCEDBREP(#43);\n",
        );
        let mut builder = builder(&parsed);
        builder.item(parsed.get(44).expect("advanced brep"), &IDENTITY, 0);
        assert_eq!(builder.bodies.unread_items, 0);
        assert_eq!(builder.bodies.shells.len(), 1);
        let shell = &builder.bodies.shells[0];
        assert!(shell.complete);
        assert_eq!(shell.faces.len(), 1);
        assert_eq!(shell.faces[0].loops[0].len(), 4);
        assert!(matches!(
            shell.faces[0].surface,
            BimBrepSurface::Plane { .. }
        ));
    }

    #[test]
    fn composes_edge_sense_and_recovers_a_full_circle_from_equal_vertices() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #2=IFCDIRECTION((0.,0.,1.));\n\
             #3=IFCDIRECTION((1.,0.,0.));\n\
             #4=IFCAXIS2PLACEMENT3D(#1,#2,#3);\n\
             #5=IFCCIRCLE(#4,1.);\n\
             #6=IFCCARTESIANPOINT((1.,0.,0.));\n\
             #7=IFCVERTEXPOINT(#6);\n\
             #8=IFCEDGECURVE(#7,#7,#5,.T.);\n\
             #9=IFCORIENTEDEDGE(*,*,#8,.F.);\n",
        );
        let edge = builder(&parsed)
            .oriented_edge(parsed.get(9).expect("oriented edge"), &IDENTITY)
            .expect("a circle edge");
        let BimBrepCurve::Arc(arc) = edge.curve else {
            panic!("the circle must stay analytic");
        };
        assert!((arc.end_angle - arc.start_angle + std::f64::consts::TAU).abs() < 1e-12);
    }

    #[test]
    fn reads_cylinder_sphere_and_torus_surfaces_without_faceting_them() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((1.,2.,3.));\n\
             #2=IFCDIRECTION((0.,0.,1.));\n\
             #3=IFCDIRECTION((1.,0.,0.));\n\
             #4=IFCAXIS2PLACEMENT3D(#1,#2,#3);\n\
             #5=IFCCYLINDRICALSURFACE(#4,2.);\n\
             #6=IFCSPHERICALSURFACE(#4,3.);\n\
             #7=IFCTOROIDALSURFACE(#4,5.,1.);\n",
        );
        let builder = builder(&parsed);
        let cylinder = builder
            .advanced_surface(parsed.get(5).expect("cylinder"), &IDENTITY, true)
            .expect("cylinder surface");
        let BimBrepSurface::Cylinder { radius, .. } = cylinder else {
            panic!("cylinder");
        };
        same(radius.value, 2.0);

        for (id, major, minor) in [(6, 0.0, 3.0), (7, 5.0, 1.0)] {
            let surface = builder
                .advanced_surface(parsed.get(id).expect("surface"), &IDENTITY, true)
                .expect("surface of revolution");
            let BimBrepSurface::Revolution { profile, .. } = surface else {
                panic!("revolution");
            };
            let BimBrepProfile::Arc { center, radius, .. } = profile else {
                panic!("arc profile");
            };
            same(center.coordinates[0], major);
            same(radius.value, minor);
        }
    }

    #[test]
    fn reads_the_line_profile_of_a_surface_of_revolution() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #2=IFCDIRECTION((0.,-1.,0.));\n\
             #3=IFCDIRECTION((1.,0.,0.));\n\
             #4=IFCAXIS2PLACEMENT3D(#1,#2,#3);\n\
             #5=IFCDIRECTION((0.,0.,1.));\n\
             #6=IFCAXIS1PLACEMENT(#1,#5);\n\
             #7=IFCCARTESIANPOINT((2.,3.));\n\
             #8=IFCCARTESIANPOINT((4.,7.));\n\
             #9=IFCPOLYLINE((#7,#8));\n\
             #10=IFCARBITRARYOPENPROFILEDEF(.CURVE.,$,#9);\n\
             #11=IFCSURFACEOFREVOLUTION(#10,#4,#6);\n",
        );
        let surface = builder(&parsed)
            .advanced_surface(parsed.get(11).expect("surface"), &IDENTITY, true)
            .expect("surface of revolution");
        let BimBrepSurface::Revolution { profile, .. } = surface else {
            panic!("revolution");
        };
        let BimBrepProfile::Line { origin, direction } = profile else {
            panic!("line profile");
        };
        for (found, stated) in origin.coordinates.iter().zip([2.0, 0.0, 3.0]) {
            same(*found, stated);
        }
        assert!((direction[0] - 1.0 / 5.0_f64.sqrt()).abs() < 1e-12);
        assert!((direction[2] - 2.0 / 5.0_f64.sqrt()).abs() < 1e-12);
    }
}
