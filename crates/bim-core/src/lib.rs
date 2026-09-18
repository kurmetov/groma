#![forbid(unsafe_code)]

pub mod work;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BimModel {
    /// Application/release that supplied the data, retained for provenance.
    ///
    /// The shorthand for a model read from one file. A federated model leaves
    /// this `None` and states each file in [`BimModel::documents`] instead,
    /// because there is no single application that supplied it.
    pub source: Option<BimSource>,
    /// What the source file's own project information states about it.
    ///
    /// The shorthand for a model read from one file, same as [`Self::source`]:
    /// `None` for a federated model, because several files name several
    /// projects and none of them is *the* project of the assembled whole.
    pub project: Option<BimProjectIdentity>,
    /// Which file, and which save of it, this model was read from.
    ///
    /// The shorthand for a model read from one file, same as [`Self::source`]:
    /// `None` for a federated model, which states one of these per file in
    /// [`BimDocument::identity`] instead.
    pub document_identity: Option<BimDocumentIdentity>,
    /// Where the project sits, when the file states one location and every
    /// record of it agrees. `None` for a federated model, same reasoning as
    /// [`Self::project`]; also `None` for a single file whose named
    /// locations disagree - see [`BimSiteLocation`] for why that is refused
    /// rather than guessed at.
    pub site: Option<BimSiteLocation>,
    /// The frame the model is shared in, stated in the model's own
    /// coordinates: where its origin sits and how its axes are turned, as the
    /// source's active named location declares them.
    ///
    /// Every coordinate in [`Self::elements`] is in the model's own frame and
    /// stays that way. This says how to get from that frame to the one the
    /// project is shared under, which is what lets an exported file land on
    /// the same ground as another export of the same site. `None` where the
    /// source states no such location, or states an identity one - a project
    /// that was never placed.
    pub site_placement: Option<BimPlacement>,
    /// The source files this model was assembled from, one entry per file.
    ///
    /// Empty for a model a reader produced directly; [`federate`] fills it,
    /// with one entry even for a single source.
    pub documents: Vec<BimDocument>,
    pub elements: Vec<BimElement>,
    pub levels: Vec<BimLevel>,
    pub relations: Vec<BimRelation>,
    /// A space's own face, matched against the one real element whose own
    /// face coincides with it. Empty until [`compute_space_boundaries`] is
    /// run over [`Self::elements`] - it is not filled in automatically,
    /// since it is a real geometric computation and not every caller needs
    /// it paid for.
    pub space_boundaries: Vec<BimSpaceBoundary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimSource {
    pub application: String,
    pub release: Option<String>,
    /// Whether the reader had an identifier catalog for [`Self::release`].
    ///
    /// `None` where the question does not arise - an IFC states its own
    /// property names, so no release table is consulted to read one. `false`
    /// says the file was read by a reader that has no table for the release
    /// that wrote it, which is not a failure but is a different reading:
    /// built-in parameter names, their units and every category-driven
    /// classification fall away, and what is left came from class names
    /// alone. Carried so that a report or a viewer can say so rather than
    /// showing a thinner model with no explanation.
    pub release_catalogued: Option<bool>,
}

/// What a Revit project's own `ProjectInfo` element states about itself.
///
/// Every field is what the project information dialog holds, not what an
/// element declares - Revit's own IFC export reaches every one of these from
/// exactly the built-in parameters named beside them, which is what fixes
/// the mapping below rather than a guess: `IfcProject.Name` is the project
/// number, not the project name, and `IfcProject.LongName` is the reverse.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BimProjectIdentity {
    /// `PROJECT_NUMBER`, which Revit's own export writes as `IfcProject.Name`.
    pub number: Option<String>,
    /// `PROJECT_NAME`, the descriptive text Revit's own export writes as
    /// `IfcProject.LongName`.
    pub name: Option<String>,
    /// `PROJECT_BUILDING_NAME`, written as both `IfcBuilding.Name` and
    /// `IfcBuilding.LongName`.
    pub building_name: Option<String>,
    /// `PROJECT_ADDRESS`, written whole as the one line of
    /// `IfcPostalAddress.AddressLines` on the building - Revit does not
    /// parse it into a town, a region or a postal code.
    pub address: Option<String>,
    /// `PROJECT_STATUS`, written as `IfcProject.Phase`.
    pub phase: Option<String>,
}

/// Which file, and which save of it, a model was read from.
///
/// This answers a question the rest of the model cannot: given an exported
/// IFC, is it this source file, and has it already been converted? An element
/// identity says which element, and a project identity says which project -
/// several files belong to one project, and one file has many saves.
///
/// Every field is stated by the source or left `None`. Nothing here is
/// computed from the file's bytes: a hash would answer "the same bytes",
/// which a re-save with no edits already breaks, and these are what the
/// document says about itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BimDocumentIdentity {
    /// The identity of this save of the document, hyphenated and lower-case.
    ///
    /// Revit's own `Unique Document GUID`, which changes with every save. It
    /// is the field to compare when the question is whether two exports came
    /// from the same file: measured over a 74-file corpus, two files sharing
    /// it and two files being byte-identical are the same 9 pairs out of
    /// 2701.
    pub document_guid: Option<String>,
    /// How many times the document has been saved, which orders two saves of
    /// one model where the GUIDs only tell them apart.
    pub increment: Option<u32>,
    /// The lineage the document was created in - a template, typically shared
    /// by every model in an office. Never an identity: 74 corpus files state
    /// 19 of these. Kept for grouping, and named so that nothing mistakes it
    /// for [`Self::document_guid`].
    pub creation_guid: Option<String>,
    /// The lineage of the central model this one was detached from, shared by
    /// everything detached from the same one. Not an identity either - 61
    /// values over those 74 files - but it is what stays put across the saves
    /// of one model.
    pub detach_guid: Option<String>,
    /// Whether the file is a central model or a local copy of one, where the
    /// source says. A local copy and its central are different files.
    pub worksharing: Option<String>,
}

