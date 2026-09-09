#![forbid(unsafe_code)]

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BimModel {
    /// Application/release that supplied the data, retained for provenance.
    pub source: Option<BimSource>,
    pub elements: Vec<BimElement>,
    pub levels: Vec<BimLevel>,
    pub relations: Vec<BimRelation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimSource {
    pub application: String,
    pub release: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimElement {
    pub id: BimElementId,
    /// Format-neutral semantic type. `Unknown` preserves elements that have
    /// not yet been classified without forcing an exporter-specific guess.
    pub element_type: BimElementType,
    /// Format-neutral source class, such as `Wall` or `FamilyInstance`.
    pub class_name: Option<String>,
    pub name: Option<String>,
    /// A descriptive name where `name` carries the identifying one: a room is
    /// named by its number and called something else. IFC's `LongName`.
    pub long_name: Option<String>,
    pub category: Option<BimCategory>,
    pub level_id: Option<BimElementId>,
    pub type_id: Option<BimElementId>,
    pub placement: Option<BimPlacement>,
    pub geometry: Option<BimGeometry>,
    pub properties: Vec<BimProperty>,
    /// Properties the element's type carries rather than the element itself.
    /// Kept apart from `properties` because they are read from another record
    /// and hold for every element of that type, which a consumer that merged
    /// the two could no longer tell.
    pub type_properties: Vec<BimProperty>,
    /// What the element is made of, layer by layer, when its type declares a
    /// layered build-up. A wall, floor, roof or ceiling has one; a component
    /// does not.
    pub material_layers: Option<BimMaterialLayerSet>,
}

/// A layered build-up, in order from one face to the other. The order is the
/// source's own and carries the geometry: layer 0 is against one face of the
/// host and the last is against the other.
#[derive(Clone, Debug, PartialEq)]
pub struct BimMaterialLayerSet {
    /// The type the layers were read from, when the element is not itself that
    /// type. Two elements sharing this identifier share the build-up.
    pub source_type_id: Option<BimElementId>,
    pub name: Option<String>,
    pub layers: Vec<BimMaterialLayer>,
}

impl BimMaterialLayerSet {
    /// Total thickness, or `None` when the layers do not agree on a unit.
    #[must_use]
    pub fn total_thickness(&self) -> Option<BimNumber> {
        let unit = self.layers.first()?.thickness.unit.clone();
        self.layers
            .iter()
            .all(|layer| layer.thickness.unit == unit)
            .then(|| BimNumber {
                value: self.layers.iter().map(|layer| layer.thickness.value).sum(),
                unit,
            })
    }
}

/// One layer of a [`BimMaterialLayerSet`].
#[derive(Clone, Debug, PartialEq)]
pub struct BimMaterialLayer {
    pub material: Option<BimMaterial>,
    /// Zero for a membrane, which is a real layer with no thickness.
    pub thickness: BimNumber,
    /// Whether the layer lies inside the build-up's structural core rather
    /// than in the shell on either side of it.
    pub is_core: bool,
    /// Whether the type names this layer as the one its structural material
    /// comes from.
    pub is_structural: bool,
    /// The source's own layer-function code, unlabelled. Kept because the
    /// source carries it and discarding it would lose data, but no meaning is
    /// attached to the number here: nothing has established what its values
    /// mean, so a consumer must not render it as a function name.
    pub source_function: Option<i64>,
}

/// A material as the source names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimMaterial {
    pub id: Option<BimExternalId>,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum BimElementType {
    #[default]
    Unknown,
    PipeSegment,
    PipeFitting,
    SanitaryTerminal,
    AirTerminal,
    FireSuppressionTerminal,
    Alarm,
    CableCarrierFitting,
    /// A run of a building system, named by the category its own type
    /// declares: a pipe, a duct, or a conduit or cable tray carrying cable.
    /// No reference export measures these - the corpus has none for a plumbing
    /// or electrical model - so what stands behind them is the category the
    /// element's type declares and the geometry the run itself carries.
    DuctSegment,
    CableCarrierSegment,
    /// Source category that is certainly an MEP distribution element without
    /// naming the device. Recorded as the supertype rather than guessed.
    DistributionElement,
    /// Source category that is certainly a flow element (a valve, strainer,
    /// meter, pump or air handler) without naming which one.
    DistributionFlowElement,
    // Architectural system families. Unlike the categories above, these are
    // established by the source *class* alone: Revit's own IFC export of the
    // reference model maps each of them one-to-one, with no spread.
    Wall,
    Slab,
    Roof,
    Stair,
    StairFlight,
    // Loadable families, established by the category their family declares.
    // The same reference join fixes these: each of the categories below maps
    // to one IFC entity with no spread at all.
    Railing,
    Column,
    Member,
    Plate,
    Window,
    Door,
    /// A wall whose type is a curtain-wall type: a framed assembly rather than
    /// a layered build-up.
    CurtainWall,
    /// A place rather than a building element: it bounds volume, is part of
    /// the spatial structure and is decomposed by the storey it sits on
    /// rather than contained in it.
    Space,
}

impl BimElementType {
    /// Whether this type belongs to the spatial structure - a place the model
    /// is divided into - rather than to the elements the structure holds.
    #[must_use]
    pub fn is_spatial(self) -> bool {
        matches!(self, Self::Space)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BimGeometry {
    /// An exact source centerline without an inferred body/profile.
    AxisLine(BimLineSegment),
    SweptDisk(BimSweptDisk),
    /// An independently verified element-local extent. This is a bounding
    /// representation, not a claim about the element's body or topology.
    BoundingBox(BimBoundingBox),
    /// A decoded boundary representation. May be a partial shell: faces the
    /// source decoder could not resolve are omitted rather than guessed, so
    /// this is not always a claim of a closed, watertight solid.
    Brep(BimBrep),
    /// Several closed solids that together are one element: a nested family
    /// places one sub-instance per body, and no single one of them describes
    /// the element. Every member is a closed shell - an incomplete one is left
    /// out rather than shipped, exactly as a lone body is - so an exporter may
    /// write them as one representation of several solid items.
    Assembly(Vec<BimBrep>),
}

/// A boundary representation in world coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct BimBrep {
    pub faces: Vec<BimBrepFace>,
    /// `true` only when every face the source record declared was resolved
    /// into `faces` - i.e. this is claimed to be a closed shell, not merely
    /// whatever subset decoded cleanly. An exporter should not emit a closed
    /// solid representation when this is `false`.
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimBrepFace {
    pub surface: BimBrepSurface,
    /// The face's boundary loops: the first is the outer bound, any further
    /// loops are holes.
    pub loops: Vec<Vec<BimBrepEdge>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BimBrepSurface {
    Plane {
        origin: BimPoint3,
        x_axis: [f64; 3],
        y_axis: [f64; 3],
    },
    Cylinder {
        center: BimPoint3,
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        z_axis: [f64; 3],
        radius: BimNumber,
    },
    /// A profile curve swept about `z_axis`. The frame is in world
    /// coordinates; the profile is in the frame's own, which is where the
    /// source keeps it.
    Revolution {
        center: BimPoint3,
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        z_axis: [f64; 3],
        profile: BimBrepProfile,
    },
    /// Two profiles joined by straight rulings:
    /// `S(u, v) = (1 - v) * first(u) + v * second(u)`, `u` running along the
    /// profiles and `v` across them. Unlike the other surfaces here it
    /// carries no frame of its own: the source states both profiles in the
    /// body's coordinates, so both are placed in world coordinates like any
    /// other point.
    Ruled {
        first: BimBrepRuling,
        second: BimBrepRuling,
    },
}

/// One side of a [`BimBrepSurface::Ruled`]: a profile curve in world
/// coordinates, or the point a degenerate profile collapses to.
#[derive(Clone, Debug, PartialEq)]
pub enum BimBrepRuling {
    Point(BimPoint3),
    /// `profile` evaluated at `start + u * (end - start)` for `u` in [0, 1].
    /// The interval is an angle for an arc and a length in metres for a line,
    /// matching what the profile's own numbers mean.
    Curve {
        profile: BimBrepProfile,
        start: f64,
        end: f64,
    },
}

/// The curve a [`BimBrepSurface::Revolution`] turns, in that surface's frame:
/// a line gives a cone, an arc a torus, and an arc centred on the axis a
/// sphere.
#[derive(Clone, Debug, PartialEq)]
pub enum BimBrepProfile {
    Line {
        origin: BimPoint3,
        direction: [f64; 3],
    },
    Arc {
        center: BimPoint3,
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        radius: BimNumber,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimBrepEdge {
    pub start: BimPoint3,
    pub end: BimPoint3,
    pub curve: BimBrepCurve,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BimBrepCurve {
    Line,
    Arc(BimBrepArc),
    /// A sampled source curve, including its two topological endpoints.
    Polyline(Vec<BimPoint3>),
}

/// `point(angle) = center + radius * (cos(angle) * x_axis + sin(angle) *
/// cross(z_axis, x_axis))`; the edge runs from `start_angle` to `end_angle`
/// in that formula's increasing-angle direction whenever `end_angle >=
/// start_angle`.
#[derive(Clone, Debug, PartialEq)]
pub struct BimBrepArc {
    pub center: BimPoint3,
    pub x_axis: [f64; 3],
    pub z_axis: [f64; 3],
    pub radius: BimNumber,
    pub start_angle: f64,
    pub end_angle: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimBoundingBox {
    pub min: BimPoint3,
    pub max: BimPoint3,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimSweptDisk {
    pub directrix: BimLineSegment,
    pub radius: BimNumber,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimLineSegment {
    pub start: BimPoint3,
    pub end: BimPoint3,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimPoint3 {
    pub coordinates: [f64; 3],
    pub unit: BimUnit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimPlacement {
    /// Placement origin in source/world coordinates.
    pub origin: BimPoint3,
    /// Local X direction expressed in the same world coordinate system.
    pub reference_direction: [f64; 3],
    /// Local Z direction expressed in the same world coordinate system.
    pub axis: [f64; 3],
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BimElementId(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimCategory {
    pub id: Option<BimExternalId>,
    pub name: String,
}

/// Identifier in a source-system namespace. Keeping the namespace prevents a
/// Revit enum value from being confused with an IFC classification code.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BimExternalId {
    pub system: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimProperty {
    pub id: Option<BimExternalId>,
    pub name: String,
    /// Forge spec or another source-independent quantity-kind identifier.
    pub specification: Option<String>,
    pub value: BimPropertyValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BimPropertyValue {
    Bool(bool),
    Integer(i64),
    Number(BimNumber),
    Text(String),
    Reference(BimElementId),
    Bytes(Vec<u8>),
    Unknown(Vec<u8>),
}

/// A number paired with the unit in which `value` is expressed. `None` means
/// the dimension has not been established; it does not mean unitless.
#[derive(Clone, Debug, PartialEq)]
pub struct BimNumber {
    pub value: f64,
    pub unit: Option<BimUnit>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BimUnit {
    /// Stable external identifier, for example a Forge unit type ID.
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimRelation {
    pub kind: String,
    pub source: BimElementId,
    pub target: BimElementId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimLevel {
    pub id: BimElementId,
    pub name: Option<String>,
    /// Elevation in a canonical, explicitly named unit.
    pub elevation: Option<BimNumber>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_unit_is_distinct_from_a_unitless_quantity() {
        let unknown = BimNumber {
            value: 12.0,
            unit: None,
        };
        let unitless = BimNumber {
            value: 12.0,
            unit: Some(BimUnit {
                id: "autodesk.unit.unit:general-1.0.1".to_owned(),
                name: "General".to_owned(),
            }),
        };
        assert_ne!(unknown, unitless);
    }
}