impl BimDocumentIdentity {
    /// The `IfcPropertySet` an exporter states this under, on `IfcProject`.
    ///
    /// IFC has no attribute for "which file was this made from", so a writer
    /// and a reader of one have to agree on a spelling. These are that
    /// agreement, kept here rather than in either of them, because an
    /// exported file outlives the session that wrote it: changing one of
    /// these strings orphans every IFC already written under the old one.
    pub const IFC_PROPERTY_SET: &'static str = "openRVT Source Document";
    /// The spelling written before the project was renamed from Rivet.
    ///
    /// A reader accepts it and nothing writes it, for the reason the note
    /// above gives: the strings outlive the session that wrote them, so the
    /// old one has to keep resolving or every IFC already exported stops
    /// being recognisable as one of ours.
    pub const IFC_PROPERTY_SET_LEGACY: &'static str = "Rivet Source Document";
    /// [`Self::document_guid`], the property that identifies the save.
    pub const IFC_DOCUMENT_GUID: &'static str = "DocumentGuid";
    /// [`Self::increment`], which orders two saves of one model.
    pub const IFC_INCREMENT: &'static str = "DocumentIncrement";
    /// [`Self::creation_guid`]. Lineage, not identity.
    pub const IFC_CREATION_GUID: &'static str = "CreationGuid";
    /// [`Self::detach_guid`]. Lineage, not identity.
    pub const IFC_DETACH_GUID: &'static str = "DetachGuid";
    /// [`Self::worksharing`].
    pub const IFC_WORKSHARING: &'static str = "Worksharing";
    /// The source file's own name, which the identity itself does not carry:
    /// a name is not an identity, and is written beside one so that a person
    /// reading the set recognises the file.
    pub const IFC_FILE_NAME: &'static str = "SourceFileName";
}

/// Where a project's `Manage > Location` places it.
///
/// A Revit project can carry more than one named site (an alternate the user
/// switched away from is not deleted, just no longer active), and a reader
/// that cannot yet tell which record is the active one has to choose between
/// guessing and refusing - see whichever field on the importer side reads
/// this, and its own reasoning for picking "every record in the file agrees"
/// as the one case safe enough not to guess.
#[derive(Clone, Debug, PartialEq)]
pub struct BimSiteLocation {
    pub latitude_degrees: f64,
    pub longitude_degrees: f64,
    pub elevation: Option<BimNumber>,
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
    /// What that type is called. Kept beside the identifier rather than
    /// derived from it: an IFC type is identified by a GUID and an RVT one by
    /// a record number, so neither identifier is anything to show a reader.
    pub type_name: Option<String>,
    /// The element this one is cut into, for a door, a window or anything
    /// else a wall, a floor or a roof carries an opening for. `None` for an
    /// element nothing hosts.
    pub host_id: Option<BimElementId>,
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
    /// The source file this element came from, in a federated model. `None`
    /// where the model was read from a single file and the question does not
    /// arise.
    pub document: Option<BimDocumentId>,
    /// The stable identity the source model itself authored for this element,
    /// as 16 UUID bytes in network order - Revit's `UniqueId`. An exporter
    /// writes it where a format has somewhere to put it, so a converted model
    /// and the source's own export name the same element the same way. `None`
    /// where the source states none, and an exporter then derives one.
    pub authored_uuid: Option<[u8; 16]>,
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
    /// The shading colour Revit's own *Shaded* view paints this material
    /// with - not a photorealistic render appearance, which this does not
    /// read. `None` where the source named no material record to read one
    /// from, same as `name`.
    pub color: Option<BimColor>,
}

/// An 8-bit sRGB colour, as Revit's own shading colour states it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BimColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
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
    /// Furniture. Unlike the loadable families above, no reference join fixes
    /// this one: Revit's own export of AR S1 carries no furniture at all, its
    /// export settings having dropped the category. What stands behind it is
    /// Revit's own published category table, `data/importIFCClassMapping.txt`,
    /// which pairs `IfcFurnishingElement` with the furniture category and is
    /// the table Revit reads when it maps the two itself.
    FurnishingElement,
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
    /// The material this one face is painted with, where the source names
    /// one by face rather than through the element's own layered build-up -
    /// a door, a window, a piece of furniture, almost anything a
    /// `material_layers` set does not already cover. `None` for a face with
    /// no material of its own, which is most of them: the element's
    /// category default applies instead, and this export states nothing
    /// about a default it did not read.
    ///
    /// Boxed: a model this size holds millions of faces and almost none of
    /// them carry one, so `Option<BimMaterial>` inline would cost every face
    /// the 72 bytes only a few of them use - see
    /// `the_shapes_a_model_holds_millions_of_stay_narrow`.
    pub material: Option<Box<BimMaterial>>,
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
        /// Boxed for the same reason an arc edge is: a plane is what nearly
        /// every face of a building is, and it should not be charged for the
        /// profile of the few that turn.
        profile: Box<BimBrepProfile>,
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
        profile: Box<BimBrepProfile>,
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

/// How an edge runs between its two endpoints.
///
/// Both variants that carry anything are held behind a pointer. A structural
/// model states tens of millions of edges - 23.8 million in one 1.8 GiB IFC -
/// and every one of them is as large as the largest variant, so an inline arc
/// charged its 120 bytes to the straight edges that are the great majority.
/// Boxed, a [`BimBrepEdge`] is 88 bytes rather than 184, which on that file is
/// 2.3 GB the model no longer holds.
#[derive(Clone, Debug, PartialEq)]
pub enum BimBrepCurve {
    Line,
    Arc(Box<BimBrepArc>),
    /// A sampled source curve, including its two topological endpoints. Held
    /// as a boxed slice: it is built once and never appended to.
    Polyline(Box<[BimPoint3]>),
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

/// The names of a unit, held once and pointed at.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BimUnitNames {
    /// Stable external identifier, for example a Forge unit type ID.
    pub id: String,
    pub name: String,
}

/// A unit of measure, as a shared handle.
///
/// Every [`BimPoint3`] carries one, and a model has essentially one unit, so
/// the two `String`s this used to hold inline were the single largest cost in
/// a decoded model: 72 bytes and two allocations per point, spelling
/// "autodesk.unit.unit:meters-1.0.0" and "Meters" over and over. A structural
/// model with 58.7 million declared edges reached 39 GB of memory that way,
/// against 2.2 GB for a model of the same file size and a twelfth the
/// geometry. One pointer instead - 8 bytes, no allocation per point - and
/// cloning is a refcount bump.
///
/// It still reads like the struct it replaced: `unit.id` and `unit.name` work
/// through [`Deref`](std::ops::Deref).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BimUnit(Arc<BimUnitNames>);

impl BimUnit {
    /// A unit from its two names. Allocates, so a caller on a hot path should
    /// hold the result and clone it rather than calling this per point.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self(Arc::new(BimUnitNames {
            id: id.into(),
            name: name.into(),
        }))
    }

    /// Metres, the unit every length in a decoded model is carried in.
    ///
    /// Made once for the life of the process and handed out by clone, because
    /// the per-point closures that convert a body call this for every
    /// coordinate they read.
    #[must_use]
    pub fn metres() -> Self {
        static METRES: OnceLock<BimUnit> = OnceLock::new();
        METRES
            .get_or_init(|| Self::new("autodesk.unit.unit:meters-1.0.0", "Meters"))
            .clone()
    }
}

impl std::ops::Deref for BimUnit {
    type Target = BimUnitNames;

    fn deref(&self) -> &BimUnitNames {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimRelation {
    pub kind: String,
    pub source: BimElementId,
    pub target: BimElementId,
}

/// One face of a space's own closed body, matched against the one real
/// element whose own face coincides with it - what `IfcRelSpaceBoundary`
/// states in IFC.
///
/// Written only where a match was found: a face this reader could not match
/// to a real element is left out rather than stated as a virtual boundary it
/// has no evidence for. See [`compute_space_boundaries`].
#[derive(Clone, Debug, PartialEq)]
pub struct BimSpaceBoundary {
    pub space_id: BimElementId,
    pub element_id: BimElementId,
    /// The space's own face's plane, verbatim - in the model's project
    /// frame, the same frame every `BimGeometry::Brep` is already carried
    /// in before an exporter places it relative to a storey.
    pub origin: BimPoint3,
    pub x_axis: [f64; 3],
    pub y_axis: [f64; 3],
    /// The face's outer boundary, closed (first point not repeated), in the
    /// same frame as `origin`.
    pub boundary: Vec<BimPoint3>,
}

/// How close two candidate faces' planes have to agree, in metres, to call
/// them the same surface. Real modelled geometry that genuinely touches
/// agrees to a small fraction of a millimetre; this leaves room for the
/// arithmetic without accepting two surfaces that merely sit close.
const SPACE_BOUNDARY_PLANE_TOLERANCE_METRES: f64 = 0.005;
/// How nearly opposite two coincident faces' outward normals have to point.
/// Two solids meeting at a shared boundary always face away from each
/// other there, so this should be almost exactly -1; the margin is for the
/// arithmetic, not for a judgement about what counts as facing away.
const SPACE_BOUNDARY_NORMAL_ALIGNMENT: f64 = 0.999;
/// Side of the grid cell candidate faces are bucketed into before the
/// per-face test, in metres - a few times a typical room's own extent, so a
/// room's own cell and its neighbours hold every wall that could plausibly
/// bound it without scanning the whole model.
const SPACE_BOUNDARY_GRID_METRES: f64 = 5.0;

/// One straight-edged planar face, reduced to what matching a space
/// boundary needs: its plane, its own outer polygon, and the 3D box that
/// polygon spans - the last for the coarse spatial filter below.
struct PlanarFace<'a> {
    element_id: &'a BimElementId,
    origin: [f64; 3],
    x_axis: [f64; 3],
    y_axis: [f64; 3],
    normal: [f64; 3],
    /// The face's own outer loop, as the plain coordinates
    /// [`compute_space_boundaries`] does its arithmetic in.
    polygon: Vec<[f64; 3]>,
    min: [f64; 3],
    max: [f64; 3],
}

fn subtract3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize3(a: [f64; 3]) -> Option<[f64; 3]> {
    let length = dot3(a, a).sqrt();
    (length > 0.0).then(|| [a[0] / length, a[1] / length, a[2] / length])
}

/// The straight-line outer boundary of one face, in plain coordinates - or
/// `None` for a face this cannot read one from: a surface that is not a
/// plane, a boundary with a curved edge, or one with too few points to be a
/// polygon at all. A space boundary is only ever stated from geometry this
/// exactly, never approximated from a curve.
fn planar_face<'a>(element_id: &'a BimElementId, face: &BimBrepFace) -> Option<PlanarFace<'a>> {
    let BimBrepSurface::Plane {
        origin,
        x_axis,
        y_axis,
    } = &face.surface
    else {
        return None;
    };
    let normal = normalize3(cross3(*x_axis, *y_axis))?;
    let outer = face.loops.first()?;
    if outer.len() < 3 {
        return None;
    }
    let mut polygon = Vec::with_capacity(outer.len());
    for edge in outer {
        if !matches!(edge.curve, BimBrepCurve::Line) {
            return None;
        }
        polygon.push(edge.start.coordinates);
    }
    let mut min = polygon[0];
    let mut max = polygon[0];
    for point in &polygon[1..] {
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    Some(PlanarFace {
        element_id,
        origin: origin.coordinates,
        x_axis: *x_axis,
        y_axis: *y_axis,
        normal,
        polygon,
        min,
        max,
    })
}

/// The grid cell one point falls into, at [`SPACE_BOUNDARY_GRID_METRES`].
fn grid_cell(point: [f64; 3]) -> [i32; 3] {
    #[allow(clippy::cast_possible_truncation)]
    // A building's coordinates are nowhere near i32's range at this cell
    // size.
    point.map(|value| (value / SPACE_BOUNDARY_GRID_METRES).floor() as i32)
}

/// Whether `face` sits close enough to `candidate`'s own plane, facing away
/// from it, to be considered the same physical surface.
fn coincides(face: &PlanarFace<'_>, candidate: &PlanarFace<'_>) -> bool {
    if dot3(face.normal, candidate.normal) > -SPACE_BOUNDARY_NORMAL_ALIGNMENT {
        return false;
    }
    let offset = dot3(subtract3(candidate.origin, face.origin), face.normal);
    offset.abs() <= SPACE_BOUNDARY_PLANE_TOLERANCE_METRES
}

/// Whether the two faces' own polygons genuinely overlap once projected
/// into `face`'s own 2D frame, rather than merely sharing a plane - two
/// walls end to end share a plane at their butt joint without either one
/// bounding what lies past it.
///
/// The extent of each polygon in that frame is compared rather than the
/// polygons themselves: exact polygon intersection would tell an L-shaped
/// room's short leg from a wall that only borders its long one, which this
/// cannot, but every overlap this accepts is a real one - it only risks
/// accepting a boundary too generously, never inventing a plane that is not
/// there.
fn overlaps_in_plane(face: &PlanarFace<'_>, candidate: &PlanarFace<'_>) -> bool {
    let project = |point: [f64; 3]| {
        let local = subtract3(point, face.origin);
        (dot3(local, face.x_axis), dot3(local, face.y_axis))
    };
    let extent = |polygon: &[[f64; 3]]| {
        let mut min = (f64::INFINITY, f64::INFINITY);
        let mut max = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for point in polygon {
            let (u, v) = project(*point);
            min = (min.0.min(u), min.1.min(v));
            max = (max.0.max(u), max.1.max(v));
        }
        (min, max)
    };
    let (a_min, a_max) = extent(&face.polygon);
    let (b_min, b_max) = extent(&candidate.polygon);
    a_min.0 <= b_max.0 && b_min.0 <= a_max.0 && a_min.1 <= b_max.1 && b_min.1 <= a_max.1
}

/// Every grid cell a face's own `[min, max]` extent touches - so a face
/// larger than one cell is still found from any of them, in either
/// direction: the same function buckets a candidate's own cells on the way
/// in and a space face's cells on the way out.
fn grid_cells(min: [f64; 3], max: [f64; 3]) -> impl Iterator<Item = [i32; 3]> {
    let low = grid_cell(min);
    let high = grid_cell(max);
    (low[0]..=high[0]).flat_map(move |x| {
        (low[1]..=high[1]).flat_map(move |y| (low[2]..=high[2]).map(move |z| [x, y, z]))
    })
}

/// Match every space's own closed body against the real elements around it,
/// one face at a time - what `IfcRelSpaceBoundary` states in IFC.
///
/// A face this reader cannot match to a real element - a curved boundary,
/// an unrecognised surface, or one genuinely bordering nothing modelled -
/// is left out rather than guessed at: every entry this returns names a
/// real element whose own face was found to coincide, never a virtual
/// boundary inferred from its absence.
///
/// Elements are bucketed into a coarse grid before the precise per-face
/// test, so a space is only ever compared against the elements actually
/// near it rather than the whole model. A face is registered under every
/// cell its own extent touches rather than one corner, so a wall or slab
/// larger than one cell is still found from whichever end of it a space
/// actually borders.
#[must_use]
pub fn compute_space_boundaries(elements: &[BimElement]) -> Vec<BimSpaceBoundary> {
    let mut faces = Vec::new();
    for element in elements {
        if element.element_type.is_spatial() {
            continue;
        }
        let Some(BimGeometry::Brep(brep)) = &element.geometry else {
            continue;
        };
        for face in &brep.faces {
            if let Some(planar) = planar_face(&element.id, face) {
                faces.push(planar);
            }
        }
    }
    let mut grid: HashMap<[i32; 3], Vec<usize>> = HashMap::new();
    for (index, face) in faces.iter().enumerate() {
        for cell in grid_cells(face.min, face.max) {
            grid.entry(cell).or_default().push(index);
        }
    }

    let mut boundaries = Vec::new();
    for space in elements {
        if !space.element_type.is_spatial() {
            continue;
        }
        let Some(BimGeometry::Brep(brep)) = &space.geometry else {
            continue;
        };
        for face in &brep.faces {
            let Some(space_face) = planar_face(&space.id, face) else {
                continue;
            };
            let matched = grid_cells(space_face.min, space_face.max)
                .filter_map(|cell| grid.get(&cell))
                .flatten()
                .map(|&index| &faces[index])
                .find(|candidate| {
                    coincides(&space_face, candidate) && overlaps_in_plane(&space_face, candidate)
                });
            if let Some(matched) = matched {
                boundaries.push(BimSpaceBoundary {
                    space_id: space.id.clone(),
                    element_id: matched.element_id.clone(),
                    origin: BimPoint3 {
                        coordinates: space_face.origin,
                        unit: BimUnit::metres(),
                    },
                    x_axis: space_face.x_axis,
                    y_axis: space_face.y_axis,
                    boundary: space_face
                        .polygon
                        .iter()
                        .map(|point| BimPoint3 {
                            coordinates: *point,
                            unit: BimUnit::metres(),
                        })
                        .collect(),
                });
            }
        }
    }
    boundaries
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimLevel {
    pub id: BimElementId,
    pub name: Option<String>,
    /// Elevation in a canonical, explicitly named unit.
    pub elevation: Option<BimNumber>,
}

/// One source file that contributed to a model.
///
/// A model read from a single file has one of these; a federated model - an
/// IFC set, or a model and its links - has one per file, and every element
/// names the one it came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimDocument {
    pub id: BimDocumentId,
    /// The file's own name, as it was handed over.
    pub name: String,
    /// The source format's short tag, such as `rvt` or `ifc`.
    pub kind: String,
    pub source: Option<BimSource>,
    /// Which file, and which save of it, this is - as the file states it,
    /// not as it was named or where it was found. `None` for a format that
    /// says nothing about its own identity.
    pub identity: Option<BimDocumentIdentity>,
    /// Elements this document contributed.
    pub elements: usize,
}

/// A document's short name inside a federated model.
///
/// Kept separate from [`BimElementId`] because it namespaces one: two files
/// may both call an element `1234`, and only the pair identifies it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BimDocumentId(pub String);

impl BimElementId {
    /// This identifier qualified by the document it came from.
    ///
    /// The separator is `/`, which neither an RVT record number nor an IFC
    /// `GlobalId` contains, so the qualified form can always be split back.
    #[must_use]
    pub fn qualified(&self, document: &BimDocumentId) -> Self {
        Self(format!("{}/{}", document.0, self.0))
    }
}

/// What [`federate`] assembled, and what a reader of the result should be
/// told about it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BimFederationReport {
    pub documents: usize,
    pub elements: usize,
    /// Element identifiers that occurred in more than one document. These are
    /// the collisions qualification prevented; zero means qualification
    /// changed nothing but the spelling of every identifier.
    pub collisions: usize,
    /// Elements whose authored identity more than one document claimed, and
    /// which therefore lost it. See [`federate`].
    pub authored_identity_collisions: usize,
    /// Pairs of documents whose stated geometry does not overlap at all.
    ///
    /// This is the only coordinate check made, and it is a report rather than
    /// a correction. Files exported from one coordinated project share a
    /// survey point and their extents overlap; files that do not are almost
    /// certainly stated about different origins, and nothing here knows the
    /// transform that would reconcile them. Nothing is moved on the strength
    /// of this - it says where to look.
    pub disjoint: Vec<(BimDocumentId, BimDocumentId)>,
}

/// Assemble several source models into one.
///
/// With a single source nothing is renamed: its identifiers reach the output
/// exactly as its reader stated them, so a one-file conversion's element ids,
/// and every `GlobalId` derived from them, stay what they have always been.
///
/// With several, every identifier is qualified by its document - element ids,
/// the level and type each element names, relation endpoints, and the
/// references stored in property values - because two files that each number
/// an element `1234` would otherwise collapse into one element. The
/// qualification is applied whether or not a collision actually occurred, so
/// that an identifier's meaning does not depend on what else happened to be
/// federated with it.
///
/// Coordinates are **not** reconciled. Each document's geometry arrives in
/// whatever world system its own file stated; see
/// [`BimFederationReport::disjoint`].
#[must_use]
pub fn federate(sources: Vec<(BimDocument, BimModel)>) -> (BimModel, BimFederationReport) {
    let mut report = BimFederationReport {
        documents: sources.len(),
        ..BimFederationReport::default()
    };
    let qualify = sources.len() > 1;

    // Which documents claimed each identifier, so that a collision can be
    // counted rather than assumed.
    let mut claimed: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut extents: Vec<(BimDocumentId, Option<BimBoundingBox>)> = Vec::new();
    let mut out = BimModel::default();

    for (mut document, mut model) in sources {
        document.elements = model.elements.len();
        extents.push((document.id.clone(), model.extent()));
        for element in &model.elements {
            *claimed.entry(element.id.0.clone()).or_default() += 1;
        }
        if qualify {
            model.qualify(&document.id);
        }
        for element in &mut model.elements {
            element.document = Some(document.id.clone());
        }
        // A single source keeps its own provenance in the shorthand field as
        // well, so every consumer that reads only that still works.
        if !qualify {
            out.source = model.source.clone();
            out.project = model.project.clone();
            out.document_identity.clone_from(&model.document_identity);
            out.site = model.site;
            // A federated model has no single frame to be shared in - each
            // file states its own - so this, like the fields above it, is the
            // single-source shorthand and stays unset for a federation.
            out.site_placement.clone_from(&model.site_placement);
        }
        out.elements.append(&mut model.elements);
        out.levels.append(&mut model.levels);
        out.relations.append(&mut model.relations);
        out.space_boundaries.append(&mut model.space_boundaries);
        out.documents.push(document);
    }

    report.elements = out.elements.len();
    report.collisions = claimed.values().filter(|count| **count > 1).count();
    // An authored identity is the source's own, and two sources can state the
    // same one: a model linked into two others carries its elements, and their
    // `UniqueId`s, into both. An identifier is qualified by its document to
    // keep those apart, but an authored UUID cannot be - so where one is not
    // unique across the federation, nobody gets it and the exporter derives an
    // identity for each instead. IFC requires a `GlobalId` to be unique, and a
    // duplicate is worse than a derived identity.
    let mut authored: std::collections::BTreeMap<[u8; 16], usize> =
        std::collections::BTreeMap::new();
    for element in &out.elements {
        if let Some(uuid) = element.authored_uuid {
            *authored.entry(uuid).or_default() += 1;
        }
    }
    for element in &mut out.elements {
        if element
            .authored_uuid
            .is_some_and(|uuid| authored.get(&uuid).is_some_and(|count| *count > 1))
        {
            element.authored_uuid = None;
            report.authored_identity_collisions += 1;
        }
    }
    for (left_index, (left, left_extent)) in extents.iter().enumerate() {
        for (right, right_extent) in &extents[left_index + 1..] {
            if let (Some(left_extent), Some(right_extent)) = (left_extent, right_extent) {
                if !left_extent.overlaps(right_extent) {
                    report.disjoint.push((left.clone(), right.clone()));
                }
            }
        }
    }
    (out, report)
}

impl BimBoundingBox {
    /// Whether two boxes share any volume, touching counted as sharing.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        (0..3).all(|axis| {
            self.min.coordinates[axis] <= other.max.coordinates[axis]
                && other.min.coordinates[axis] <= self.max.coordinates[axis]
        })
    }

    /// The smallest box holding both.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        let mut min = self.min.clone();
        let mut max = self.max.clone();
        for axis in 0..3 {
            min.coordinates[axis] = min.coordinates[axis].min(other.min.coordinates[axis]);
            max.coordinates[axis] = max.coordinates[axis].max(other.max.coordinates[axis]);
        }
        Self { min, max }
    }
}

impl BimModel {
    /// The hull of every point the model's geometry states, or `None` where
    /// no element carries geometry.
    ///
    /// This is a hull of stated vertices, not an exact extent: an arc bulges
    /// past its own endpoints, and a swept disk past its directrix by its
    /// radius. It is enough to tell whether two documents are stated about
    /// the same origin, which is what it is for.
    #[must_use]
    pub fn extent(&self) -> Option<BimBoundingBox> {
        let mut extent: Option<BimBoundingBox> = None;
        let mut include = |point: &BimPoint3| {
            let box_of = BimBoundingBox {
                min: point.clone(),
                max: point.clone(),
            };
            extent = Some(match extent.take() {
                Some(current) => current.union(&box_of),
                None => box_of,
            });
        };
        for element in &self.elements {
            match &element.geometry {
                None => {}
                Some(BimGeometry::AxisLine(line)) => {
                    include(&line.start);
                    include(&line.end);
                }
                Some(BimGeometry::SweptDisk(disk)) => {
                    include(&disk.directrix.start);
                    include(&disk.directrix.end);
                }
                Some(BimGeometry::BoundingBox(bounds)) => {
                    include(&bounds.min);
                    include(&bounds.max);
                }
                Some(BimGeometry::Brep(brep)) => brep_points(brep, &mut include),
                Some(BimGeometry::Assembly(breps)) => {
                    for brep in breps {
                        brep_points(brep, &mut include);
                    }
                }
            }
        }
        extent
    }

    /// Rewrite every identifier this model states to name `document` as well.
    ///
    /// Every field of type [`BimElementId`] is covered: an identifier missed
    /// here would become a reference into another document's elements, which
    /// is a silently wrong model rather than a broken one.
    fn qualify(&mut self, document: &BimDocumentId) {
        for element in &mut self.elements {
            element.id = element.id.qualified(document);
            if let Some(level) = &element.level_id {
                element.level_id = Some(level.qualified(document));
            }
            if let Some(kind) = &element.type_id {
                element.type_id = Some(kind.qualified(document));
            }
            if let Some(host) = &element.host_id {
                element.host_id = Some(host.qualified(document));
            }
            if let Some(layers) = &mut element.material_layers {
                if let Some(source) = &layers.source_type_id {
                    layers.source_type_id = Some(source.qualified(document));
                }
            }
            for property in element
                .properties
                .iter_mut()
                .chain(element.type_properties.iter_mut())
            {
                if let BimPropertyValue::Reference(id) = &property.value {
                    property.value = BimPropertyValue::Reference(id.qualified(document));
                }
            }
        }
        for level in &mut self.levels {
            level.id = level.id.qualified(document);
        }
        for relation in &mut self.relations {
            relation.source = relation.source.qualified(document);
            relation.target = relation.target.qualified(document);
        }
        for boundary in &mut self.space_boundaries {
            boundary.space_id = boundary.space_id.qualified(document);
            boundary.element_id = boundary.element_id.qualified(document);
        }
    }
}

/// Every point a boundary representation states, endpoints included.
fn brep_points(brep: &BimBrep, include: &mut impl FnMut(&BimPoint3)) {
    for face in &brep.faces {
        for boundary in &face.loops {
            for edge in boundary {
                include(&edge.start);
                include(&edge.end);
                if let BimBrepCurve::Polyline(points) = &edge.curve {
                    for point in points {
                        include(point);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(coordinates: [f64; 3]) -> BimPoint3 {
        BimPoint3 {
            coordinates,
            unit: BimUnit::metres(),
        }
    }

    /// One planar, straight-edged face: the plane `origin`/`x_axis`/`y_axis`
    /// declare, bounded by `polygon` - closed, first point not repeated.
    fn planar_face(
        origin: [f64; 3],
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        polygon: &[[f64; 3]],
    ) -> BimBrepFace {
        let edges = (0..polygon.len())
            .map(|index| BimBrepEdge {
                start: point(polygon[index]),
                end: point(polygon[(index + 1) % polygon.len()]),
                curve: BimBrepCurve::Line,
            })
            .collect();
        BimBrepFace {
            surface: BimBrepSurface::Plane {
                origin: point(origin),
                x_axis,
                y_axis,
            },
            loops: vec![edges],
            material: None,
        }
    }

    fn space_with_faces(id: &str, faces: Vec<BimBrepFace>) -> BimElement {
        BimElement {
            geometry: Some(BimGeometry::Brep(BimBrep {
                faces,
                complete: false,
            })),
            ..element(id, [0.0, 0.0, 0.0])
        }
    }

    fn boundary_element_with_faces(id: &str, faces: Vec<BimBrepFace>) -> BimElement {
        BimElement {
            element_type: BimElementType::Wall,
            geometry: Some(BimGeometry::Brep(BimBrep {
                faces,
                complete: false,
            })),
            ..element(id, [0.0, 0.0, 0.0])
        }
    }

    /// A room's own face at the plane `x = 3`, its outward normal `+X`
    /// (matching a room whose interior sits at `x < 3`), spanning the same
    /// 4x3 m rectangle two of the fixtures below share.
    fn room_facing_wall() -> BimBrepFace {
        planar_face(
            [3.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            &[
                [3.0, 0.0, 0.0],
                [3.0, 4.0, 0.0],
                [3.0, 4.0, 3.0],
                [3.0, 0.0, 3.0],
            ],
        )
    }

    #[test]
    fn a_room_matches_the_one_wall_face_that_coincides_with_its_own() {
        let mut space = space_with_faces("room", vec![room_facing_wall()]);
        space.element_type = BimElementType::Space;

        // The wall's own interior face, at the same plane, facing back into
        // the room - `-X`, anti-parallel to the room's `+X` - and spanning
        // the same rectangle. A real wall's own body carries more faces than
        // this one, but only this face is what a boundary match needs.
        let wall = boundary_element_with_faces(
            "wall",
            vec![planar_face(
                [3.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 0.0],
                &[
                    [3.0, 0.0, 0.0],
                    [3.0, 0.0, 3.0],
                    [3.0, 4.0, 3.0],
                    [3.0, 4.0, 0.0],
                ],
            )],
        );

        let elements = vec![space, wall];
        let boundaries = compute_space_boundaries(&elements);
        assert_eq!(boundaries.len(), 1, "{boundaries:?}");
        assert_eq!(boundaries[0].space_id, BimElementId("room".to_owned()));
        assert_eq!(boundaries[0].element_id, BimElementId("wall".to_owned()));
        assert_eq!(boundaries[0].boundary.len(), 4);
    }

    #[test]
    fn a_room_face_finds_no_match_without_a_real_coincident_face() {
        let mut space = space_with_faces("room", vec![room_facing_wall()]);
        space.element_type = BimElementType::Space;

        // Same plane and orientation, but nowhere near the room's own
        // rectangle in that plane - a wall this room does not actually
        // border, however far its infinite plane would extend.
        let distant = boundary_element_with_faces(
            "distant_wall",
            vec![planar_face(
                [3.0, 100.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 0.0],
                &[
                    [3.0, 100.0, 0.0],
                    [3.0, 100.0, 3.0],
                    [3.0, 104.0, 3.0],
                    [3.0, 104.0, 0.0],
                ],
            )],
        );
        // Same plane and extent, but facing the same way as the room's own
        // face rather than away from it - not a real coincident boundary,
        // whatever else agrees.
        let same_facing = boundary_element_with_faces("same_facing", vec![room_facing_wall()]);

        let elements = vec![space, distant, same_facing];
        assert!(compute_space_boundaries(&elements).is_empty());
    }

    /// One element with an identifier, a level, a type and a reference, so
    /// that every kind of identifier a model states is present.
    fn element(id: &str, at: [f64; 3]) -> BimElement {
        BimElement {
            id: BimElementId(id.to_owned()),
            document: None,
            authored_uuid: None,
            element_type: BimElementType::Wall,
            class_name: None,
            name: None,
            long_name: None,
            category: None,
            level_id: Some(BimElementId("level".to_owned())),
            type_id: Some(BimElementId("type".to_owned())),
            type_name: None,
            host_id: Some(BimElementId("host".to_owned())),
            placement: None,
            geometry: Some(BimGeometry::BoundingBox(BimBoundingBox {
                min: point(at),
                max: point([at[0] + 1.0, at[1] + 1.0, at[2] + 1.0]),
            })),
            properties: vec![BimProperty {
                id: None,
                name: "Host".to_owned(),
                specification: None,
                value: BimPropertyValue::Reference(BimElementId("host".to_owned())),
            }],
            type_properties: Vec::new(),
            material_layers: Some(BimMaterialLayerSet {
                source_type_id: Some(BimElementId("type".to_owned())),
                name: None,
                layers: Vec::new(),
            }),
        }
    }

    fn model(id: &str, at: [f64; 3]) -> BimModel {
        BimModel {
            source: Some(BimSource {
                application: "Test".to_owned(),
                release: None,
                release_catalogued: None,
            }),
            project: Some(BimProjectIdentity {
                number: Some("PN-1".to_owned()),
                ..BimProjectIdentity::default()
            }),
            site: None,
            site_placement: None,
            document_identity: Some(identity(id)),
            documents: Vec::new(),
            elements: vec![element(id, at)],
            levels: vec![BimLevel {
                id: BimElementId("level".to_owned()),
                name: None,
                elevation: None,
            }],
            relations: vec![BimRelation {
                kind: "contains".to_owned(),
                source: BimElementId("level".to_owned()),
                target: BimElementId(id.to_owned()),
            }],
            space_boundaries: Vec::new(),
        }
    }

    fn document(id: &str) -> BimDocument {
        BimDocument {
            id: BimDocumentId(id.to_owned()),
            name: format!("{id}.ifc"),
            kind: "ifc".to_owned(),
            source: None,
            identity: Some(identity(id)),
            elements: 0,
        }
    }

    /// An identity distinct per file, so a federated model can be checked for
    /// keeping each file's own rather than one of them for all.
    fn identity(id: &str) -> BimDocumentIdentity {
        BimDocumentIdentity {
            document_guid: Some(format!("guid-of-{id}")),
            ..BimDocumentIdentity::default()
        }
    }

    #[test]
    fn one_source_keeps_every_identifier_its_reader_stated() {
        let (federated, report) = federate(vec![(document("a"), model("1234", [0.0; 3]))]);

        // A conversion of a single file must not have its ids renamed: an IFC
        // GlobalId is derived from them, and they are what a caller has
        // already stored.
        assert_eq!(federated.elements[0].id, BimElementId("1234".to_owned()));
        assert_eq!(federated.levels[0].id, BimElementId("level".to_owned()));
        assert_eq!(
            federated.relations[0].target,
            BimElementId("1234".to_owned())
        );
        assert_eq!(federated.documents.len(), 1);
        assert_eq!(federated.documents[0].elements, 1);
        assert_eq!(
            federated.elements[0].document,
            Some(BimDocumentId("a".to_owned()))
        );
        // The shorthand still answers for a single-source model.
        assert!(federated.source.is_some());
        assert_eq!(
            federated.project,
            Some(BimProjectIdentity {
                number: Some("PN-1".to_owned()),
                ..BimProjectIdentity::default()
            })
        );
        assert_eq!(
            federated.document_identity,
            Some(identity("1234")),
            "which file this was read from survives being federated with itself"
        );
        assert_eq!(report.collisions, 0);
        assert!(report.disjoint.is_empty());
    }

    #[test]
    fn several_sources_state_one_identity_each_and_none_for_the_whole() {
        let (federated, _) = federate(vec![
            (document("a"), model("1234", [0.0; 3])),
            (document("b"), model("1234", [0.0; 3])),
        ]);

        // No file is *the* file a federated model came from, so the
        // shorthand answers for none of them - and a reader asking which
        // files these are gets all of them rather than one picked out.
        assert_eq!(federated.document_identity, None);
        assert_eq!(
            federated
                .documents
                .iter()
                .map(|document| document.identity.clone())
                .collect::<Vec<_>>(),
            vec![Some(identity("a")), Some(identity("b"))]
        );
    }

    #[test]
    fn several_sources_qualify_every_kind_of_identifier() {
        let (federated, report) = federate(vec![
            (document("a"), model("1234", [0.0; 3])),
            (document("b"), model("5678", [0.5, 0.0, 0.0])),
        ]);

        // Two files each name a project; the federated whole names neither,
        // the same rule this test already applies to `source` below.
        assert_eq!(federated.project, None);
        assert_eq!(federated.elements.len(), 2);
        let first = &federated.elements[0];
        assert_eq!(first.id, BimElementId("a/1234".to_owned()));
        assert_eq!(first.level_id, Some(BimElementId("a/level".to_owned())));
        assert_eq!(first.type_id, Some(BimElementId("a/type".to_owned())));
        assert_eq!(first.host_id, Some(BimElementId("a/host".to_owned())));
        assert_eq!(
            first.material_layers.as_ref().unwrap().source_type_id,
            Some(BimElementId("a/type".to_owned()))
        );
        assert_eq!(
            first.properties[0].value,
            BimPropertyValue::Reference(BimElementId("a/host".to_owned()))
        );
        assert_eq!(first.document, Some(BimDocumentId("a".to_owned())));

        // The two files each state a level called `level`; qualification is
        // what keeps them two levels.
        assert_eq!(federated.levels.len(), 2);
        assert_eq!(federated.levels[0].id, BimElementId("a/level".to_owned()));
        assert_eq!(federated.levels[1].id, BimElementId("b/level".to_owned()));
        assert_eq!(
            federated.relations[1].target,
            BimElementId("b/5678".to_owned())
        );
        // No element id occurred twice, even though the levels did.
        assert_eq!(report.documents, 2);
        assert_eq!(report.elements, 2);
        assert!(report.disjoint.is_empty());
        // There is no one application behind a federated model.
        assert!(federated.source.is_none());
    }

    /// An authored identity cannot be qualified by its document the way an
    /// identifier can - it is a UUID the source states - so where two
    /// documents state the same one, neither element keeps it. A duplicate
    /// `GlobalId` is not valid IFC; a derived identity is.
    #[test]
    fn an_authored_identity_two_documents_both_state_is_given_up() {
        let shared = [7_u8; 16];
        let mut left = model("1234", [0.0; 3]);
        left.elements[0].authored_uuid = Some(shared);
        let mut right = model("5678", [0.5, 0.0, 0.0]);
        right.elements[0].authored_uuid = Some(shared);
        let mut alone = model("9999", [1.0, 0.0, 0.0]);
        alone.elements[0].authored_uuid = Some([9_u8; 16]);

        let (federated, report) = federate(vec![
            (document("a"), left),
            (document("b"), right),
            (document("c"), alone),
        ]);

        assert_eq!(report.authored_identity_collisions, 2);
        assert_eq!(federated.elements[0].authored_uuid, None);
        assert_eq!(federated.elements[1].authored_uuid, None);
        assert_eq!(
            federated.elements[2].authored_uuid,
            Some([9_u8; 16]),
            "the one nothing else claimed keeps it"
        );
    }

    #[test]
    fn an_identifier_two_documents_both_claim_is_counted() {
        let (federated, report) = federate(vec![
            (document("a"), model("1234", [0.0; 3])),
            (document("b"), model("1234", [0.5, 0.0, 0.0])),
        ]);

        assert_eq!(report.collisions, 1);
        // And they stayed two elements, which is the point of counting it.
        assert_eq!(federated.elements.len(), 2);
        assert_ne!(federated.elements[0].id, federated.elements[1].id);
    }

    #[test]
    fn documents_stated_about_different_origins_are_reported_not_moved() {
        let far = model("5678", [10_000.0, 0.0, 0.0]);
        let (federated, report) = federate(vec![
            (document("a"), model("1234", [0.0; 3])),
            (document("b"), far),
        ]);

        assert_eq!(
            report.disjoint,
            vec![(BimDocumentId("a".to_owned()), BimDocumentId("b".to_owned()))]
        );
        // Reported, and nothing was moved on the strength of it.
        let Some(BimGeometry::BoundingBox(bounds)) = &federated.elements[1].geometry else {
            panic!("the far document lost its geometry");
        };
        assert!((bounds.min.coordinates[0] - 10_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_extent_is_none_where_nothing_carries_geometry() {
        let mut empty = model("1234", [0.0; 3]);
        empty.elements[0].geometry = None;
        assert!(empty.extent().is_none());
        // And a document without geometry is never called disjoint from one
        // that has it, because there is nothing to compare.
        let (_, report) = federate(vec![
            (document("a"), empty),
            (document("b"), model("5678", [10_000.0, 0.0, 0.0])),
        ]);
        assert!(report.disjoint.is_empty());
    }

    /// The geometry a model holds is these three shapes, tens of millions of
    /// times over, so their size *is* the model's memory: a 1.8 GiB structural
    /// IFC states 23.8 million edges and 7.1 million faces, where eight bytes
    /// added here is 250 MB added there. Pinned so that a variant widened
    /// without a thought is a failing test rather than a machine that swaps.
    #[test]
    fn the_shapes_a_model_holds_millions_of_stay_narrow() {
        assert_eq!(std::mem::size_of::<BimBrepEdge>(), 88);
        // +8 over the edge-only shape: `material` is a boxed pointer, one
        // word, so the millions of faces that carry none pay eight bytes
        // rather than the 72 an inline `Option<BimMaterial>` would cost every
        // one of them for the few that do.
        assert_eq!(std::mem::size_of::<BimBrepFace>(), 160);
        assert_eq!(std::mem::size_of::<BimPoint3>(), 32);
    }

    #[test]
    fn an_unknown_unit_is_distinct_from_a_unitless_quantity() {
        let unknown = BimNumber {
            value: 12.0,
            unit: None,
        };
        let unitless = BimNumber {
            value: 12.0,
            unit: Some(BimUnit::new("autodesk.unit.unit:general-1.0.1", "General")),
        };
        assert_ne!(unknown, unitless);
    }
}
