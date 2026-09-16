use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    error::Error,
    fmt,
    hash::{Hash, Hasher},
};

use bim_convert::{ifc_entity_name, resolved_element_type};
use bim_core::{
    BimBoundingBox, BimBrep, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepProfile, BimBrepRuling,
    BimBrepSurface, BimElement, BimElementId, BimExternalId, BimGeometry, BimLevel, BimLineSegment,
    BimMaterial, BimMaterialLayer, BimModel, BimNumber, BimPlacement, BimPoint3, BimProperty,
    BimPropertyValue,
};

use crate::{
    ClassMapping, EntityRef, ExportSettings, IfcGuid, LengthUnit, Mapped, StepFile, StepHeader,
    StepValue,
    extrusion::{self, NotAPrism, SolidReport},
    ifc4_entities::{
        Attribute, IFC4_BASE_QUANTITY_SETS, IFC4_COMMON_PROPERTY_SETS, IFC4_ELEMENT_TYPES,
        IFC4_ELEMENTS, IFC4_SPATIAL_ELEMENT_TYPES, Ifc4Entity,
    },
    quantities::{self, Measured},
};

/// How near the axis of revolution a profile's point must be to call it *on*
/// the axis, in metres - the difference between a sphere and a torus, and
/// between a cylinder and a cone. Well below any modelled dimension and well
/// above the noise in a decoded double.
const REVOLVED_AXIS_TOLERANCE_METRES: f64 = 1.0e-9;

/// How far past its face's own reach a cone's profile is swept, as a fraction
/// of that reach, so the boundary trims the surface's interior rather than
/// landing exactly on its edge.
const CONE_MARGIN_FRACTION: f64 = 0.05;

/// How much smaller than the major radius a minor radius has to be before the
/// surface is written as an `IfcToroidalSurface`. The schema's own rule is a
/// strict inequality; this keeps a torus that satisfies it by one part in
/// 10^15 - a horn torus whose radii came back from two decoded doubles - out
/// of a form that cannot hold it.
const TORUS_RADIUS_MARGIN: f64 = 1.0e-6;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataOptions {
    /// Stable namespace for this source model.
    pub model_namespace: [u8; 16],
    pub file_name: String,
    pub timestamp: String,
    pub creation_time: i64,
    pub project_name: String,
    pub site_name: String,
    pub building_name: String,
    /// What this export is allowed to decide. See [`ExportSettings`].
    pub settings: ExportSettings,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataError {
    DuplicateElementId(BimElementId),
    DuplicateLevelId(BimElementId),
    MissingLevelElevation(BimElementId),
    NonFiniteLevelElevation(BimElementId),
    UnsupportedLevelUnit {
        id: BimElementId,
        unit: Option<String>,
    },
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateElementId(id) => write!(formatter, "duplicate element ID {}", id.0),
            Self::DuplicateLevelId(id) => write!(formatter, "duplicate level ID {}", id.0),
            Self::MissingLevelElevation(id) => {
                write!(formatter, "level {} has no recovered elevation", id.0)
            }
            Self::NonFiniteLevelElevation(id) => {
                write!(formatter, "level {} has a non-finite elevation", id.0)
            }
            Self::UnsupportedLevelUnit { id, unit } => write!(
                formatter,
                "level {} is not expressed in metres (unit: {})",
                id.0,
                unit.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

impl Error for MetadataError {}

/// Build an IFC4 Reference View exchange model with optional element geometry.
///
/// Recognized BIM element types use their corresponding IFC entity; unknown
/// elements remain `IfcBuildingElementProxy`. Elements with a known level are
/// contained by that storey; the rest are contained by the building.
/// Unsupported or unknown units are labels, never guessed numeric measures.
///
/// # Errors
///
/// Returns an error rather than inventing an elevation for any level.
pub fn metadata_ifc(
    model: &BimModel,
    options: &MetadataOptions,
) -> Result<StepFile, MetadataError> {
    metadata_ifc_reported(model, options).map(|(file, _)| file)
}

/// The same file, and what the writer made of the model's solids.
///
/// The tally is the exporter's own funnel: how many bodies were written as a
/// sweep of their profile, and for the rest, which test refused them. A caller
/// that reports it is saying what this file is made of; nothing about the file
/// depends on whether anyone asks.
///
/// # Errors
///
/// The same as [`metadata_ifc`].
pub fn metadata_ifc_reported(
    model: &BimModel,
    options: &MetadataOptions,
) -> Result<(StepFile, SolidReport), MetadataError> {
    validate_model(model)?;
    let mut report = SolidReport::default();
    let lengths = Lengths::new(options.settings.length_unit);
    let mut file = StepFile::new(step_header(options));
    let ownership = push_ownership(&mut file, options.creation_time);
    let context = push_context(&mut file, lengths);
    let units = push_units(&mut file, options.settings.length_unit);
    let project = push_project(&mut file, options, ownership, context, units);
    let origin_axis = push_axis(&mut file, lengths, 0.0);
    let site_placement = file.push(
        "IFCLOCALPLACEMENT",
        vec![StepValue::Omitted, StepValue::Reference(origin_axis)],
    );
    let site = push_site(&mut file, options, ownership, site_placement, model.site.as_ref());
    let building_placement = file.push(
        "IFCLOCALPLACEMENT",
        vec![
            StepValue::Reference(site_placement),
            StepValue::Reference(origin_axis),
        ],
    );
    let building = push_building(&mut file, options, ownership, building_placement);
    push_aggregate(
        &mut file,
        options,
        ownership,
        "project-site",
        project,
        vec![site],
    );
    push_aggregate(
        &mut file,
        options,
        ownership,
        "site-building",
        site,
        vec![building],
    );

    let (storeys, placements) = push_storeys(
        &mut file,
        &model.levels,
        options,
        ownership,
        building_placement,
        lengths,
    );
    if !storeys.is_empty() {
        push_aggregate(
            &mut file,
            options,
            ownership,
            "building-storeys",
            building,
            storeys.values().copied().collect(),
        );
    }
    let elevations = model
        .levels
        .iter()
        .filter_map(|level| Some((level.id.clone(), level.elevation.as_ref()?.value)))
        .collect::<BTreeMap<_, _>>();
    push_elements(
        &mut file,
        &model.elements,
        &mut report,
        WriteContext {
            options,
            owner: ownership,
            lengths,
        },
        Storeys {
            entities: &storeys,
            placements: &placements,
            elevations: &elevations,
            building,
            building_placement,
        },
        context,
        origin_axis,
    );
    Ok((file, report))
}

fn validate_model(model: &BimModel) -> Result<(), MetadataError> {
    let mut ids = BTreeSet::new();
    for element in &model.elements {
        if !ids.insert(&element.id) {
            return Err(MetadataError::DuplicateElementId(element.id.clone()));
        }
    }
    ids.clear();
    for level in &model.levels {
        if !ids.insert(&level.id) {
            return Err(MetadataError::DuplicateLevelId(level.id.clone()));
        }
        let elevation = level
            .elevation
            .as_ref()
            .ok_or_else(|| MetadataError::MissingLevelElevation(level.id.clone()))?;
        if !elevation.value.is_finite() {
            return Err(MetadataError::NonFiniteLevelElevation(level.id.clone()));
        }
        let unit = elevation.unit.as_ref().map(|unit| unit.id.clone());
        if unit.as_deref() != Some("autodesk.unit.unit:meters-1.0.0") {
            return Err(MetadataError::UnsupportedLevelUnit {
                id: level.id.clone(),
                unit,
            });
        }
    }
    Ok(())
}

/// What wrote this file, named the same way everywhere it is named.
const PREPROCESSOR: &str = concat!("Rivet ", env!("CARGO_PKG_VERSION"));

fn step_header(options: &MetadataOptions) -> StepHeader {
    StepHeader {
        description: vec![
            format!(
                "ViewDefinition [{}]",
                options.settings.view_definition.view_definition()
            ),
            // Said in the file itself, because a reader who finds Revit's
            // element ids, Revit's type names and Revit's categories in here
            // would otherwise reasonably take it for Revit's own export. It
            // is not: it is this converter's reading of a Revit model, and
            // what it does not state about that model is missing rather than
            // absent from the model.
            format!(
                "Converter [{PREPROCESSOR}: converted from an Autodesk Revit model, \
                 not exported by Revit]"
            ),
        ],
        file_name: options.file_name.clone(),
        timestamp: options.timestamp.clone(),
        authors: vec!["Rivet".to_owned()],
        organizations: vec!["Rivet".to_owned()],
        preprocessor_version: PREPROCESSOR.to_owned(),
        originating_system: "Rivet".to_owned(),
        authorization: String::new(),
        schema: "IFC4".to_owned(),
    }
}

fn push_ownership(file: &mut StepFile, creation_time: i64) -> EntityRef {
    let person = file.push(
        "IFCPERSON",
        vec![
            omitted(),
            omitted(),
            string("Rivet"),
            omitted(),
            omitted(),
            omitted(),
            omitted(),
            omitted(),
        ],
    );
    let organization = file.push(
        "IFCORGANIZATION",
        vec![omitted(), string("Rivet"), omitted(), omitted(), omitted()],
    );
    let user = file.push(
        "IFCPERSONANDORGANIZATION",
        vec![reference(person), reference(organization), omitted()],
    );
    let application = file.push(
        "IFCAPPLICATION",
        vec![
            reference(organization),
            string(env!("CARGO_PKG_VERSION")),
            string("Rivet"),
            string("RIVET"),
        ],
    );
    file.push(
        "IFCOWNERHISTORY",
        vec![
            reference(user),
            reference(application),
            omitted(),
            enumeration("ADDED"),
            StepValue::Integer(creation_time),
            reference(user),
            reference(application),
            StepValue::Integer(creation_time),
        ],
    )
}

fn push_context(file: &mut StepFile, lengths: Lengths) -> EntityRef {
    let axis = push_axis(file, lengths, 0.0);
    file.push(
        "IFCGEOMETRICREPRESENTATIONCONTEXT",
        vec![
            omitted(),
            string("Model"),
            StepValue::Integer(3),
            // A hundredth of a millimetre, stated in the file's own unit: the
            // precision is a length like any other, and Revit's export of the
            // same models states the same distance.
            lengths.value(1.0e-5),
            reference(axis),
            omitted(),
        ],
    )
}

fn push_axis(file: &mut StepFile, lengths: Lengths, elevation: f64) -> EntityRef {
    let point = file.push(
        "IFCCARTESIANPOINT",
        vec![lengths.coordinates([0.0, 0.0, elevation])],
    );
    file.push(
        "IFCAXIS2PLACEMENT3D",
        vec![reference(point), omitted(), omitted()],
    )
}

fn push_units(file: &mut StepFile, length_unit: LengthUnit) -> EntityRef {
    let mut units = Vec::new();
    for (unit_type, prefix, name) in [
        ("LENGTHUNIT", length_unit.si_prefix(), "METRE"),
        ("AREAUNIT", None, "SQUARE_METRE"),
        ("VOLUMEUNIT", None, "CUBIC_METRE"),
        ("PLANEANGLEUNIT", None, "RADIAN"),
        ("MASSUNIT", Some("KILO"), "GRAM"),
        ("TIMEUNIT", None, "SECOND"),
        ("ELECTRICCURRENTUNIT", None, "AMPERE"),
        ("THERMODYNAMICTEMPERATUREUNIT", None, "KELVIN"),
        ("LUMINOUSINTENSITYUNIT", None, "CANDELA"),
    ] {
        units.push(reference(push_si_unit(file, unit_type, prefix, name)));
    }
    units.extend(push_derived_units(file));
    file.push("IFCUNITASSIGNMENT", vec![StepValue::List(units)])
}

fn push_si_unit(
    file: &mut StepFile,
    unit_type: &str,
    prefix: Option<&str>,
    name: &str,
) -> EntityRef {
    file.push(
        "IFCSIUNIT",
        vec![
            StepValue::Derived,
            enumeration(unit_type),
            prefix.map_or_else(omitted, enumeration),
            enumeration(name),
        ],
    )
}

/// The SI units a derived measure is stated in.
///
/// A property whose spec is a derived quantity - a mass per unit length, a
/// thermal resistance - is written as the IFC measure for that quantity and
/// carries no unit of its own, so the unit it is read in is the one declared
/// here. Revit's own export of AR S1 states its eight the same way: an
/// exponent product of SI units in the assignment, and a bare measure on the
/// property.
///
/// The metre here is the metre whatever the document's length unit is. The
/// values are converted into SI by the catalogue, and a millimetre document
/// does not make a density kilograms per cubic millimetre.
fn push_derived_units(file: &mut StepFile) -> Vec<StepValue> {
    let kilogram = push_si_unit(file, "MASSUNIT", Some("KILO"), "GRAM");
    let metre = push_si_unit(file, "LENGTHUNIT", None, "METRE");
    let second = push_si_unit(file, "TIMEUNIT", None, "SECOND");
    let kelvin = push_si_unit(file, "THERMODYNAMICTEMPERATUREUNIT", None, "KELVIN");
    let mut derived = Vec::new();
    for (unit_type, factors) in [
        ("MASSPERLENGTHUNIT", vec![(kilogram, 1), (metre, -1)]),
        ("MASSDENSITYUNIT", vec![(kilogram, 1), (metre, -3)]),
        ("AREADENSITYUNIT", vec![(kilogram, 1), (metre, -2)]),
        // A square metre kelvin per watt. A watt is a kilogram metre squared
        // per second cubed, so the metres cancel and what is left is kelvin
        // seconds cubed per kilogram.
        (
            "THERMALRESISTANCEUNIT",
            vec![(kilogram, -1), (second, 3), (kelvin, 1)],
        ),
        ("MOMENTOFINERTIAUNIT", vec![(metre, 4)]),
    ] {
        let elements = factors
            .into_iter()
            .map(|(unit, exponent)| {
                reference(file.push(
                    "IFCDERIVEDUNITELEMENT",
                    vec![reference(unit), StepValue::Integer(exponent)],
                ))
            })
            .collect();
        derived.push(reference(file.push(
            "IFCDERIVEDUNIT",
            vec![StepValue::List(elements), enumeration(unit_type), omitted()],
        )));
    }
    derived
}

fn push_project(
    file: &mut StepFile,
    options: &MetadataOptions,
    owner: EntityRef,
    context: EntityRef,
    units: EntityRef,
) -> EntityRef {
    let project = &options.settings.project;
    file.push(
        "IFCPROJECT",
        vec![
            global_id(options, "project"),
            reference(owner),
            string(project.name.as_deref().unwrap_or(&options.project_name)),
            omitted(),
            omitted(),
            optional_string(project.long_name.as_deref()),
            optional_string(project.phase.as_deref()),
            StepValue::List(vec![reference(context)]),
            reference(units),
        ],
    )
}

fn push_site(
    file: &mut StepFile,
    options: &MetadataOptions,
    owner: EntityRef,
    placement: EntityRef,
    site: Option<&bim_core::BimSiteLocation>,
) -> EntityRef {
    let (ref_latitude, ref_longitude, ref_elevation) = site.map_or(
        (omitted(), omitted(), omitted()),
        |site| {
            (
                compound_plane_angle(site.latitude_degrees),
                compound_plane_angle(site.longitude_degrees),
                site.elevation
                    .as_ref()
                    .map_or_else(omitted, |elevation| StepValue::Real(elevation.value)),
            )
        },
    );
    file.push(
        "IFCSITE",
        vec![
            global_id(options, "site"),
            reference(owner),
            string(
                options
                    .settings
                    .project
                    .site_name
                    .as_deref()
                    .unwrap_or(&options.site_name),
            ),
            omitted(),
            omitted(),
            reference(placement),
            omitted(),
            omitted(),
            enumeration("ELEMENT"),
            ref_latitude,
            ref_longitude,
            ref_elevation,
            omitted(),
            omitted(),
        ],
    )
}

/// A decimal angle as `IfcCompoundPlaneAngleMeasure`: degrees, minutes,
/// seconds, and millionths of a second, all sharing the angle's own sign.
///
/// Every component but the last is truncated, not rounded, down to the
/// fractional second - and that one is truncated too, not rounded, which is
/// what keeps this matching Revit's own arithmetic exactly rather than
/// agreeing with it to within a millionth of an arcsecond: rounding the last
/// component lands one high wherever the true value sits in the top half of
/// its millionth, and `s1_revit.ifc`'s own `IfcSite` states a longitude,
/// `(-71,-15,-29,-58837)`, where it does.
fn compound_plane_angle(degrees: f64) -> StepValue {
    let sign = if degrees < 0.0 { -1 } else { 1 };
    let remainder = degrees.abs();
    let whole_degrees = remainder.floor();
    let remainder = (remainder - whole_degrees) * 60.0;
    let minutes = remainder.floor();
    let remainder = (remainder - minutes) * 60.0;
    let seconds = remainder.floor();
    let remainder = (remainder - seconds) * 1_000_000.0;
    let millionths = remainder.floor();
    StepValue::List(
        [whole_degrees, minutes, seconds, millionths]
            .into_iter()
            .map(|component| {
                #[allow(clippy::cast_possible_truncation)]
                // Each component is already floored and bounded well within
                // i64 by the arithmetic above (a degree count, a 0..60
                // minute/second, or a 0..1_000_000 fraction).
                StepValue::Integer(sign * component as i64)
            })
            .collect(),
    )
}

fn push_building(
    file: &mut StepFile,
    options: &MetadataOptions,
    owner: EntityRef,
    placement: EntityRef,
) -> EntityRef {
    let project = &options.settings.project;
    // Revit puts the project's postal address on the building, and so does
    // this - `IfcBuilding.BuildingAddress` is the last of its twelve
    // attributes. Nothing is written where nothing was given.
    let address = project.has_address().then(|| {
        let lines = project.address_lines.iter().map(|line| string(line));
        file.push(
            "IFCPOSTALADDRESS",
            vec![
                omitted(),
                omitted(),
                omitted(),
                omitted(),
                if project.address_lines.is_empty() {
                    omitted()
                } else {
                    StepValue::List(lines.collect())
                },
                omitted(),
                optional_string(project.town.as_deref()),
                optional_string(project.region.as_deref()),
                optional_string(project.postal_code.as_deref()),
                optional_string(project.country.as_deref()),
            ],
        )
    });
    file.push(
        "IFCBUILDING",
        vec![
            global_id(options, "building"),
            reference(owner),
            string(
                project
                    .building_name
                    .as_deref()
                    .unwrap_or(&options.building_name),
            ),
            omitted(),
            omitted(),
            reference(placement),
            omitted(),
            omitted(),
            enumeration("ELEMENT"),
            omitted(),
            omitted(),
            address.map_or_else(omitted, reference),
        ],
    )
}

fn push_storeys(
    file: &mut StepFile,
    levels: &[BimLevel],
    options: &MetadataOptions,
    owner: EntityRef,
    building_placement: EntityRef,
    lengths: Lengths,
) -> (
    BTreeMap<BimElementId, EntityRef>,
    BTreeMap<BimElementId, EntityRef>,
) {
    let mut storeys = BTreeMap::new();
    let mut placements = BTreeMap::new();
    for level in levels {
        let elevation = level
            .elevation
            .as_ref()
            .expect("levels were validated")
            .value;
        let axis = push_axis(file, lengths, elevation);
        let placement = file.push(
            "IFCLOCALPLACEMENT",
            vec![reference(building_placement), reference(axis)],
        );
        let storey = file.push(
            "IFCBUILDINGSTOREY",
            vec![
                global_id(options, &format!("storey:{}", level.id.0)),
                reference(owner),
                string(level.name.as_deref().unwrap_or(&level.id.0)),
                omitted(),
                omitted(),
                reference(placement),
                omitted(),
                omitted(),
                enumeration("ELEMENT"),
                lengths.value(elevation),
            ],
        );
        storeys.insert(level.id.clone(), storey);
        placements.insert(level.id.clone(), placement);
    }
    (storeys, placements)
}

/// The storeys a model's elements stand on, in the three forms the writer
/// asks of them, and the building that holds an element standing on none.
#[derive(Clone, Copy)]
struct Storeys<'a> {
    entities: &'a BTreeMap<BimElementId, EntityRef>,
    placements: &'a BTreeMap<BimElementId, EntityRef>,
    elevations: &'a BTreeMap<BimElementId, f64>,
    building: EntityRef,
    building_placement: EntityRef,
}

/// Where one element stands, and what its body is read into.
struct ElementFrame {
    /// The spatial element that holds it, and the name the relationship
    /// holding it is identified by.
    container: EntityRef,
    container_identity: String,
    /// The placement its own hangs off.
    parent: EntityRef,
    frame: GeometryFrame,
}

impl Storeys<'_> {
    fn of(&self, element: &BimElement, lengths: Lengths) -> ElementFrame {
        let (container, container_identity) = element
            .level_id
            .as_ref()
            .and_then(|id| Some((self.entities.get(id).copied()?, format!("storey:{}", id.0))))
            .unwrap_or((self.building, "building".to_owned()));
        ElementFrame {
            container,
            container_identity,
            parent: element
                .level_id
                .as_ref()
                .and_then(|id| self.placements.get(id).copied())
                .unwrap_or(self.building_placement),
            frame: GeometryFrame {
                lengths,
                storey_elevation: element
                    .level_id
                    .as_ref()
                    .and_then(|id| self.elevations.get(id).copied())
                    .unwrap_or(0.0),
                placement: element
                    .placement
                    .as_ref()
                    .and_then(validated_metric_placement),
            },
        }
    }
}

/// Every table one pass over the model's elements fills in, so
/// [`push_elements`] itself is just the loop and the relationships each table
/// writes once the loop is done.
#[derive(Default)]
struct ElementTables {
    materials: MaterialLibrary,
    types: TypeLibrary,
    // One body per distinct shape, however many elements carry it. See
    // [`BodyWriter`].
    bodies: HashMap<u64, EntityRef>,
    common_sets: CommonPropertySets,
    containment: BTreeMap<String, (EntityRef, Vec<EntityRef>)>,
    // A space is part of the spatial structure, so its storey decomposes it
    // rather than containing it. Kept apart from containment so the two
    // relationships never carry the same product.
    decomposition: BTreeMap<String, (EntityRef, Vec<EntityRef>)>,
    // Every product's entity, by its element's id - see [`track_product`].
    products_by_id: HashMap<String, EntityRef>,
    openings: PendingOpenings,
}

/// What every element of one pass is written against: the spatial tree
/// [`Storeys::of`] places it in, and the two entities every element's
/// geometry is written relative to. Fixed for the whole pass, unlike
/// [`WriteContext`], which every writer in the file needs.
#[derive(Clone, Copy)]
struct PassContext<'a> {
    storeys: &'a Storeys<'a>,
    representation_context: EntityRef,
    origin_axis: EntityRef,
}

impl ElementTables {
    /// Write one element and fold what it contributes into every table above.
    fn visit(
        &mut self,
        file: &mut StepFile,
        element: &BimElement,
        report: &mut SolidReport,
        context: WriteContext<'_>,
        pass: PassContext<'_>,
    ) {
        let PassContext {
            storeys,
            representation_context,
            origin_axis,
        } = pass;
        // What the element is written as is settled before anything is
        // written: a category the mapping table keeps out of the file leaves
        // nothing behind it - no product, no placement, no property set and no
        // type - and deciding afterwards would leave the placement stranded.
        let Some(written_as) = write_as(element, context, report) else {
            return;
        };
        let ElementFrame {
            container,
            container_identity,
            parent,
            frame,
        } = storeys.of(element, context.lengths);
        let placement = push_element_placement(file, parent, origin_axis, frame);
        let geometry_context = ElementGeometryContext {
            representation_context,
            frame,
        };
        let spatial = written_as.name == "IFCSPACE";
        let mut writer = BodyWriter {
            maps: context
                .options
                .settings
                .shared_bodies
                .then_some(&mut self.bodies),
            report,
            used_map: None,
            written_faces: Vec::new(),
        };
        let written = if spatial {
            push_space(
                file,
                element,
                context,
                placement,
                geometry_context,
                &mut writer,
            )
        } else {
            push_element(
                file,
                element,
                &written_as,
                context,
                placement,
                geometry_context,
                &mut writer,
            )
        };
        let entity = written.entity;
        let body_map = writer.used_map;
        let written_faces = writer.written_faces;
        track_product(
            &mut self.products_by_id,
            &mut self.openings,
            element,
            placement,
            written,
        );
        if spatial {
            &mut self.decomposition
        } else {
            &mut self.containment
        }
        .entry(container_identity)
        .or_insert_with(|| (container, Vec::new()))
        .1
        .push(entity);
        // The type comes first: where it holds the type's parameters, the
        // element does not repeat them.
        let type_carries_properties = context.options.settings.types
            && self
                .types
                .associate(file, element, entity, written_as.name, body_map, context);
        push_property_set(file, element, entity, context, type_carries_properties);
        if context.options.settings.property_sets.ifc_common {
            self.common_sets
                .associate(file, element, entity, written_as.name, context);
        }
        if context.options.settings.property_sets.base_quantities {
            push_quantities(file, element, entity, written_as.name, context);
        }
        if !spatial {
            self.materials
                .associate(file, element, entity, context.lengths);
            self.materials
                .associate_face_material(file, element, entity, &written_faces);
        }
    }

    /// The relationships every table above holds until every element has
    /// been visited: a type or a material is shared, so it and what relates
    /// to it are written once here rather than once per element that carries
    /// it, and an opening's host may be named by an element visited before
    /// the host itself was.
    fn finish(self, file: &mut StepFile, context: WriteContext<'_>) {
        self.common_sets.push_relations(file, context);
        self.types.push_relations(file, context);
        self.materials
            .push_associations(file, context.options, context.owner);
        if context.options.settings.openings {
            self.openings.push(file, &self.products_by_id, context);
        }
        for (identity, (container, elements)) in self.containment {
            push_containment(
                file,
                context.options,
                context.owner,
                &identity,
                container,
                elements,
            );
        }
        for (identity, (container, spaces)) in self.decomposition {
            push_aggregate(
                file,
                context.options,
                context.owner,
                &format!("spaces:{identity}"),
                container,
                spaces,
            );
        }
    }
}

fn push_elements(
    file: &mut StepFile,
    elements: &[BimElement],
    report: &mut SolidReport,
    context: WriteContext<'_>,
    storeys: Storeys<'_>,
    representation_context: EntityRef,
    origin_axis: EntityRef,
) {
    let pass = PassContext {
        storeys: &storeys,
        representation_context,
        origin_axis,
    };
    let mut tables = ElementTables::default();
    for element in elements {
        tables.visit(file, element, report, context, pass);
    }
    tables.finish(file, context);
}

/// What every writer here needs beside the thing it is writing: this export's
/// settings and identity, the owner history every entity points at, and the
/// unit its lengths go out in. They travel together everywhere, so they are
/// one value rather than three arguments repeated down the file.
#[derive(Clone, Copy)]
struct WriteContext<'a> {
    options: &'a MetadataOptions,
    owner: EntityRef,
    lengths: Lengths,
}

/// Write one `IfcSpace`. It is a spatial structure element, not an element:
/// where `IfcElement` ends its eight attributes with `Tag`, this carries
/// `LongName`, `CompositionType` and its own `PredefinedType`, so it cannot go
/// through the generic writer.
///
/// Revit's own export names a space by its number and puts the room's name in
/// `LongName`, and both are on the record; the same split is written here.
/// `PredefinedType` stays `NOTDEFINED` because nothing in the source says
/// which kind of space this is.
fn push_space(
    file: &mut StepFile,
    element: &BimElement,
    context: WriteContext<'_>,
    placement: EntityRef,
    geometry_context: ElementGeometryContext,
    writer: &mut BodyWriter<'_>,
) -> WrittenProduct {
    let representation = element.geometry.as_ref().and_then(|geometry| {
        push_geometry(
            file,
            geometry,
            geometry_context.representation_context,
            geometry_context.frame,
            writer,
        )
    });
    let entity = file.push(
        "IFCSPACE",
        vec![
            global_id(context.options, &format!("element:{}", element.id.0)),
            reference(context.owner),
            string(element.name.as_deref().unwrap_or(&element.id.0)),
            omitted(),
            optional_string(element.class_name.as_deref()),
            reference(placement),
            representation.map_or_else(omitted, reference),
            optional_string(element.long_name.as_deref()),
            enumeration("ELEMENT"),
            enumeration("NOTDEFINED"),
            omitted(),
        ],
    );
    WrittenProduct {
        entity,
        representation,
    }
}

#[derive(Clone, Copy)]
struct MetricPlacement {
    origin: [f64; 3],
    reference_direction: [f64; 3],
    axis: [f64; 3],
}

/// The unit every length in the file is written in, and the factor from the
/// metres the model carries. The model is metric throughout and stays that
/// way; this is applied where a length becomes a number in the file, so that
/// every comparison and tolerance above it is still in metres.
#[derive(Clone, Copy)]
struct Lengths {
    per_metre: f64,
}

impl Lengths {
    const fn new(unit: LengthUnit) -> Self {
        Self {
            per_metre: unit.per_metre(),
        }
    }

    /// One length, in the file's unit.
    fn value(self, metres: f64) -> StepValue {
        StepValue::Real(metres * self.per_metre)
    }

    /// A point's coordinates, in the file's unit.
    fn coordinates<const N: usize>(self, metres: [f64; N]) -> StepValue {
        StepValue::List(metres.into_iter().map(|value| self.value(value)).collect())
    }
}

/// Where a body's coordinates are read from, and what they are written in.
/// The two placement fields travel together everywhere geometry is written, so
/// they are one value; the unit rides with them because it is needed at the
/// same points and nowhere else.
#[derive(Clone, Copy)]
struct GeometryFrame {
    lengths: Lengths,
    /// The storey elevation the body's coordinates are made relative to.
    storey_elevation: f64,
    placement: Option<MetricPlacement>,
}

#[derive(Clone, Copy)]
struct ElementGeometryContext {
    representation_context: EntityRef,
    frame: GeometryFrame,
}

fn validated_metric_placement(placement: &BimPlacement) -> Option<MetricPlacement> {
    let origin = metric_coordinates(&placement.origin)?;
    let reference_direction = placement.reference_direction;
    let axis = placement.axis;
    let norm_squared = |direction: [f64; 3]| {
        direction
            .into_iter()
            .map(|value| value * value)
            .sum::<f64>()
    };
    if reference_direction
        .into_iter()
        .chain(axis)
        .any(|value| !value.is_finite())
        || (norm_squared(reference_direction) - 1.0).abs() > 1.0e-8
        || (norm_squared(axis) - 1.0).abs() > 1.0e-8
        || dot(reference_direction, axis).abs() > 1.0e-8
    {
        return None;
    }
    Some(MetricPlacement {
        origin,
        reference_direction,
        axis,
    })
}

fn push_element_placement(
    file: &mut StepFile,
    parent_placement: EntityRef,
    origin_axis: EntityRef,
    frame: GeometryFrame,
) -> EntityRef {
    let relative = frame.placement.map_or(origin_axis, |placement| {
        let mut origin = placement.origin;
        origin[2] -= frame.storey_elevation;
        let point = push_cartesian_point(file, frame.lengths, origin);
        let axis = push_direction(file, placement.axis);
        let reference_direction = push_direction(file, placement.reference_direction);
        file.push(
            "IFCAXIS2PLACEMENT3D",
            vec![
                reference(point),
                reference(axis),
                reference(reference_direction),
            ],
        )
    });
    file.push(
        "IFCLOCALPLACEMENT",
        vec![reference(parent_placement), reference(relative)],
    )
}

/// A product this pass wrote, and the shape behind it - kept so a later pass
/// over the same elements, such as [`push_openings`], can place an
/// `IfcOpeningElement` through the same representation rather than deriving
/// one of its own.
#[derive(Clone, Copy)]
struct WrittenProduct {
    entity: EntityRef,
    representation: Option<EntityRef>,
}

fn push_element(
    file: &mut StepFile,
    element: &BimElement,
    written_as: &ResolvedEntity<'_>,
    context: WriteContext<'_>,
    placement: EntityRef,
    geometry_context: ElementGeometryContext,
    writer: &mut BodyWriter<'_>,
) -> WrittenProduct {
    let object_type = element.class_name.as_deref().or_else(|| {
        element
            .category
            .as_ref()
            .map(|category| category.name.as_str())
    });
    let entity = written_as.name;
    let representation = element.geometry.as_ref().and_then(|geometry| {
        push_geometry(
            file,
            geometry,
            geometry_context.representation_context,
            geometry_context.frame,
            writer,
        )
    });
    let mut attributes = vec![
        global_id(context.options, &format!("element:{}", element.id.0)),
        reference(context.owner),
        string(element.name.as_deref().unwrap_or(&element.id.0)),
        omitted(),
        optional_string(object_type),
        reference(placement),
        representation.map_or_else(omitted, reference),
        string(&element.id.0),
    ];
    // STEP writes every attribute an entity declares, set or not, and what
    // each entity declares past `Tag` comes from the schema itself - see
    // `ifc4_entities`. A window's `OverallHeight` is not read from the source
    // and goes in unset; its `PredefinedType` goes in as the enumeration
    // member that says so.
    attributes.extend(declared_attributes(IFC4_ELEMENTS, entity));
    if let Some(predefined_type) = written_as.predefined_type {
        set_predefined_type(IFC4_ELEMENTS, entity, &mut attributes, predefined_type);
    }
    WrittenProduct {
        entity: file.push(entity, attributes),
        representation,
    }
}

/// What the element is written as, or `None` where it is not written at all.
///
/// Two things keep an element out of the file: a category the mapping table
/// names `Not Exported`, and - unless the setup asks otherwise - a body this
/// export does not carry. Both are settled before anything is written,
/// because an element held back afterwards would leave a placement, a
/// property set and a containment behind it.
fn write_as<'a>(
    element: &BimElement,
    context: WriteContext<'a>,
    report: &mut SolidReport,
) -> Option<ResolvedEntity<'a>> {
    let written_as = resolve_entity(element, context.options.settings.class_mapping())?;
    if context.options.settings.elements_without_a_body
        || written_as.name == "IFCSPACE"
        || carries_a_body(element)
    {
        return Some(written_as);
    }
    report.count_bodiless();
    None
}

/// Whether this export carries a solid for the element.
///
/// A decoded boundary representation or an assembly of them is a body; a
/// swept disk is the pipe it describes. A bounding box is not - it is what is
/// known about where the element is - and neither is an axis line on its own,
/// nor no geometry at all.
fn carries_a_body(element: &BimElement) -> bool {
    match &element.geometry {
        Some(BimGeometry::SweptDisk(_)) => true,
        // A shell the decode read no face of is not a body, complete or not:
        // what would be written for it is an empty representation.
        Some(BimGeometry::Brep(brep)) => !brep.faces.is_empty(),
        Some(BimGeometry::Assembly(bodies)) => bodies.iter().any(|brep| !brep.faces.is_empty()),
        Some(BimGeometry::AxisLine(_) | BimGeometry::BoundingBox(_)) | None => false,
    }
}

/// What an element is written as. The class mapping table decides where it
/// names the element's category; otherwise it is what [`ifc_entity_name`]
/// says, with the kind left for the entity's own `NOTDEFINED`.
struct ResolvedEntity<'a> {
    name: &'a str,
    predefined_type: Option<&'a str>,
}

/// The entity an element is written as, or `None` where the mapping table says
/// the category is not exported.
fn resolve_entity<'a>(
    element: &BimElement,
    mapping: Option<&'a ClassMapping>,
) -> Option<ResolvedEntity<'a>> {
    let mapped = mapping.and_then(|mapping| {
        let category = element.category.as_ref()?;
        mapping.lookup(
            Some(category.name.as_str()),
            category.id.as_ref().map(|id| id.value.as_str()),
        )
    });
    match mapped {
        Some(Mapped::NotExported) => None,
        Some(Mapped::Entity {
            name,
            predefined_type,
        }) => Some(ResolvedEntity {
            name: name.as_str(),
            predefined_type: predefined_type.as_deref(),
        }),
        None => Some(ResolvedEntity {
            name: ifc_entity_name(resolved_element_type(element)),
            predefined_type: None,
        }),
    }
}

/// Put a mapped `PredefinedType` in the slot the schema gives it. The value
/// was checked against the entity's own enumeration when the table was read,
/// so a member that is not the entity's cannot reach here.
fn set_predefined_type(
    table: &'static [Ifc4Entity],
    entity: &str,
    attributes: &mut [StepValue],
    value: &str,
) {
    let Some(declared) = table
        .iter()
        .find(|candidate| candidate.name == entity)
        .and_then(|candidate| candidate.predefined_type)
    else {
        return;
    };
    // The declared attributes follow the base ones this writer put in first.
    let base = attributes.len()
        - table
            .iter()
            .find(|candidate| candidate.name == entity)
            .map_or(0, |candidate| candidate.attributes.len());
    if let Some(slot) = attributes.get_mut(base + declared.index) {
        *slot = enumeration(value);
    }
}

/// The attributes an entity declares past the base ones, written the way the
/// schema asks: an enumeration that can say "not stated" says it, and anything
/// else this exporter does not read from the source is left unset.
///
/// An entity the table does not hold declares nothing here, which is the
/// conservative answer: `push` would then write the base attributes alone and
/// a reader would see a truncated entity, so every entity this exporter names
/// is checked to be in the table by `every_entity_this_exporter_writes_is_in_the_schema_table`.
fn declared_attributes(table: &'static [Ifc4Entity], name: &str) -> Vec<StepValue> {
    table
        .iter()
        .find(|entity| entity.name == name)
        .map(|entity| {
            entity
                .attributes
                .iter()
                .map(|attribute| match attribute {
                    Attribute::Notdefined => enumeration("NOTDEFINED"),
                    Attribute::Optional | Attribute::Required => omitted(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The type entity that stands behind a product, where the schema has one.
///
/// IFC4 names a type after its element - `IfcWall` and `IfcWallType`,
/// `IfcPipeSegment` and `IfcPipeSegmentType` - and that rule holds for every
/// one of the 130 instantiable elements: the 104 that have a type are named
/// this way without exception, and for the other 26 the name simply is not in
/// the schema. So the rule is applied and the answer checked against the
/// table, which is what keeps `IfcDistributionFlowElementType` - declared
/// ABSTRACT, and refused by `ifcopenshell.validate` on 26 elements of SMALL -
/// out of the file, along with the standard-case entities that have no type.
///
/// The attribute layout comes from the same table, and agrees with Revit's own
/// export of AR S1 entity for entity: ten attributes on an `IfcWallType`,
/// eleven on an `IfcSpaceType` with its `LongName`, thirteen on an
/// `IfcDoorType` and an `IfcWindowType`.
fn type_entity_for(entity: &str) -> Option<(&'static str, &'static [Ifc4Entity])> {
    // A space's type is a spatial element type, not an element type, and lives
    // in its own table.
    let table: &'static [Ifc4Entity] = if entity == "IFCSPACE" {
        IFC4_SPATIAL_ELEMENT_TYPES
    } else {
        IFC4_ELEMENT_TYPES
    };
    let name = format!("{entity}TYPE");
    table
        .iter()
        .find(|candidate| candidate.name == name)
        .map(|candidate| (candidate.name, table))
}

/// The types written so far, and the products defined by each.
///
/// A type belongs to many elements - 13 193 walls of AR S1 share 111 wall
/// types - so it is written once, keyed by the record it was read from, and
/// one `IfcRelDefinesByType` at the end relates every product of it. That is
/// also where the type's own parameters go: they hold for every element of the
/// type, and writing them on each of thousands of products repeats them
/// thousands of times.
struct TypeEntry {
    table: &'static [Ifc4Entity],
    identity: String,
    name: Option<String>,
    type_id: String,
    properties: Option<EntityRef>,
    products: Vec<EntityRef>,
    /// The `IfcRepresentationMap`s the type's own products were written
    /// through, in the order first seen. A type is written once its whole
    /// loop is done - see [`TypeLibrary::push_relations`] - so this is filled
    /// in as each product is associated and only read once every element has
    /// been visited.
    body_maps: Vec<EntityRef>,
    /// Whether the type was written holding its own parameters.
    carries_properties: bool,
}

#[derive(Default)]
struct TypeLibrary {
    /// `(type record, entity)` to the type and the products defined by it. The
    /// entity is part of the key because one record may reach the export as
    /// two different products only if the decode disagrees with itself; a
    /// shared key would then put a wall and a slab under one wall type.
    types: BTreeMap<(String, &'static str), TypeEntry>,
}

impl TypeLibrary {
    /// Record that `product` is of `element`'s type, writing the type the
    /// first time it is seen. Returns whether the type carries the element's
    /// type parameters, so the element does not repeat them.
    fn associate(
        &mut self,
        file: &mut StepFile,
        element: &BimElement,
        product: EntityRef,
        product_entity: &str,
        body_map: Option<EntityRef>,
        context: WriteContext<'_>,
    ) -> bool {
        // A space names no family type - Revit has none to declare - but
        // Revit's own export still gives every space its own `IfcSpaceType`,
        // named after the room rather than shared, and that alone is over a
        // third of the type-relation count measured against AR S1. Matched
        // here the same way: the element's own id stands in for a type it
        // does not have, so each space still reaches exactly one type of its
        // own.
        let (type_id, synthetic_name) = match element.type_id.as_ref() {
            Some(type_id) => (type_id, None),
            None if product_entity == "IFCSPACE" => (
                &element.id,
                element.long_name.as_deref().or(element.name.as_deref()),
            ),
            None => return false,
        };
        let Some((entity, table)) = type_entity_for(product_entity) else {
            return false;
        };
        let key = (type_id.0.clone(), entity);
        let entry = match self.types.entry(key) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let identity = format!("type:{}:{}", type_id.0, entity);
                let properties = context
                    .options
                    .settings
                    .property_sets
                    .revit_type_parameters
                    .then(|| {
                        push_property_set_entity(
                            file,
                            &element.type_properties,
                            context,
                            "Rivet Type Properties",
                            &format!("type-properties:{}", type_id.0),
                        )
                    })
                    .flatten();
                entry.insert(TypeEntry {
                    table,
                    identity,
                    name: element
                        .type_name
                        .clone()
                        .or_else(|| synthetic_name.map(str::to_owned)),
                    type_id: type_id.0.clone(),
                    properties,
                    products: Vec::new(),
                    body_maps: Vec::new(),
                    carries_properties: properties.is_some(),
                })
            }
        };
        entry.products.push(product);
        // The same body fingerprints to the same map every time - see
        // `FINGERPRINT_GRID` - so a type only ever collects a second map here
        // when its instances genuinely disagree on their body.
        if let Some(map) = body_map {
            if !entry.body_maps.contains(&map) {
                entry.body_maps.push(map);
            }
        }
        // Type parameters are read from the type record, so every element of
        // one carries the same ones and the set on the type states them all.
        // An element whose parameters are *not* on the type says so, and
        // writes its own set as it did before types were exported.
        entry.carries_properties || element.type_properties.is_empty()
    }

    /// The type entity itself, held back until now because
    /// `RepresentationMaps` is only complete once every product of the type
    /// has been visited - see [`TypeEntry::body_maps`] - followed by one
    /// `IfcRelDefinesByType` relating every product to it.
    fn push_relations(self, file: &mut StepFile, context: WriteContext<'_>) {
        for ((type_id, entity), entry) in self.types {
            if entry.products.is_empty() {
                continue;
            }
            let mut attributes = vec![
                global_id(context.options, &entry.identity),
                reference(context.owner),
                optional_string(entry.name.as_deref()),
                omitted(),
                omitted(),
                entry
                    .properties
                    .map_or_else(omitted, |pset| StepValue::List(vec![reference(pset)])),
                if entry.body_maps.is_empty() {
                    omitted()
                } else {
                    StepValue::List(entry.body_maps.iter().copied().map(reference).collect())
                },
                string(&entry.type_id),
                omitted(),
            ];
            attributes.extend(declared_attributes(entry.table, entity));
            let written = file.push(entity, attributes);
            file.push(
                "IFCRELDEFINESBYTYPE",
                vec![
                    global_id(
                        context.options,
                        &format!("type-relation:{type_id}:{entity}"),
                    ),
                    reference(context.owner),
                    omitted(),
                    omitted(),
                    StepValue::List(entry.products.into_iter().map(reference).collect()),
                    reference(written),
                ],
            );
        }
    }
}

/// Record what an element's product is, so a later element naming it as a
/// host can find it, and - where the element names a host of its own -
/// queue it as a candidate for [`PendingOpenings::push`].
fn track_product(
    products_by_id: &mut HashMap<String, EntityRef>,
    openings: &mut PendingOpenings,
    element: &BimElement,
    placement: EntityRef,
    written: WrittenProduct,
) {
    products_by_id.insert(element.id.0.clone(), written.entity);
    if let Some(host_id) = &element.host_id {
        openings.candidates.push(OpeningCandidate {
            host_id: host_id.0.clone(),
            fenestration_id: element.id.0.clone(),
            fenestration: written.entity,
            name: element.name.clone(),
            placement,
            representation: written.representation,
        });
    }
}

/// A door, a window, or anything else that names a host, waiting on that
/// host's own product to exist. Elements are written in one pass over the
/// model, so the host of an element named earlier in it may not have a
/// product yet - resolved instead once every element has one, in
/// [`PendingOpenings::push`].
struct OpeningCandidate {
    host_id: String,
    fenestration_id: String,
    fenestration: EntityRef,
    name: Option<String>,
    placement: EntityRef,
    representation: Option<EntityRef>,
}

#[derive(Default)]
struct PendingOpenings {
    candidates: Vec<OpeningCandidate>,
}

impl PendingOpenings {
    /// One `IfcOpeningElement` per candidate whose host wrote a product,
    /// placed and shaped exactly as the element that fills it. That is not
    /// what Revit's own export states - it extrudes the fenestration's own
    /// footprint through the host's thickness, a profile this export does not
    /// derive - but it is an honest statement of a volume this export has
    /// already verified, where a guessed one would not be. Related to the
    /// host by `IfcRelVoidsElement` and to the element by
    /// `IfcRelFillsElement`.
    ///
    /// A host this export did not write a product for - held out of the file
    /// by the class mapping table, say - leaves its opening out too: a void
    /// in nothing is not a fact about the model.
    fn push(
        self,
        file: &mut StepFile,
        products_by_id: &HashMap<String, EntityRef>,
        context: WriteContext<'_>,
    ) {
        for candidate in self.candidates {
            let Some(&host) = products_by_id.get(&candidate.host_id) else {
                continue;
            };
            let opening = file.push(
                "IFCOPENINGELEMENT",
                vec![
                    global_id(
                        context.options,
                        &format!("opening:{}", candidate.fenestration_id),
                    ),
                    reference(context.owner),
                    optional_string(candidate.name.as_deref()),
                    omitted(),
                    omitted(),
                    reference(candidate.placement),
                    candidate.representation.map_or_else(omitted, reference),
                    string(&candidate.fenestration_id),
                    enumeration("OPENING"),
                ],
            );
            file.push(
                "IFCRELVOIDSELEMENT",
                vec![
                    global_id(
                        context.options,
                        &format!("voids:{}", candidate.fenestration_id),
                    ),
                    reference(context.owner),
                    omitted(),
                    omitted(),
                    reference(host),
                    reference(opening),
                ],
            );
            file.push(
                "IFCRELFILLSELEMENT",
                vec![
                    global_id(
                        context.options,
                        &format!("fills:{}", candidate.fenestration_id),
                    ),
                    reference(context.owner),
                    omitted(),
                    omitted(),
                    reference(opening),
                    reference(candidate.fenestration),
                ],
            );
        }
    }
}

fn push_geometry(
    file: &mut StepFile,
    geometry: &BimGeometry,
    representation_context: EntityRef,
    frame: GeometryFrame,
    writer: &mut BodyWriter<'_>,
) -> Option<EntityRef> {
    match geometry {
        BimGeometry::AxisLine(line) => {
            let (_, axis) = push_axis_line(file, line, representation_context, frame)?;
            Some(file.push(
                "IFCPRODUCTDEFINITIONSHAPE",
                vec![omitted(), omitted(), StepValue::List(vec![reference(axis)])],
            ))
        }
        BimGeometry::SweptDisk(swept_disk) => {
            if swept_disk.radius.unit.as_ref()?.id != "autodesk.unit.unit:meters-1.0.0"
                || !swept_disk.radius.value.is_finite()
                || swept_disk.radius.value <= 0.0
            {
                return None;
            }
            let (directrix, axis) =
                push_axis_line(file, &swept_disk.directrix, representation_context, frame)?;
            let solid = file.push(
                "IFCSWEPTDISKSOLID",
                vec![
                    reference(directrix),
                    frame.lengths.value(swept_disk.radius.value),
                    omitted(),
                    omitted(),
                    omitted(),
                ],
            );
            let body = file.push(
                "IFCSHAPEREPRESENTATION",
                vec![
                    reference(representation_context),
                    string("Body"),
                    string("AdvancedSweptSolid"),
                    StepValue::List(vec![reference(solid)]),
                ],
            );
            Some(file.push(
                "IFCPRODUCTDEFINITIONSHAPE",
                vec![
                    omitted(),
                    omitted(),
                    StepValue::List(vec![reference(axis), reference(body)]),
                ],
            ))
        }
        BimGeometry::BoundingBox(bounds) => {
            push_bounding_box(file, bounds, representation_context, frame)
        }
        BimGeometry::Brep(brep) => push_brep(file, brep, representation_context, frame, writer),
        BimGeometry::Assembly(parts) => {
            push_assembly(file, parts, representation_context, frame, writer)
        }
    }
}

/// The grid a coordinate is fingerprinted on: a nanometre, in the metres
/// every body is carried in.
///
/// Two instances of one symbol do not reach this with bit-identical numbers.
/// The body arrives in world coordinates, having been carried there through
/// the instance's own transform, and is carried back into the element's frame
/// here; a rotation and its inverse leave the last bits of a double
/// disagreeing. A nanometre is some seven orders of magnitude coarser than
/// that error on a building-sized coordinate, and some six orders finer than
/// anything a model distinguishes, so no two bodies that differ agree on it
/// and no two instances of one body disagree.
const FINGERPRINT_GRID: f64 = 1e12;

/// One body's fingerprint: everything the writer below would state about it,
/// in the element's own frame.
///
/// `None` where any of it cannot be read - a coordinate in a unit that is not
/// the model's metre, a frame that states no elevation - which is exactly
/// where the writer refuses the body too.
fn brep_fingerprint(brep: &BimBrep, frame: GeometryFrame) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    brep.complete.hash(&mut hasher);
    hash_brep(&mut hasher, brep, frame)?;
    Some(hasher.finish())
}

/// The same for the several closed bodies that are one element.
fn assembly_fingerprint(parts: &[BimBrep], frame: GeometryFrame) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    parts.len().hash(&mut hasher);
    for part in parts {
        part.complete.hash(&mut hasher);
        hash_brep(&mut hasher, part, frame)?;
    }
    Some(hasher.finish())
}

fn hash_brep(hasher: &mut impl Hasher, brep: &BimBrep, frame: GeometryFrame) -> Option<()> {
    brep.faces.len().hash(hasher);
    for face in &brep.faces {
        hash_surface(hasher, &face.surface, frame)?;
        face.loops.len().hash(hasher);
        for edges in &face.loops {
            edges.len().hash(hasher);
            for edge in edges {
                hash_point(hasher, &edge.start, frame)?;
                hash_point(hasher, &edge.end, frame)?;
                hash_curve(hasher, &edge.curve, frame)?;
            }
        }
    }
    Some(())
}

/// Every variant is spelled out rather than collapsed, and each states a tag
/// of its own, so that a surface added to the model cannot quietly fingerprint
/// the same as one already here: the compiler asks for the new arm.
fn hash_surface(
    hasher: &mut impl Hasher,
    surface: &BimBrepSurface,
    frame: GeometryFrame,
) -> Option<()> {
    match surface {
        BimBrepSurface::Plane {
            origin,
            x_axis,
            y_axis,
        } => {
            0_u8.hash(hasher);
            hash_point(hasher, origin, frame)?;
            hash_direction(hasher, *x_axis, frame);
            hash_direction(hasher, *y_axis, frame);
        }
        BimBrepSurface::Cylinder {
            center,
            x_axis,
            y_axis,
            z_axis,
            radius,
        } => {
            1_u8.hash(hasher);
            hash_point(hasher, center, frame)?;
            for axis in [x_axis, y_axis, z_axis] {
                hash_direction(hasher, *axis, frame);
            }
            hash_metres(hasher, radius)?;
        }
        BimBrepSurface::Revolution {
            center,
            x_axis,
            y_axis,
            z_axis,
            profile,
        } => {
            2_u8.hash(hasher);
            hash_point(hasher, center, frame)?;
            for axis in [x_axis, y_axis, z_axis] {
                hash_direction(hasher, *axis, frame);
            }
            hash_profile(hasher, profile, frame)?;
        }
        BimBrepSurface::Ruled { first, second } => {
            3_u8.hash(hasher);
            for ruling in [first, second] {
                match ruling {
                    BimBrepRuling::Point(point) => {
                        0_u8.hash(hasher);
                        hash_point(hasher, point, frame)?;
                    }
                    BimBrepRuling::Curve {
                        profile,
                        start,
                        end,
                    } => {
                        1_u8.hash(hasher);
                        hash_profile(hasher, profile, frame)?;
                        hash_number(hasher, *start);
                        hash_number(hasher, *end);
                    }
                }
            }
        }
    }
    Some(())
}

fn hash_profile(
    hasher: &mut impl Hasher,
    profile: &BimBrepProfile,
    frame: GeometryFrame,
) -> Option<()> {
    match profile {
        BimBrepProfile::Line { origin, direction } => {
            0_u8.hash(hasher);
            hash_point(hasher, origin, frame)?;
            hash_direction(hasher, *direction, frame);
        }
        BimBrepProfile::Arc {
            center,
            x_axis,
            y_axis,
            radius,
        } => {
            1_u8.hash(hasher);
            hash_point(hasher, center, frame)?;
            hash_direction(hasher, *x_axis, frame);
            hash_direction(hasher, *y_axis, frame);
            hash_metres(hasher, radius)?;
        }
    }
    Some(())
}

fn hash_curve(hasher: &mut impl Hasher, curve: &BimBrepCurve, frame: GeometryFrame) -> Option<()> {
    match curve {
        BimBrepCurve::Line => 0_u8.hash(hasher),
        BimBrepCurve::Arc(arc) => {
            1_u8.hash(hasher);
            hash_point(hasher, &arc.center, frame)?;
            hash_direction(hasher, arc.x_axis, frame);
            hash_direction(hasher, arc.z_axis, frame);
            hash_metres(hasher, &arc.radius)?;
            hash_number(hasher, arc.start_angle);
            hash_number(hasher, arc.end_angle);
        }
        BimBrepCurve::Polyline(points) => {
            2_u8.hash(hasher);
            points.len().hash(hasher);
            for point in points {
                hash_point(hasher, point, frame)?;
            }
        }
    }
    Some(())
}

fn hash_point(hasher: &mut impl Hasher, point: &BimPoint3, frame: GeometryFrame) -> Option<()> {
    for value in local_coordinates(point, frame)? {
        hash_number(hasher, value);
    }
    Some(())
}

fn hash_direction(hasher: &mut impl Hasher, direction: [f64; 3], frame: GeometryFrame) {
    for value in local_direction(direction, frame.placement) {
        hash_number(hasher, value);
    }
}

fn hash_metres(hasher: &mut impl Hasher, number: &BimNumber) -> Option<()> {
    let metres = number
        .unit
        .as_ref()
        .filter(|unit| unit.id == "autodesk.unit.unit:meters-1.0.0")
        .map(|_| number.value)?;
    hash_number(hasher, metres);
    Some(())
}

/// A number on the grid above. A value too large for the grid - which no
/// coordinate of a building is - keeps its own bits instead, so that two
/// different ones cannot collapse to the same saturated integer.
fn hash_number(hasher: &mut impl Hasher, value: f64) {
    let scaled = value * FINGERPRINT_GRID;
    if scaled.abs() < 9.0e18 {
        #[allow(clippy::cast_possible_truncation)]
        // Guarded above: the value is within the range an i64 holds exactly
        // enough of for a grid this coarse.
        (scaled.round() as i64).hash(hasher);
    } else {
        value.to_bits().hash(hasher);
    }
}

/// The bodies written so far, each kept behind the `IfcRepresentationMap`
/// that places it, and the tally of what the writer made of them.
///
/// A family places one symbol's solid over and over - 475 windows, a thousand
/// balusters - and it is the same body every time: this export writes an
/// element's geometry in the element's *own* frame, so two instances of one
/// symbol produce the same coordinates whatever their placement in the
/// building is. Written out per instance, that repeats every face, every loop
/// and every oriented edge once per placement, because the topology above an
/// edge is deliberately not shared between products - see `SHARED_BY_VALUE`
/// in `lib.rs`, and the reason given beside it.
///
/// So each distinct body is written once, wrapped in an
/// `IfcRepresentationMap`, and every element carrying that body reaches it
/// through an `IfcMappedItem` - which is what a reader expects of a type's
/// occurrences, and what Revit's own export does for its families.
struct BodyWriter<'a> {
    /// The map each distinct body is reached through, by the fingerprint of
    /// the body itself. Never iterated, so the file stays deterministic
    /// whatever order a hash map holds its keys in.
    ///
    /// `None` where the setup asks for every element to carry its own body.
    maps: Option<&'a mut HashMap<u64, EntityRef>>,
    report: &'a mut SolidReport,
    /// The `IfcRepresentationMap` the body just written was reached through,
    /// new or reused - read back by the caller once geometry has been
    /// written, so the element's type can list it under
    /// `IfcTypeProduct.RepresentationMaps`. `None` where the element wrote no
    /// mapped body: bodies are not shared, or the geometry is not a body at
    /// all (a bounding box, an axis line, a swept disk).
    used_map: Option<EntityRef>,
    /// The `IfcAdvancedFace`/`IfcFace` each face of the body just written
    /// became, in the same order [`element_faces`] would read them back in -
    /// `None` for a face that failed to write. Read back the same way as
    /// [`Self::used_map`], to style a face whose product's faces disagree on
    /// material - see [`MaterialLibrary::style_disagreeing_faces`].
    ///
    /// Left empty where the body took the swept-solid path: a recognised
    /// prism writes no per-face entity at all, so there is nothing here to
    /// style. Also empty for the second and later element sharing one mapped
    /// body, since [`push_mapped_body`]'s closure - the only place this is
    /// filled in - does not run again for them; such an element's faces keep
    /// whatever styling the body's first writer gave them.
    written_faces: Vec<Option<EntityRef>>,
}

/// One body's shape representation, placed through a map so that the next
/// element carrying the same body writes no geometry at all.
///
/// `fingerprint` is `None` for a body this cannot fingerprint - one stated in
/// a unit that is not the model's metre, which the writer below will refuse
/// as well. Such a body is written out and shared with nothing: a body that
/// might be something else's is never mapped on a guess.
fn push_mapped_body(
    file: &mut StepFile,
    fingerprint: Option<u64>,
    representation_context: EntityRef,
    lengths: Lengths,
    writer: &mut BodyWriter<'_>,
    write: impl FnOnce(&mut StepFile) -> Option<EntityRef>,
) -> Option<EntityRef> {
    let Some(maps) = writer.maps.as_deref_mut() else {
        // Every element carries its own body: what this wrote before bodies
        // were shared, and what a reader that cannot follow a mapped item
        // needs.
        return write(file);
    };
    let known = fingerprint.and_then(|value| maps.get(&value).copied());
    let map = if let Some(map) = known {
        writer.report.count_mapped_body();
        map
    } else {
        {
            let body = write(file)?;
            // The map's own origin, and the operator's below it. The
            // element's placement already stands where the element does, and
            // the body under it is already in that placement's frame, so both
            // are the identity and neither moves anything.
            let origin = push_axis(file, lengths, 0.0);
            let map = file.push(
                "IFCREPRESENTATIONMAP",
                vec![reference(origin), reference(body)],
            );
            if let Some(value) = fingerprint {
                maps.insert(value, map);
            }
            map
        }
    };
    writer.used_map = Some(map);
    let local_origin = push_cartesian_point(file, lengths, [0.0, 0.0, 0.0]);
    let operator = file.push(
        "IFCCARTESIANTRANSFORMATIONOPERATOR3D",
        vec![
            omitted(),
            omitted(),
            reference(local_origin),
            omitted(),
            omitted(),
        ],
    );
    let item = file.push("IFCMAPPEDITEM", vec![reference(map), reference(operator)]);
    Some(file.push(
        "IFCSHAPEREPRESENTATION",
        vec![
            reference(representation_context),
            string("Body"),
            string("MappedRepresentation"),
            StepValue::List(vec![reference(item)]),
        ],
    ))
}

/// Count a curved solid whose every curve turns about one axis.
///
/// The upper bound on what a profile that could hold an arc would reach: a
/// sweep's curved walls are cylinders about its own direction and its curved
/// profile edges are arcs in a plane square to it, so every curve of a curved
/// prism turns about one axis. This says nothing about the rest of the tests -
/// see [`SolidReport::curves_about_one_axis`].
fn note_curves(brep: &BimBrep, report: &mut SolidReport) {
    let mut axis: Option<[f64; 3]> = None;
    let mut curved = false;
    let mut turns = |direction: [f64; 3]| {
        curved = true;
        match axis {
            None => {
                axis = Some(direction);
                true
            }
            Some(held) => {
                let cross = [
                    held[1].mul_add(direction[2], -(held[2] * direction[1])),
                    held[2].mul_add(direction[0], -(held[0] * direction[2])),
                    held[0].mul_add(direction[1], -(held[1] * direction[0])),
                ];
                dot(cross, cross).sqrt() <= extrusion::TOLERANCE
            }
        }
    };
    for face in &brep.faces {
        let aligned = match &face.surface {
            BimBrepSurface::Plane { .. } => true,
            BimBrepSurface::Cylinder { z_axis, .. } => turns(*z_axis),
            // A cone, a sphere, a torus or a ruled patch is not the wall of a
            // sweep whatever the profile can hold.
            BimBrepSurface::Revolution { .. } | BimBrepSurface::Ruled { .. } => false,
        };
        if !aligned {
            return;
        }
        for edges in &face.loops {
            for edge in edges {
                let aligned = match &edge.curve {
                    BimBrepCurve::Line => true,
                    BimBrepCurve::Arc(arc) => turns(arc.z_axis),
                    BimBrepCurve::Polyline(_) => false,
                };
                if !aligned {
                    return;
                }
            }
        }
    }
    if curved {
        report.curves_about_one_axis.saw(brep.faces.len());
    }
}

/// The prism this body is, where it is one.
///
/// Two things have to hold before the question is even asked. The shell must
/// be complete - a sweep is a closed solid, and half a boundary is not one -
/// and every face must be a polygon on a plane, because a cylindrical face or
/// an arc edge would put a curve in the profile and this writes a polyline.
/// The rounded solids therefore keep their boundary representation; what it
/// would take to read them is an arc in the profile curve, and the faces they
/// hold say how much that is worth.
fn prism_of(brep: &BimBrep, frame: GeometryFrame) -> Result<extrusion::Prism, NotAPrism> {
    if !brep.complete {
        return Err(NotAPrism::ShellIncomplete);
    }
    let mut shell = Vec::with_capacity(brep.faces.len());
    for face in &brep.faces {
        if !matches!(face.surface, BimBrepSurface::Plane { .. }) {
            return Err(NotAPrism::SurfaceIsCurved);
        }
        if face.loops.is_empty() {
            return Err(NotAPrism::NotASolid);
        }
        let mut boundaries = Vec::with_capacity(face.loops.len());
        for edges in &face.loops {
            boundaries.push(polygon_of(edges, frame)?);
        }
        shell.push(boundaries);
    }
    extrusion::recognise(&shell)
}

/// One boundary loop as the polygon it closes, in the element's own
/// coordinates.
///
/// Each edge contributes its start, the way the tessellator reads a loop: the
/// next edge's start is this edge's end, and the last closes on the first. A
/// point stated twice over carries no direction and would put a zero-length
/// side in the profile, so the repeats go.
fn polygon_of(
    edges: &[BimBrepEdge],
    frame: GeometryFrame,
) -> Result<extrusion::Polygon, NotAPrism> {
    let mut polygon = Vec::with_capacity(edges.len());
    for edge in edges {
        if !matches!(edge.curve, BimBrepCurve::Line) {
            return Err(NotAPrism::EdgeIsCurved);
        }
        polygon.push(local_coordinates(&edge.start, frame).ok_or(NotAPrism::PointNotReadable)?);
    }
    polygon.dedup_by(|left, right| coincident(*left, *right));
    if polygon.len() > 1 && coincident(polygon[0], polygon[polygon.len() - 1]) {
        polygon.pop();
    }
    if polygon.len() < 3 {
        return Err(NotAPrism::NotASolid);
    }
    Ok(polygon)
}

/// Two points of one loop that are the same point. A tenth of the recognition
/// tolerance, so that a side this drops is one no test downstream could have
/// told from nothing.
fn coincident(left: [f64; 3], right: [f64; 3]) -> bool {
    left.into_iter()
        .zip(right)
        .all(|(left, right)| (left - right).abs() <= extrusion::TOLERANCE / 10.0)
}

/// One recognised prism as `IfcExtrudedAreaSolid`: the profile in its own
/// plane, that plane's placement, and the depth swept along it.
///
/// The profile is stated in the placement's own coordinates and swept along
/// its Z, which is the form every reader expects and the one that keeps the
/// profile's two numbers two rather than three.
fn push_extruded_area_solid(
    file: &mut StepFile,
    prism: &extrusion::Prism,
    lengths: Lengths,
) -> EntityRef {
    let outer = push_profile_curve(file, lengths, &prism.outer);
    let profile = if prism.voids.is_empty() {
        file.push(
            "IFCARBITRARYCLOSEDPROFILEDEF",
            vec![enumeration("AREA"), omitted(), reference(outer)],
        )
    } else {
        let voids = prism
            .voids
            .iter()
            .map(|boundary| reference(push_profile_curve(file, lengths, boundary)))
            .collect();
        file.push(
            "IFCARBITRARYPROFILEDEFWITHVOIDS",
            vec![
                enumeration("AREA"),
                omitted(),
                reference(outer),
                StepValue::List(voids),
            ],
        )
    };
    let origin = push_cartesian_point(file, lengths, prism.origin);
    let axis = push_direction(file, prism.direction);
    let reference_direction = push_direction(file, prism.x_axis);
    let position = file.push(
        "IFCAXIS2PLACEMENT3D",
        vec![
            reference(origin),
            reference(axis),
            reference(reference_direction),
        ],
    );
    let along = push_direction(file, [0.0, 0.0, 1.0]);
    file.push(
        "IFCEXTRUDEDAREASOLID",
        vec![
            reference(profile),
            reference(position),
            reference(along),
            lengths.value(prism.depth),
        ],
    )
}

/// A profile's boundary as a closed `IfcPolyline`.
///
/// ISO 10303-42 makes a polyline closed by repeating its first point as its
/// last, which is what `IfcArbitraryClosedProfileDef` requires of the curve it
/// is given.
fn push_profile_curve(file: &mut StepFile, lengths: Lengths, boundary: &[[f64; 2]]) -> EntityRef {
    let mut points: Vec<StepValue> = boundary
        .iter()
        .map(|point| reference(push_cartesian_point_2d(file, lengths, *point)))
        .collect();
    if let Some(first) = points.first().cloned() {
        points.push(first);
    }
    file.push("IFCPOLYLINE", vec![StepValue::List(points)])
}

/// `IfcAdvancedBrep`/`IfcClosedShell` when [`BimBrep::complete`] holds, so a
/// closed-solid claim always corresponds to every source face resolving;
/// otherwise `IfcShellBasedSurfaceModel`/`IfcOpenShell` over whichever faces
/// did resolve, which is schema-valid for a shell known to be incomplete.
fn push_brep_item(
    file: &mut StepFile,
    brep: &BimBrep,
    frame: GeometryFrame,
) -> Option<(EntityRef, &'static str, Vec<Option<EntityRef>>)> {
    if brep.faces.is_empty() {
        return None;
    }
    // Kept in `brep.faces`' own order, `None` where a face did not write, so
    // a caller can style a written face by the same index `element_faces`
    // would read it back at - see `BodyWriter::written_faces`.
    let mut written = Vec::with_capacity(brep.faces.len());
    // A face this cannot write leaves the shell open instead of discarding
    // the whole body, which is what the incomplete-shell path is for. The
    // closed-solid claim then has to account for it: a shell missing a face
    // the source does declare is not closed, however the face was lost.
    let mut wrote_every_face = true;
    for face in &brep.faces {
        let face_entity = push_advanced_face(file, face, frame);
        if face_entity.is_none() {
            wrote_every_face = false;
        }
        written.push(face_entity);
    }
    let faces: Vec<EntityRef> = written.iter().copied().flatten().collect();
    if faces.is_empty() {
        return None;
    }
    let face_list = StepValue::List(faces.into_iter().map(reference).collect());
    let (item, representation_type) = if brep.complete && wrote_every_face {
        let shell = file.push("IFCCLOSEDSHELL", vec![face_list]);
        (
            file.push("IFCADVANCEDBREP", vec![reference(shell)]),
            "AdvancedBrep",
        )
    } else {
        let shell = file.push("IFCOPENSHELL", vec![face_list]);
        (
            file.push(
                "IFCSHELLBASEDSURFACEMODEL",
                vec![StepValue::List(vec![reference(shell)])],
            ),
            "SurfaceModel",
        )
    };
    Some((item, representation_type, written))
}

/// One body's shape representation: the sweep it is, where it is one, and
/// otherwise the item [`push_brep_item`] wrote.
fn push_brep(
    file: &mut StepFile,
    brep: &BimBrep,
    representation_context: EntityRef,
    frame: GeometryFrame,
    writer: &mut BodyWriter<'_>,
) -> Option<EntityRef> {
    let read = prism_of(brep, frame);
    // Counted for every element that carries this body, whether or not the
    // body itself is written again: the tally says what the model holds, not
    // how many times the file states it.
    writer
        .report
        .saw(read.as_ref().map_err(|refusal| *refusal), brep.faces.len());
    note_curves(brep, writer.report);
    let mut written_faces = Vec::new();
    let body = push_mapped_body(
        file,
        brep_fingerprint(brep, frame),
        representation_context,
        frame.lengths,
        writer,
        |file| {
            let (item, representation_type) = if let Ok(prism) = read {
                (
                    push_extruded_area_solid(file, &prism, frame.lengths),
                    "SweptSolid",
                )
            } else {
                let (item, representation_type, faces) = push_brep_item(file, brep, frame)?;
                written_faces = faces;
                (item, representation_type)
            };
            Some(push_body_representation(
                file,
                representation_context,
                representation_type,
                vec![item],
            ))
        },
    )?;
    writer.written_faces = written_faces;
    Some(file.push(
        "IFCPRODUCTDEFINITIONSHAPE",
        vec![omitted(), omitted(), StepValue::List(vec![reference(body)])],
    ))
}

/// The several closed solids of one element, as one `Body` representation.
///
/// A nested family is placed as several sub-instances and no single body
/// describes it; `IfcShapeRepresentation.Items` is a set, so the members go in
/// as several items of one representation rather than as several
/// representations, which is what keeps the `RepresentationType` a true
/// statement about all of them. Only members that write as `AdvancedBrep` are
/// taken: mixing a surface model in under that type would make the type say
/// something false about the item beside it.
fn push_assembly(
    file: &mut StepFile,
    parts: &[BimBrep],
    representation_context: EntityRef,
    frame: GeometryFrame,
    writer: &mut BodyWriter<'_>,
) -> Option<EntityRef> {
    if parts.is_empty() {
        return None;
    }
    // Every member is recognised before any of them is written, because the
    // `RepresentationType` has to be true of all the items under it: a set
    // where one member is a sweep and the next is not is written as boundary
    // representations throughout. Recognition reads and writes nothing, so
    // asking first costs the file no entity that then goes unreferenced.
    let read: Vec<Result<extrusion::Prism, NotAPrism>> =
        parts.iter().map(|part| prism_of(part, frame)).collect();
    for (part, outcome) in parts.iter().zip(&read) {
        writer.report.saw(
            outcome.as_ref().map_err(|refusal| *refusal),
            part.faces.len(),
        );
        note_curves(part, writer.report);
    }
    let mut written_faces = Vec::new();
    let body = push_mapped_body(
        file,
        assembly_fingerprint(parts, frame),
        representation_context,
        frame.lengths,
        writer,
        |file| {
            if let Ok(prisms) = read.into_iter().collect::<Result<Vec<_>, _>>() {
                let items = prisms
                    .iter()
                    .map(|prism| push_extruded_area_solid(file, prism, frame.lengths))
                    .collect();
                return Some(push_body_representation(
                    file,
                    representation_context,
                    "SweptSolid",
                    items,
                ));
            }
            let mut items = Vec::with_capacity(parts.len());
            for part in parts {
                let Some((item, "AdvancedBrep", faces)) = push_brep_item(file, part, frame) else {
                    return None;
                };
                written_faces.extend(faces);
                items.push(item);
            }
            if items.is_empty() {
                return None;
            }
            Some(push_body_representation(
                file,
                representation_context,
                "AdvancedBrep",
                items,
            ))
        },
    )?;
    writer.written_faces = written_faces;
    Some(file.push(
        "IFCPRODUCTDEFINITIONSHAPE",
        vec![omitted(), omitted(), StepValue::List(vec![reference(body)])],
    ))
}

fn push_body_representation(
    file: &mut StepFile,
    representation_context: EntityRef,
    representation_type: &str,
    items: Vec<EntityRef>,
) -> EntityRef {
    file.push(
        "IFCSHAPEREPRESENTATION",
        vec![
            reference(representation_context),
            string("Body"),
            string(representation_type),
            StepValue::List(items.into_iter().map(reference).collect()),
        ],
    )
}

/// # Note on `SameSense`
///
/// Every `IfcAdvancedFace` here is written with `SameSense = TRUE`. Whether
/// the source loop's winding actually agrees with the surface's own normal
/// direction (which is what `SameSense` is meant to record) has not been
/// checked against a real solid's outward orientation - it is a placeholder
/// pending validation against `ifcopenshell`'s geometrization of an actual
/// exported symbol, not a measured constant.
fn push_advanced_face(
    file: &mut StepFile,
    face: &BimBrepFace,
    frame: GeometryFrame,
) -> Option<EntityRef> {
    let surface = push_brep_surface(file, face, frame)?;
    if face.loops.is_empty() {
        return None;
    }
    let mut bounds = Vec::with_capacity(face.loops.len());
    for (index, loop_edges) in face.loops.iter().enumerate() {
        let edge_loop = push_edge_loop(file, loop_edges, frame)?;
        let entity = if index == 0 {
            "IFCFACEOUTERBOUND"
        } else {
            "IFCFACEBOUND"
        };
        bounds.push(file.push(entity, vec![reference(edge_loop), StepValue::Boolean(true)]));
    }
    Some(file.push(
        "IFCADVANCEDFACE",
        vec![
            StepValue::List(bounds.into_iter().map(reference).collect()),
            reference(surface),
            StepValue::Boolean(true),
        ],
    ))
}

/// One face's surface. The face rather than the surface alone, because a cone
/// is written as a *bounded* curve revolved about an axis and the face's own
/// boundary is what bounds it.
fn push_brep_surface(
    file: &mut StepFile,
    face: &BimBrepFace,
    frame: GeometryFrame,
) -> Option<EntityRef> {
    match &face.surface {
        BimBrepSurface::Plane {
            origin,
            x_axis,
            y_axis,
        } => {
            let normal = cross(*x_axis, *y_axis);
            let axis = push_local_axis(file, origin, normal, *x_axis, frame)?;
            Some(file.push("IFCPLANE", vec![reference(axis)]))
        }
        BimBrepSurface::Cylinder {
            center,
            x_axis,
            y_axis: _,
            z_axis,
            radius,
        } => {
            if radius.unit.as_ref()?.id != "autodesk.unit.unit:meters-1.0.0"
                || !radius.value.is_finite()
                || radius.value <= 0.0
            {
                return None;
            }
            let axis = push_local_axis(file, center, *z_axis, *x_axis, frame)?;
            Some(file.push(
                "IFCCYLINDRICALSURFACE",
                vec![reference(axis), frame.lengths.value(radius.value)],
            ))
        }
        surface @ BimBrepSurface::Revolution { .. } => {
            push_revolved_surface(file, surface, &face.loops, frame)
        }
        // IFC4 has no ruled-surface entity. Some ruled surfaces coincide with
        // one it does have - a profile translated along a direction is an
        // `IfcSurfaceOfLinearExtrusion`, and a circle ruled to a point is a
        // cone - but each of those is a condition to be tested on the numbers,
        // not assumed, so the surface is refused until it is. Refusing leaves
        // the face out of the shell rather than approximating it.
        BimBrepSurface::Ruled { .. } => None,
    }
}

/// The frame a `SurfRev` turns its profile in: an origin and three axes, in
/// world coordinates, and the profile's own numbers read against them.
struct RevolvedFrame<'a> {
    center: &'a BimPoint3,
    x_axis: [f64; 3],
    y_axis: [f64; 3],
    z_axis: [f64; 3],
}

impl RevolvedFrame<'_> {
    fn point(&self, local: [f64; 3]) -> BimPoint3 {
        BimPoint3 {
            coordinates: [0, 1, 2].map(|axis| {
                self.center.coordinates[axis]
                    + local[0] * self.x_axis[axis]
                    + local[1] * self.y_axis[axis]
                    + local[2] * self.z_axis[axis]
            }),
            unit: self.center.unit.clone(),
        }
    }

    fn direction(&self, local: [f64; 3]) -> [f64; 3] {
        [0, 1, 2].map(|axis| {
            local[0] * self.x_axis[axis]
                + local[1] * self.y_axis[axis]
                + local[2] * self.z_axis[axis]
        })
    }
}

/// A revolved profile is named by what it sweeps out rather than written as an
/// `IfcSurfaceOfRevolution`: the profile is a line coplanar with the axis or an
/// arc in a plane holding it, so the surface is a cone, a torus or a sphere,
/// and IFC has all three as elementary surfaces the kernel knows how to build.
fn push_revolved_surface(
    file: &mut StepFile,
    surface: &BimBrepSurface,
    loops: &[Vec<BimBrepEdge>],
    frame: GeometryFrame,
) -> Option<EntityRef> {
    let BimBrepSurface::Revolution {
        center,
        x_axis,
        y_axis,
        z_axis,
        profile,
    } = surface
    else {
        return None;
    };
    let revolution = RevolvedFrame {
        center,
        x_axis: *x_axis,
        y_axis: *y_axis,
        z_axis: *z_axis,
    };
    match &**profile {
        BimBrepProfile::Line { origin, direction } => push_revolved_line_surface(
            file,
            &revolution,
            (origin.coordinates, *direction),
            loops,
            frame,
        ),
        BimBrepProfile::Arc { center, radius, .. } => {
            push_revolved_arc_surface(file, &revolution, (center.coordinates, radius), frame)
        }
    }
}

/// A line turned about the frame's axis: a cone, or - where it does not slant -
/// the cylinder or the flat annulus that slant would degenerate into.
fn push_revolved_line_surface(
    file: &mut StepFile,
    revolution: &RevolvedFrame,
    (point, direction): ([f64; 3], [f64; 3]),
    loops: &[Vec<BimBrepEdge>],
    frame: GeometryFrame,
) -> Option<EntityRef> {
    // Which way out of the axis the profile's own plane lies. The point names
    // it, unless the point is *on* the axis - a cone declared from its own
    // apex - and then the direction does. A line with neither is the axis
    // itself, which sweeps no surface.
    let (radial_local, radius) = {
        let from_point = point[0].hypot(point[1]);
        let from_direction = direction[0].hypot(direction[1]);
        if from_point > REVOLVED_AXIS_TOLERANCE_METRES {
            (
                [point[0] / from_point, point[1] / from_point, 0.0],
                from_point,
            )
        } else if from_direction > REVOLVED_AXIS_TOLERANCE_METRES {
            (
                [
                    direction[0] / from_direction,
                    direction[1] / from_direction,
                    0.0,
                ],
                0.0,
            )
        } else {
            return None;
        }
    };
    if !radius.is_finite() {
        return None;
    }
    let radial = revolution.direction(radial_local);
    // How fast the radius grows as the profile climbs.
    let outward = direction[0] * radial_local[0] + direction[1] * radial_local[1];
    let rise = direction[2];
    let position = revolution.point([0.0, 0.0, point[2]]);
    if rise.abs() <= REVOLVED_AXIS_TOLERANCE_METRES {
        // The line is perpendicular to the axis: an annulus, which is flat.
        let axis = push_local_axis(file, &position, revolution.z_axis, radial, frame)?;
        return Some(file.push("IFCPLANE", vec![reference(axis)]));
    }
    if outward.abs() <= REVOLVED_AXIS_TOLERANCE_METRES {
        // Parallel to the axis: a cylinder of that radius.
        let axis = push_local_axis(file, &position, revolution.z_axis, radial, frame)?;
        return Some(file.push(
            "IFCCYLINDRICALSURFACE",
            vec![reference(axis), frame.lengths.value(radius)],
        ));
    }
    // IFC4 has no conical surface - `IfcConicalSurface` is ISO 10303-42's, and
    // `ifcopenshell.validate` refuses it - so the cone is written as what the
    // record already says it is: the profile line, revolved about the frame's
    // axis. `IfcSurfaceOfRevolution` sweeps a *bounded* curve, and the bound
    // is the face's own boundary measured along that axis: every point of it
    // lies on the surface, so the span they cover is the span the face needs.
    let slope = outward / rise;
    let (low, high) = axial_span(revolution, loops)?;
    // Off the ends, so the boundary is trimmed out of the surface's interior
    // rather than off its edge.
    let margin = ((high - low) * CONE_MARGIN_FRACTION).max(REVOLVED_AXIS_TOLERANCE_METRES);
    let mut ends = [low - margin, high + margin];
    // The margin must not carry the profile past the axis, where the sweep
    // would double back into a second cone. The apex itself is allowed - a
    // face that runs to its own point needs the surface to reach it, and
    // `ifcopenshell` builds that.
    let apex = point[2] - radius / slope;
    if slope > 0.0 {
        ends[0] = ends[0].max(apex);
    } else {
        ends[1] = ends[1].min(apex);
    }
    if ends[0] >= ends[1] || !ends[0].is_finite() || !ends[1].is_finite() {
        return None;
    }
    let profile_radius = |height: f64| radius + (height - point[2]) * slope;
    // Three points on the one line, not two. A profile of one edge sends
    // `ifcopenshell` down a path that revolves it without applying
    // `Position` to the axis, so a schema-correct axis comes out wrong there;
    // two edges take the path that sweeps first and places after, which is
    // the schema's reading. The middle point changes no point of the surface.
    let curve = {
        let points = [ends[0], f64::midpoint(ends[0], ends[1]), ends[1]].map(|height| {
            push_cartesian_point_2d(file, frame.lengths, [profile_radius(height), height])
        });
        file.push(
            "IFCPOLYLINE",
            vec![StepValue::List(points.into_iter().map(reference).collect())],
        )
    };
    let profile = file.push(
        "IFCARBITRARYOPENPROFILEDEF",
        vec![enumeration("CURVE"), omitted(), reference(curve)],
    );
    // The profile's own plane: `x` along the radius, and therefore `y` along
    // the axis, which is where a surface of revolution requires its axis to
    // lie. The placement is in the element's coordinates and the axis in the
    // placement's: see `push_revolution_axis`.
    let position = push_local_axis(
        file,
        revolution.center,
        cross(radial, revolution.z_axis),
        radial,
        frame,
    )?;
    let axis = push_revolution_axis(file, frame.lengths);
    Some(file.push(
        "IFCSURFACEOFREVOLUTION",
        vec![reference(profile), reference(position), reference(axis)],
    ))
}

/// How far a revolved face's boundary reaches along the frame's axis, measured
/// from the frame's own origin. `None` when the face has no boundary to
/// measure or a point of it is not in metres.
fn axial_span(revolution: &RevolvedFrame, loops: &[Vec<BimBrepEdge>]) -> Option<(f64, f64)> {
    let center = metric_coordinates(revolution.center)?;
    let mut span: Option<(f64, f64)> = None;
    for edge in loops.iter().flatten() {
        for point in [&edge.start, &edge.end] {
            let height = dot(
                subtract(metric_coordinates(point)?, center),
                revolution.z_axis,
            );
            if !height.is_finite() {
                return None;
            }
            span = Some(match span {
                Some((low, high)) => (low.min(height), high.max(height)),
                None => (height, height),
            });
        }
    }
    span
}

/// An arc turned about the frame's axis: a torus, or a sphere where the arc is
/// centred on the axis itself.
fn push_revolved_arc_surface(
    file: &mut StepFile,
    revolution: &RevolvedFrame,
    (point, radius): ([f64; 3], &BimNumber),
    frame: GeometryFrame,
) -> Option<EntityRef> {
    if radius.unit.as_ref()?.id != "autodesk.unit.unit:meters-1.0.0"
        || !radius.value.is_finite()
        || radius.value <= 0.0
    {
        return None;
    }
    let major = point[0].hypot(point[1]);
    if !major.is_finite() {
        return None;
    }
    let position = revolution.point([0.0, 0.0, point[2]]);
    if major <= REVOLVED_AXIS_TOLERANCE_METRES {
        // The arc is centred on the axis: a sphere.
        let axis = push_local_axis(file, &position, revolution.z_axis, revolution.x_axis, frame)?;
        return Some(file.push(
            "IFCSPHERICALSURFACE",
            vec![reference(axis), frame.lengths.value(radius.value)],
        ));
    }
    let radial = revolution.direction([point[0] / major, point[1] / major, 0.0]);
    if radius.value >= major * (1.0 - TORUS_RADIUS_MARGIN) {
        // The profile circle reaches the axis or crosses it, and
        // `IfcToroidalSurface` requires a minor radius strictly under the
        // major one - so this torus, which is a real shape with no hole left
        // in it, is written the way the cone is: the profile revolved.
        return push_revolved_arc_as_revolution(
            file,
            revolution,
            (radial, [major, point[2]], radius.value),
            frame,
        );
    }
    let axis = push_local_axis(file, &position, revolution.z_axis, radial, frame)?;
    Some(file.push(
        "IFCTOROIDALSURFACE",
        vec![
            reference(axis),
            frame.lengths.value(major),
            frame.lengths.value(radius.value),
        ],
    ))
}

/// The profile circle itself, revolved: for a torus whose hole has closed, the
/// only form IFC4 has. The circle is written whole - a full turn either side
/// of the axis - because the face's own boundary is what trims it, and where
/// its two-dimensional reference direction points does not change the set of
/// points it sweeps.
fn push_revolved_arc_as_revolution(
    file: &mut StepFile,
    revolution: &RevolvedFrame,
    (radial, center, radius): ([f64; 3], [f64; 2], f64),
    frame: GeometryFrame,
) -> Option<EntityRef> {
    let profile_center = push_cartesian_point_2d(file, frame.lengths, center);
    let profile_direction = push_direction_2d(file, [1.0, 0.0]);
    let profile_position = file.push(
        "IFCAXIS2PLACEMENT2D",
        vec![reference(profile_center), reference(profile_direction)],
    );
    let circle = file.push(
        "IFCCIRCLE",
        vec![reference(profile_position), frame.lengths.value(radius)],
    );
    // A full turn, in the radians this file declares its angles in, as two
    // half turns: one edge would take the path in `ifcopenshell` that leaves
    // `Position` off the axis - see `push_revolved_line_surface`.
    let halves = [
        (0.0, std::f64::consts::PI, "CONTSAMEGRADIENT"),
        (std::f64::consts::PI, std::f64::consts::TAU, "DISCONTINUOUS"),
    ]
    .map(|(start, end, transition)| {
        let trimmed = file.push(
            "IFCTRIMMEDCURVE",
            vec![
                reference(circle),
                StepValue::List(vec![parameter_value(start)]),
                StepValue::List(vec![parameter_value(end)]),
                StepValue::Boolean(true),
                enumeration("PARAMETER"),
            ],
        );
        file.push(
            "IFCCOMPOSITECURVESEGMENT",
            vec![
                enumeration(transition),
                StepValue::Boolean(true),
                reference(trimmed),
            ],
        )
    });
    let composite = file.push(
        "IFCCOMPOSITECURVE",
        vec![
            StepValue::List(halves.into_iter().map(reference).collect()),
            StepValue::Boolean(false),
        ],
    );
    let profile = file.push(
        "IFCARBITRARYOPENPROFILEDEF",
        vec![enumeration("CURVE"), omitted(), reference(composite)],
    );
    let position = push_local_axis(
        file,
        revolution.center,
        cross(radial, revolution.z_axis),
        radial,
        frame,
    )?;
    let axis = push_revolution_axis(file, frame.lengths);
    Some(file.push(
        "IFCSURFACEOFREVOLUTION",
        vec![reference(profile), reference(position), reference(axis)],
    ))
}

/// `IfcEdgeLoop.IsContinuous` (`IfcLoopHeadToTail`) requires edge `i`'s
/// `EdgeEnd` and edge `i+1`'s `EdgeStart` to be the *same* `IfcVertex`
/// instance, not merely a numerically-equal one - confirmed the hard way:
/// giving every edge its own fresh vertex, even at coordinates that agreed
/// to full float precision, failed the rule on all 608 loops of a real
/// export. So a loop's `N` distinct corners (edge `i`'s start, for `i` in
/// `0..N`; `edges[i].end` is trusted to already equal `edges[i+1].start`,
/// which is what closes a [`crate::brep`]-produced loop in the first place)
/// are pushed once each and then shared by both edges that meet there.
fn push_edge_loop(
    file: &mut StepFile,
    edges: &[BimBrepEdge],
    frame: GeometryFrame,
) -> Option<EntityRef> {
    if edges.is_empty() {
        return None;
    }
    let mut vertices = Vec::with_capacity(edges.len());
    for edge in edges {
        let corner = local_coordinates(&edge.start, frame)?;
        if corner.into_iter().any(|value| !value.is_finite()) {
            return None;
        }
        let point = push_cartesian_point(file, frame.lengths, corner);
        vertices.push(file.push("IFCVERTEXPOINT", vec![reference(point)]));
    }
    let mut oriented = Vec::with_capacity(edges.len());
    for (index, edge) in edges.iter().enumerate() {
        let start_vertex = vertices[index];
        let end_vertex = vertices[(index + 1) % vertices.len()];
        oriented.push(push_oriented_edge(
            file,
            edge,
            start_vertex,
            end_vertex,
            frame,
        )?);
    }
    Some(file.push(
        "IFCEDGELOOP",
        vec![StepValue::List(
            oriented.into_iter().map(reference).collect(),
        )],
    ))
}

fn push_oriented_edge(
    file: &mut StepFile,
    edge: &BimBrepEdge,
    start_vertex: EntityRef,
    end_vertex: EntityRef,
    frame: GeometryFrame,
) -> Option<EntityRef> {
    let start = local_coordinates(&edge.start, frame)?;
    let end = local_coordinates(&edge.end, frame)?;
    if start.into_iter().chain(end).any(|value| !value.is_finite()) {
        return None;
    }
    let curve = match &edge.curve {
        BimBrepCurve::Line => {
            let delta = subtract(end, start);
            let length = dot(delta, delta).sqrt();
            if length <= f64::EPSILON {
                return None;
            }
            let direction = delta.map(|value| value / length);
            let direction_ref = push_direction(file, direction);
            let vector = file.push(
                "IFCVECTOR",
                // `IfcVector.Magnitude` is a length, so one unit of the file's
                // own length unit - not one metre in a millimetre file.
                vec![reference(direction_ref), frame.lengths.value(1.0)],
            );
            // `IfcLine.Pnt` describes the underlying infinite line, not a
            // topological vertex, so it needs its own value, not the shared
            // `IfcVertexPoint`'s.
            let line_point = push_cartesian_point(file, frame.lengths, start);
            file.push("IFCLINE", vec![reference(line_point), reference(vector)])
        }
        BimBrepCurve::Arc(arc) => {
            if arc.radius.unit.as_ref()?.id != "autodesk.unit.unit:meters-1.0.0"
                || !arc.radius.value.is_finite()
                || arc.radius.value <= 0.0
            {
                return None;
            }
            let axis = push_local_axis(file, &arc.center, arc.z_axis, arc.x_axis, frame)?;
            file.push(
                "IFCCIRCLE",
                vec![reference(axis), frame.lengths.value(arc.radius.value)],
            )
        }
        BimBrepCurve::Polyline(points) => {
            if points.len() < 3 {
                return None;
            }
            let mut point_refs = Vec::with_capacity(points.len());
            for point in points {
                let coordinates = local_coordinates(point, frame)?;
                if coordinates.into_iter().any(|value| !value.is_finite()) {
                    return None;
                }
                point_refs.push(reference(push_cartesian_point(
                    file,
                    frame.lengths,
                    coordinates,
                )));
            }
            file.push("IFCPOLYLINE", vec![StepValue::List(point_refs)])
        }
    };
    let edge_curve = file.push(
        "IFCEDGECURVE",
        vec![
            reference(start_vertex),
            reference(end_vertex),
            reference(curve),
            StepValue::Boolean(true),
        ],
    );
    Some(file.push(
        "IFCORIENTEDEDGE",
        vec![
            StepValue::Derived,
            StepValue::Derived,
            reference(edge_curve),
            StepValue::Boolean(true),
        ],
    ))
}

/// Build an `IfcAxis2Placement3D` from a world point and two world
/// direction vectors, transformed into the same local frame
/// [`local_coordinates`] and [`local_direction`] apply elsewhere.
fn push_local_axis(
    file: &mut StepFile,
    origin: &BimPoint3,
    axis_world: [f64; 3],
    ref_direction_world: [f64; 3],
    frame: GeometryFrame,
) -> Option<EntityRef> {
    let point = local_coordinates(origin, frame)?;
    let axis = local_direction(axis_world, frame.placement);
    let ref_direction = local_direction(ref_direction_world, frame.placement);
    if point
        .into_iter()
        .chain(axis)
        .chain(ref_direction)
        .any(|value| !value.is_finite())
    {
        return None;
    }
    let point_ref = push_cartesian_point(file, frame.lengths, point);
    let axis_ref = push_direction(file, axis);
    let ref_direction_ref = push_direction(file, ref_direction);
    Some(file.push(
        "IFCAXIS2PLACEMENT3D",
        vec![
            reference(point_ref),
            reference(axis_ref),
            reference(ref_direction_ref),
        ],
    ))
}

/// Rotate a world direction vector into the element's local placement basis,
/// the same rotation [`local_coordinates`] applies to points - without the
/// translation, since a direction has no position.
fn local_direction(direction: [f64; 3], placement: Option<MetricPlacement>) -> [f64; 3] {
    let Some(placement) = placement else {
        return direction;
    };
    let local_y = cross(placement.axis, placement.reference_direction);
    [
        dot(direction, placement.reference_direction),
        dot(direction, local_y),
        dot(direction, placement.axis),
    ]
}

fn push_bounding_box(
    file: &mut StepFile,
    bounds: &BimBoundingBox,
    representation_context: EntityRef,
    frame: GeometryFrame,
) -> Option<EntityRef> {
    // The box arrives in the model's project coordinates, which is the frame
    // every `BimGeometry` is carried in, and `IfcBoundingBox` states its
    // corner in the product's own. Reading it as if it were already local is
    // what put 91 of AR S1's slabs, its roof and its curtain walls at twice
    // their level elevation: the storey placement added the elevation a
    // second time under a corner that already carried it.
    //
    // An `IfcBoundingBox` has a corner and three lengths along the frame's
    // axes and cannot be turned, so a frame the box is not square to admits
    // only the hull of the eight carried corners. That is exact wherever the
    // frame's axes are the project's, which is every one of the 827 elements
    // carrying a box in AR S1 - 723 axis-aligned placements and 104 with no
    // placement at all, and not one turned.
    let world_min = metric_coordinates(&bounds.min)?;
    let world_max = metric_coordinates(&bounds.max)?;
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for corner in 0_u8..8 {
        let world = [
            if corner & 1 == 0 {
                world_min[0]
            } else {
                world_max[0]
            },
            if corner & 2 == 0 {
                world_min[1]
            } else {
                world_max[1]
            },
            if corner & 4 == 0 {
                world_min[2]
            } else {
                world_max[2]
            },
        ];
        let local = frame_coordinates(world, frame)?;
        for (axis, value) in local.into_iter().enumerate() {
            min[axis] = min[axis].min(value);
            max[axis] = max[axis].max(value);
        }
    }
    let dimensions = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
    if dimensions
        .into_iter()
        .any(|dimension| !dimension.is_finite() || dimension <= 0.0)
    {
        return None;
    }
    let corner = push_cartesian_point(file, frame.lengths, min);
    let item = file.push(
        "IFCBOUNDINGBOX",
        vec![
            reference(corner),
            frame.lengths.value(dimensions[0]),
            frame.lengths.value(dimensions[1]),
            frame.lengths.value(dimensions[2]),
        ],
    );
    let representation = file.push(
        "IFCSHAPEREPRESENTATION",
        vec![
            reference(representation_context),
            string("Box"),
            string("BoundingBox"),
            StepValue::List(vec![reference(item)]),
        ],
    );
    Some(file.push(
        "IFCPRODUCTDEFINITIONSHAPE",
        vec![
            omitted(),
            omitted(),
            StepValue::List(vec![reference(representation)]),
        ],
    ))
}

fn push_axis_line(
    file: &mut StepFile,
    line: &BimLineSegment,
    representation_context: EntityRef,
    frame: GeometryFrame,
) -> Option<(EntityRef, EntityRef)> {
    let start = local_coordinates(&line.start, frame)?;
    let end = local_coordinates(&line.end, frame)?;
    let length_squared = start
        .into_iter()
        .zip(end)
        .map(|(start, end)| (end - start).powi(2))
        .sum::<f64>();
    if length_squared <= f64::EPSILON
        || start.into_iter().chain(end).any(|value| !value.is_finite())
    {
        return None;
    }
    let start = push_cartesian_point(file, frame.lengths, start);
    let end = push_cartesian_point(file, frame.lengths, end);
    // The item of this representation, not a curve some edge also uses: an
    // `IfcRepresentationItem` belongs to the representation that holds it, so
    // two elements with the same local centreline keep one each.
    let line = file.push_once(
        "IFCPOLYLINE",
        vec![StepValue::List(vec![reference(start), reference(end)])],
    );
    let axis = file.push(
        "IFCSHAPEREPRESENTATION",
        vec![
            reference(representation_context),
            string("Axis"),
            string("Curve3D"),
            StepValue::List(vec![reference(line)]),
        ],
    );
    Some((line, axis))
}

fn metric_coordinates(point: &BimPoint3) -> Option<[f64; 3]> {
    (point.unit.id == "autodesk.unit.unit:meters-1.0.0"
        && point.coordinates.into_iter().all(f64::is_finite))
    .then_some(point.coordinates)
}

fn local_coordinates(point: &BimPoint3, frame: GeometryFrame) -> Option<[f64; 3]> {
    frame_coordinates(metric_coordinates(point)?, frame)
}

/// The same conversion for a point already read out of its `BimPoint3`: what
/// a box has, whose corners are eight combinations of two carried points
/// rather than eight carried points.
fn frame_coordinates(mut world: [f64; 3], frame: GeometryFrame) -> Option<[f64; 3]> {
    if let Some(placement) = frame.placement {
        let delta = subtract(world, placement.origin);
        let local_y = cross(placement.axis, placement.reference_direction);
        return Some([
            dot(delta, placement.reference_direction),
            dot(delta, local_y),
            dot(delta, placement.axis),
        ]);
    }
    if !frame.storey_elevation.is_finite() {
        return None;
    }
    world[2] -= frame.storey_elevation;
    Some(world)
}

fn subtract(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter().zip(right).map(|(a, b)| a * b).sum()
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

/// A point of a profile's own two-dimensional plane, which is not the
/// element's coordinate system and takes none of its placement.
fn push_cartesian_point_2d(
    file: &mut StepFile,
    lengths: Lengths,
    coordinates: [f64; 2],
) -> EntityRef {
    file.push("IFCCARTESIANPOINT", vec![lengths.coordinates(coordinates)])
}

/// The axis a surface of revolution turns about, stated where the schema
/// states it: in the surface's own `Position`, which every writer here builds
/// with `x` along the radius and `y` along the axis. The profile is swept in
/// that frame and `Position` then carries the swept surface into the
/// element's, so the axis is the frame's `y` through its origin whatever the
/// element's coordinates are.
///
/// Written in the element's coordinates instead - which is what this once
/// did - the two readings agree only while the surface's centre is at the
/// element's origin and its axis happens to be the element's `y`. Elsewhere
/// the reference kernel turns the profile about a line displaced by the
/// centre's own coordinates: on AR S1 an 8 cm anchor became a torus 28 to
/// 45 m across, and on SMALL the pipe fittings' cones stood 25 to 33 mm off
/// the faces they bound.
fn push_revolution_axis(file: &mut StepFile, lengths: Lengths) -> EntityRef {
    let point = push_cartesian_point(file, lengths, [0.0, 0.0, 0.0]);
    let axis = push_direction(file, [0.0, 1.0, 0.0]);
    file.push("IFCAXIS1PLACEMENT", vec![reference(point), reference(axis)])
}

fn push_cartesian_point(file: &mut StepFile, lengths: Lengths, coordinates: [f64; 3]) -> EntityRef {
    file.push("IFCCARTESIANPOINT", vec![lengths.coordinates(coordinates)])
}

/// A direction of a profile's own two-dimensional plane.
fn push_direction_2d(file: &mut StepFile, direction: [f64; 2]) -> EntityRef {
    file.push(
        "IFCDIRECTION",
        vec![StepValue::List(
            direction.into_iter().map(StepValue::Real).collect(),
        )],
    )
}

fn push_direction(file: &mut StepFile, direction: [f64; 3]) -> EntityRef {
    file.push(
        "IFCDIRECTION",
        vec![StepValue::List(
            direction.into_iter().map(StepValue::Real).collect(),
        )],
    )
}

/// The total thickness of the layered build-up the element's type declares,
/// in metres, where every layer states one in the same unit.
///
/// This is the one base quantity that is not measured from the body, because
/// the source states it: `Width` is what the compound structure's layers add
/// up to. Joining our export of AR S1 to Revit's own on the Revit element id,
/// the layer totals this decode recovers reproduce Revit's own
/// `Qto_WallBaseQuantities.Width` on **7 615 of 7 615 walls** to within a
/// thousandth, and `Qto_SlabBaseQuantities.Width` on 450 of 513 slabs. The 63
/// slabs that differ are one class - a 50 mm single-layer floor insulation
/// whose type is named `_t=50`, for which Revit's `Width` is a plan dimension
/// of several metres rather than the thickness - so the disagreement is in
/// what Revit put in that field there, not in the layer table.
///
/// Guarded exactly as [`push_material_layer`] guards a layer it writes: a unit
/// that is not Revit's metre, or a total that is not finite and positive, is
/// left unwritten rather than written as something else. Zero is refused here
/// where a single layer may legitimately be zero, because a build-up with no
/// thickness at all states nothing about the element.
fn layer_set_thickness(element: &BimElement) -> Option<f64> {
    let total = element.material_layers.as_ref()?.total_thickness()?;
    (total.unit.as_ref()?.id == "autodesk.unit.unit:meters-1.0.0"
        && total.value.is_finite()
        && total.value > 0.0)
        .then_some(total.value)
}

/// The base quantity set for an entity, and the quantities it holds - the
/// buildingSMART templates' own answer, because the names differ from entity
/// to entity: a wall's volume is in `Qto_WallBaseQuantities` and a proxy's in
/// `Qto_BuildingElementProxyQuantities`, which is not even called *Base*.
fn base_quantity_set(entity: &str) -> Option<(&'static str, &'static [&'static str])> {
    IFC4_BASE_QUANTITY_SETS
        .iter()
        .find(|(name, _, _)| *name == entity)
        .map(|(_, set, quantities)| (*set, *quantities))
}

/// What one measured quantity is: the entity it is written as, and the value
/// in the unit that entity's measure is written in.
enum Quantity {
    /// A length, written in the file's own length unit.
    Length(f64),
    /// An area, in square metres, which is what the file declares whatever its
    /// lengths are in.
    Area(f64),
    /// A volume, in cubic metres, for the same reason.
    Volume(f64),
}

/// Every quantity this can measure for an element written as `entity`, named
/// as that entity's own template names it.
///
/// The names are not interchangeable between entities: a wall's side is
/// `NetSideArea` and a slab's face is `NetArea`, a column's `Length` runs
/// upright where a beam's runs along it. What is written is the intersection
/// of this list with the template's, so a name the template does not define
/// is never invented for an entity that has no room for it.
fn measured_quantities(entity: &str, measured: &Measured) -> Vec<(&'static str, Quantity)> {
    let Measured {
        net_volume,
        net_surface_area,
        length,
        width,
        height,
        side_area,
        plan_area,
        upright_area,
        plan_perimeter,
        pierced,
    } = *measured;
    // A gross quantity, which this can state only for a body nothing was cut
    // out of - there it is the same number as the net one. See [`Measured`].
    let gross = |value: f64| (!pierced).then_some(value);
    match entity {
        // A wall is measured across its own sides: the face pair carrying the
        // most area is the wall, and everything else follows from it.
        //
        // Against Revit's own export of AR S1, joined on the Revit element
        // id, of the 7 517 walls both files hold and this measures:
        // `Length` 7 272, `Height` 7 332, `GrossFootprintArea` 5 938 of 6 084
        // and `NetSideArea` 6 937 reproduce Revit's number to within a
        // thousandth. Where they differ it is the body that differs - the
        // 558 walls this export writes larger than Revit writes its own - not
        // the measurement.
        "IFCWALL" | "IFCWALLSTANDARDCASE" | "IFCCURTAINWALL" => vec![
            ("Length", Quantity::Length(length)),
            ("Width", Quantity::Length(width)),
            ("Height", Quantity::Length(height)),
            ("GrossFootprintArea", Quantity::Area(length * width)),
            ("NetFootprintArea", Quantity::Area(plan_area)),
            ("NetSideArea", Quantity::Area(side_area)),
            ("NetVolume", Quantity::Volume(net_volume)),
        ]
        .into_iter()
        .chain(gross(side_area).map(|area| ("GrossSideArea", Quantity::Area(area))))
        .chain(gross(net_volume).map(|volume| ("GrossVolume", Quantity::Volume(volume))))
        .collect(),
        // A slab lies down: its thickness is what it stands in, and its face
        // is what it covers. `Perimeter` is the boundary of that face, which
        // reproduces Revit's on 437 of the 440 slabs both files measure.
        //
        // No `GrossVolume`: Revit's is not this body's volume even where
        // nothing was cut out of the slab, agreeing on 60 of 427, so whatever
        // it means it is not what is measured here.
        "IFCSLAB" | "IFCROOF" | "IFCCOVERING" => vec![
            ("Width", Quantity::Length(height)),
            ("Perimeter", Quantity::Length(plan_perimeter)),
            ("NetArea", Quantity::Area(plan_area)),
            ("NetVolume", Quantity::Volume(net_volume)),
        ]
        .into_iter()
        .chain(gross(plan_area).map(|area| ("GrossArea", Quantity::Area(area))))
        .collect(),
        // A column stands: its length is its height, and the section it cuts
        // is what it shows in plan. Every one of the 56 columns both files
        // measure agrees with Revit on all five.
        "IFCCOLUMN" | "IFCPILE" => vec![
            ("Length", Quantity::Length(height)),
            ("CrossSectionArea", Quantity::Area(plan_area)),
            ("OuterSurfaceArea", Quantity::Area(upright_area)),
            ("NetVolume", Quantity::Volume(net_volume)),
        ]
        .into_iter()
        .chain(gross(net_volume).map(|volume| ("GrossVolume", Quantity::Volume(volume))))
        .collect(),
        // A member or a beam is measured as a body and nothing more: its
        // length runs whichever way its profile was swept, which this does
        // not read, and Revit's `CrossSectionArea` is that profile's area
        // rather than anything a bounding extent recovers.
        "IFCBEAM" | "IFCMEMBER" => vec![("NetVolume", Quantity::Volume(net_volume))]
            .into_iter()
            .chain(gross(net_volume).map(|volume| ("GrossVolume", Quantity::Volume(volume))))
            .collect(),
        // A door, a window and the opening they fill are measured by Revit
        // from the family's own width and height, which are not the extents
        // of the body this file carries - a window panel's body is not its
        // hole. Nothing of the sort is written rather than the bounding box's
        // numbers, which agree with Revit's on none of the 185 windows both
        // files hold.
        "IFCDOOR" | "IFCWINDOW" | "IFCOPENINGELEMENT" => {
            vec![("Volume", Quantity::Volume(net_volume))]
        }
        // Everything else is measured as a body and nothing more: nothing
        // here knows which way it runs.
        _ => vec![
            ("NetSurfaceArea", Quantity::Area(net_surface_area)),
            ("TotalSurfaceArea", Quantity::Area(net_surface_area)),
            ("NetVolume", Quantity::Volume(net_volume)),
        ]
        .into_iter()
        .chain(gross(net_surface_area).map(|area| ("GrossSurfaceArea", Quantity::Area(area))))
        .chain(gross(net_volume).map(|volume| ("GrossVolume", Quantity::Volume(volume))))
        .collect(),
    }
}

/// Write what has been measured from `element`'s body, as the quantity set the
/// entity's own template defines. Nothing is written where the body cannot be
/// measured exactly, or where the set has no name for what was measured.
fn push_quantities(
    file: &mut StepFile,
    element: &BimElement,
    product: EntityRef,
    product_entity: &str,
    context: WriteContext<'_>,
) {
    let Some((set_name, allowed)) = base_quantity_set(product_entity) else {
        return;
    };
    let mut quantities = Vec::new();
    let mut written = Vec::new();
    // The build-up's own total, written in the file's length unit because
    // `Width` is an `IfcQuantityLength` and every length in the file is in
    // that unit. It is not measured from the body: the layer table states it,
    // which is what the quantity means. See [`layer_set_thickness`].
    if allowed.contains(&"Width") {
        if let Some(thickness) = layer_set_thickness(element) {
            written.push("Width");
            quantities.push(reference(file.push(
                "IFCQUANTITYLENGTH",
                vec![
                    string("Width"),
                    omitted(),
                    omitted(),
                    context.lengths.value(thickness),
                    omitted(),
                ],
            )));
        }
    }
    if let Some(measured) = element.geometry.as_ref().and_then(quantities::measure) {
        for (name, quantity) in measured_quantities(product_entity, &measured) {
            if !allowed.contains(&name) || written.contains(&name) {
                continue;
            }
            let (entity, value) = match quantity {
                Quantity::Length(metres) => ("IFCQUANTITYLENGTH", context.lengths.value(metres)),
                Quantity::Area(square_metres) => {
                    ("IFCQUANTITYAREA", StepValue::Real(square_metres))
                }
                Quantity::Volume(cubic_metres) => {
                    ("IFCQUANTITYVOLUME", StepValue::Real(cubic_metres))
                }
            };
            // A quantity that measured to nothing is not a measurement of
            // this body; it is what a body this cannot read looks like.
            let StepValue::Real(number) = value else {
                continue;
            };
            if !number.is_finite() || number <= 0.0 {
                continue;
            }
            written.push(name);
            quantities.push(reference(file.push(
                entity,
                vec![string(name), omitted(), omitted(), value, omitted()],
            )));
        }
    }
    if quantities.is_empty() {
        return;
    }
    let set = file.push(
        "IFCELEMENTQUANTITY",
        vec![
            global_id(context.options, &format!("quantities:{}", element.id.0)),
            reference(context.owner),
            string(set_name),
            omitted(),
            omitted(),
            StepValue::List(quantities),
        ],
    );
    file.push(
        "IFCRELDEFINESBYPROPERTIES",
        vec![
            global_id(
                context.options,
                &format!("quantities-relation:{}", element.id.0),
            ),
            reference(context.owner),
            omitted(),
            omitted(),
            StepValue::List(vec![reference(product)]),
            reference(set),
        ],
    );
}

/// IFC's own property set for an element written as this entity, where the
/// property set templates define one that holds `Reference`.
///
/// The names are the templates' own - see `ifc4_entities` - rather than a rule
/// applied to the entity name: a building element's set is
/// `Pset_<Entity>Common` but a distribution element's is
/// `Pset_<Entity>TypeCommon`, which the template declares applicable to the
/// occurrence as well as to the type. An entity with no such set gets none.
fn common_property_set(entity: &str) -> Option<&'static str> {
    IFC4_COMMON_PROPERTY_SETS
        .iter()
        .find(|(name, _)| *name == entity)
        .map(|(_, pset)| *pset)
}

/// The IFC common property sets written so far, and the products carrying each.
///
/// The only property in them is `Reference`, which holds the element's type
/// name - so every element of one type carries the identical set, and it is
/// written once and related to all of them at the end. A reader sees on each
/// element exactly what Revit's own export puts there; what it does not see is
/// the same six properties repeated 7 617 times.
#[derive(Default)]
struct CommonPropertySets {
    /// `(set name, reference)` to the set and the products carrying it.
    sets: BTreeMap<(&'static str, String), (EntityRef, Vec<EntityRef>)>,
}

impl CommonPropertySets {
    /// Record that `product` carries the common set for its type, writing the
    /// set the first time that name and reference are seen.
    fn associate(
        &mut self,
        file: &mut StepFile,
        element: &BimElement,
        product: EntityRef,
        product_entity: &str,
        context: WriteContext<'_>,
    ) {
        let Some(name) = common_property_set(product_entity) else {
            return;
        };
        // Revit's `Reference` is the element's type name, which is the one
        // thing in these sets this decode establishes. An element whose type
        // name was not recovered carries no set rather than an empty one.
        let Some(type_reference) = element.type_name.as_deref() else {
            return;
        };
        let entry = self
            .sets
            .entry((name, type_reference.to_owned()))
            .or_insert_with(|| {
                let identity = format!("common:{name}:{type_reference}");
                let property = file.push(
                    "IFCPROPERTYSINGLEVALUE",
                    vec![
                        string("Reference"),
                        omitted(),
                        StepValue::Typed {
                            name: "IFCIDENTIFIER".to_owned(),
                            value: Box::new(string(type_reference)),
                        },
                        omitted(),
                    ],
                );
                let set = file.push(
                    "IFCPROPERTYSET",
                    vec![
                        global_id(context.options, &identity),
                        reference(context.owner),
                        string(name),
                        omitted(),
                        StepValue::List(vec![reference(property)]),
                    ],
                );
                (set, Vec::new())
            });
        entry.1.push(product);
    }

    /// One `IfcRelDefinesByProperties` per distinct set.
    fn push_relations(self, file: &mut StepFile, context: WriteContext<'_>) {
        for ((name, value), (set, products)) in self.sets {
            if products.is_empty() {
                continue;
            }
            file.push(
                "IFCRELDEFINESBYPROPERTIES",
                vec![
                    global_id(context.options, &format!("common-relation:{name}:{value}")),
                    reference(context.owner),
                    omitted(),
                    omitted(),
                    StepValue::List(products.into_iter().map(reference).collect()),
                    reference(set),
                ],
            );
        }
    }
}

/// The material layer sets written so far, and the products that carry them.
///
/// A build-up belongs to a type, not to an element: 13 193 walls of AR S1 share
/// 111 of them. Writing one `IfcMaterialLayerSet` per product would repeat the
/// same layers thousands of times, so each distinct set is written once, keyed
/// by the record it was read from, and one `IfcRelAssociatesMaterial` at the
/// end relates every product that carries it - which is also what Revit's own
/// export does.
#[derive(Default)]
struct MaterialLibrary {
    /// `IfcMaterial` by the material's identity, so a material used by many
    /// layers - or named by many faces - is written once.
    materials: BTreeMap<String, EntityRef>,
    /// `IfcMaterialLayerSet` by the set's identity, with the products carrying
    /// it. `BTreeMap` rather than a hash so the output stays deterministic.
    layer_sets: BTreeMap<String, (EntityRef, Vec<EntityRef>)>,
    /// An `IfcMaterial` by its own identity, with the products whose faces
    /// name it and nothing else - a door, a window, anything with no layered
    /// build-up of its own. Kept apart from `layer_sets`: the entity here is
    /// the material itself, not a set wrapping it.
    single_materials: BTreeMap<String, (EntityRef, Vec<EntityRef>)>,
    /// `IfcSurfaceStyle` by the colour it paints, so two faces - of one
    /// product or of two - that share a colour share the style entity too.
    /// See [`MaterialLibrary::style_disagreeing_faces`].
    styles: BTreeMap<(u8, u8, u8), EntityRef>,
}

impl MaterialLibrary {
    /// Record that `product` is made of `element`'s layers, writing the layer
    /// set the first time it is seen.
    fn associate(
        &mut self,
        file: &mut StepFile,
        element: &BimElement,
        product: EntityRef,
        lengths: Lengths,
    ) {
        let Some(set) = &element.material_layers else {
            return;
        };
        if set.layers.is_empty() {
            return;
        }
        // The build-up's own identity: the type it was read from where the
        // element wears its type's, and the element itself where it declares
        // one. Both name the same record, so a type and its instances share
        // one set rather than writing two identical ones.
        let identity = format!(
            "layers:{}",
            set.source_type_id.as_ref().unwrap_or(&element.id).0
        );
        let entry = match self.layer_sets.entry(identity) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let layers = set
                    .layers
                    .iter()
                    .filter_map(|layer| {
                        push_material_layer(file, &mut self.materials, layer, lengths)
                    })
                    .map(reference)
                    .collect::<Vec<_>>();
                if layers.is_empty() {
                    return;
                }
                let layer_set = file.push(
                    "IFCMATERIALLAYERSET",
                    vec![
                        StepValue::List(layers),
                        optional_string(set.name.as_deref()),
                        omitted(),
                    ],
                );
                entry.insert((layer_set, Vec::new()))
            }
        };
        entry.1.push(product);
    }

    /// Record that `product`'s own faces name one material and no build-up -
    /// an element `associate` above already gave a layer set never reaches
    /// here, since a build-up is the more complete statement where the
    /// source gives both. Where the faces disagree, no single
    /// `IfcRelAssociatesMaterial` is honest - a product whose faces name two
    /// different materials is a real multi-material body this export does
    /// not state as one material - so `written_faces` is styled face by face
    /// instead, through [`Self::style_disagreeing_faces`].
    fn associate_face_material(
        &mut self,
        file: &mut StepFile,
        element: &BimElement,
        product: EntityRef,
        written_faces: &[Option<EntityRef>],
    ) {
        if element
            .material_layers
            .as_ref()
            .is_some_and(|set| !set.layers.is_empty())
        {
            return;
        }
        let Some(faces) = element_faces(element) else {
            return;
        };
        // Every material identified before any of them is written: writing
        // the first as it is seen would leave a lone `IFCMATERIAL` behind for
        // a product this then refuses to relate, once a second, disagreeing
        // face turns up.
        let mut identities = faces
            .iter()
            .filter_map(|face| face.material.as_deref().and_then(material_identity));
        let Some(identity) = identities.next() else {
            return;
        };
        if identities.any(|other| other != identity) {
            self.style_disagreeing_faces(file, &faces, written_faces);
            return;
        }
        let material = faces
            .iter()
            .find_map(|face| face.material.as_deref())
            .expect("at least one face carried a material to reach this point");
        let Some((identity, entity)) = material_entity(file, &mut self.materials, material) else {
            return;
        };
        self.single_materials
            .entry(identity)
            .or_insert_with(|| (entity, Vec::new()))
            .1
            .push(product);
    }

    /// Paint each face of a product whose faces disagree on material with an
    /// `IfcStyledItem` carrying that one face's own colour - a visual
    /// statement, not the semantic one `IfcRelAssociatesMaterial` makes: two
    /// faces styled the same colour here are not thereby said to be the same
    /// material, only to look like it. `IfcSurfaceStyle` is written once per
    /// distinct colour and shared by every face that colour paints, the same
    /// way `self.materials` shares one `IfcMaterial` across every face or
    /// layer that names it.
    ///
    /// A face this cannot style - no material, no colour read for that
    /// material, or no written entity because the body took the swept-solid
    /// path or shares a body with an element `written_faces` was not filled
    /// in for - is left unstyled rather than guessed at, the same honesty
    /// [`Self::associate_face_material`] already keeps for the product as a
    /// whole.
    fn style_disagreeing_faces(
        &mut self,
        file: &mut StepFile,
        faces: &[&BimBrepFace],
        written_faces: &[Option<EntityRef>],
    ) {
        for (face, item) in faces.iter().zip(written_faces) {
            let Some(item) = item else { continue };
            let Some(color) = face
                .material
                .as_deref()
                .and_then(|material| material.color)
            else {
                continue;
            };
            let style = *self
                .styles
                .entry((color.red, color.green, color.blue))
                .or_insert_with(|| push_surface_style(file, color));
            file.push(
                "IFCSTYLEDITEM",
                vec![
                    reference(*item),
                    StepValue::List(vec![reference(style)]),
                    omitted(),
                ],
            );
        }
    }

    /// One `IfcRelAssociatesMaterial` per distinct build-up or single
    /// material.
    fn push_associations(self, file: &mut StepFile, options: &MetadataOptions, owner: EntityRef) {
        let relations = self
            .layer_sets
            .into_iter()
            .chain(self.single_materials)
            .map(|(identity, (entity, products))| {
                (format!("material-relation:{identity}"), entity, products)
            });
        for (identity, entity, products) in relations {
            if products.is_empty() {
                continue;
            }
            file.push(
                "IFCRELASSOCIATESMATERIAL",
                vec![
                    global_id(options, &identity),
                    reference(owner),
                    omitted(),
                    omitted(),
                    StepValue::List(products.into_iter().map(reference).collect()),
                    reference(entity),
                ],
            );
        }
    }
}

/// Every face of `element`'s own geometry - one body's, or every member's of
/// an assembly - or `None` where it carries no boundary representation at
/// all.
fn element_faces(element: &BimElement) -> Option<Vec<&BimBrepFace>> {
    Some(match &element.geometry {
        Some(BimGeometry::Brep(brep)) => brep.faces.iter().collect(),
        Some(BimGeometry::Assembly(parts)) => parts.iter().flat_map(|brep| &brep.faces).collect(),
        _ => return None,
    })
}

/// `IfcSurfaceStyle` wrapping one `IfcColourRgb`, shaded rather than
/// rendered: `IfcSurfaceStyleShading` states only the colour this export
/// actually read. `IfcSurfaceStyleRendering` would ask for a transparency,
/// a specular exponent and a reflectance method this export does not have -
/// Revit's own render appearance, not the shading colour this reads - and
/// writing plausible defaults for them would be exactly the guess this
/// codebase does not make.
fn push_surface_style(file: &mut StepFile, color: bim_core::BimColor) -> EntityRef {
    let normalised = |channel: u8| StepValue::Real(f64::from(channel) / 255.0);
    let rgb = file.push(
        "IFCCOLOURRGB",
        vec![
            omitted(),
            normalised(color.red),
            normalised(color.green),
            normalised(color.blue),
        ],
    );
    let shading = file.push("IFCSURFACESTYLESHADING", vec![reference(rgb), omitted()]);
    file.push(
        "IFCSURFACESTYLE",
        vec![
            omitted(),
            enumeration("BOTH"),
            StepValue::List(vec![reference(shading)]),
        ],
    )
}

/// The identity `material_entity` would key its cache by, computed without
/// touching the file - so a caller can tell whether several materials agree
/// *before* committing any of them to an entity. `None` where the material
/// carries no name: `IfcMaterial.Name` is not an optional attribute, and a
/// name this file could not resolve - the `MaterialElem` a face names is not
/// among the elements this file recovered - is not one to invent.
fn material_identity(material: &BimMaterial) -> Option<String> {
    let name = material.name.as_deref()?;
    Some(material.id.as_ref().map_or_else(
        || format!("name:{name}"),
        |id| format!("id:{}", external_id(id)),
    ))
}

/// `IfcMaterial` for `material`, written the first time this identity is
/// seen and cached in `materials` thereafter, with the identity string
/// returned alongside so a caller can key its own table by it.
fn material_entity(
    file: &mut StepFile,
    materials: &mut BTreeMap<String, EntityRef>,
    material: &BimMaterial,
) -> Option<(String, EntityRef)> {
    let name = material.name.as_deref()?;
    let identity = material_identity(material)?;
    let entity = *materials
        .entry(identity.clone())
        .or_insert_with(|| file.push("IFCMATERIAL", vec![string(name), omitted(), omitted()]));
    Some((identity, entity))
}

/// One `IfcMaterialLayer`, with its `IfcMaterial` written once per material.
///
/// `LayerThickness` is `IfcNonNegativeLengthMeasure`, so the thickness must be
/// in the file's length unit and not negative; a layer whose thickness is
/// neither is dropped rather than written as something else. Zero is kept: a
/// membrane is a real layer with no thickness.
///
/// `Category` and `Priority` are left unset. The source carries a layer
/// function code but nothing has established what its values mean, and IFC's
/// `Category` is an enumerated vocabulary - writing one would be a guess.
fn push_material_layer(
    file: &mut StepFile,
    materials: &mut BTreeMap<String, EntityRef>,
    layer: &BimMaterialLayer,
    lengths: Lengths,
) -> Option<EntityRef> {
    if layer.thickness.unit.as_ref()?.id != "autodesk.unit.unit:meters-1.0.0"
        || !layer.thickness.value.is_finite()
        || layer.thickness.value < 0.0
    {
        return None;
    }
    let material = layer
        .material
        .as_ref()
        .and_then(|material| material_entity(file, materials, material));
    Some(file.push(
        "IFCMATERIALLAYER",
        vec![
            material.map_or_else(omitted, |(_, entity)| reference(entity)),
            lengths.value(layer.thickness.value),
            omitted(),
            omitted(),
            omitted(),
            omitted(),
            omitted(),
        ],
    ))
}

fn push_property_set(
    file: &mut StepFile,
    element: &BimElement,
    product: EntityRef,
    context: WriteContext<'_>,
    type_carries_properties: bool,
) {
    if context.options.settings.property_sets.revit_parameters {
        push_named_property_set(
            file,
            &element.properties,
            product,
            context,
            "Rivet Properties",
            &format!("properties:{}", element.id.0),
            &format!("properties-relation:{}", element.id.0),
        );
    }
    // The type's values go in a set of their own, and where the type itself is
    // exported they hang off *it* rather than off each of its elements - that
    // is what `type_carries_properties` says. Where it is not, both sets hang
    // off the same product and the set name is what keeps "set on this
    // element" and "set on its type" apart for a reader.
    if context.options.settings.property_sets.revit_type_parameters && !type_carries_properties {
        push_named_property_set(
            file,
            &element.type_properties,
            product,
            context,
            "Rivet Type Properties",
            &format!("type-properties:{}", element.id.0),
            &format!("type-properties-relation:{}", element.id.0),
        );
    }
}

fn push_named_property_set(
    file: &mut StepFile,
    source: &[BimProperty],
    product: EntityRef,
    context: WriteContext<'_>,
    name: &str,
    key: &str,
    relation_key: &str,
) {
    let Some(pset) = push_property_set_entity(file, source, context, name, key) else {
        return;
    };
    file.push(
        "IFCRELDEFINESBYPROPERTIES",
        vec![
            global_id(context.options, relation_key),
            reference(context.owner),
            omitted(),
            omitted(),
            StepValue::List(vec![reference(product)]),
            reference(pset),
        ],
    );
}

/// One `IfcPropertySet`, without the relationship that carries it: a type
/// holds its own in `HasPropertySets`, where an element needs an
/// `IfcRelDefinesByProperties`. `None` where nothing in the source survived
/// into a property, which is not the same as an empty set.
fn push_property_set_entity(
    file: &mut StepFile,
    source: &[BimProperty],
    context: WriteContext<'_>,
    name: &str,
    key: &str,
) -> Option<EntityRef> {
    let names = unique_property_names(source);
    let properties = source
        .iter()
        .zip(&names)
        .filter_map(|(property, name)| push_property(file, property, name, context.lengths))
        .map(reference)
        .collect::<Vec<_>>();
    if properties.is_empty() {
        return None;
    }
    Some(file.push(
        "IFCPROPERTYSET",
        vec![
            global_id(context.options, key),
            reference(context.owner),
            string(name),
            omitted(),
            StepValue::List(properties),
        ],
    ))
}

/// `IfcPropertySet.UniquePropertyNames` requires the names within one set to
/// be unique, but distinct Revit built-in parameters share a display name -
/// `ALL_MODEL_DESCRIPTION` and `PROPERTY_SET_DESCRIPTION` are both
/// "Description", and the four `STAIRS_ATTR_CALC_*` are all "Calculation
/// Rules". Their values differ, so dropping the duplicates would lose data.
/// Every member of a colliding group is therefore qualified by the parameter
/// identifier that distinguishes it; a name that does not collide is left
/// exactly as it was declared.
fn unique_property_names(source: &[BimProperty]) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for property in source {
        *counts.entry(property.name.as_str()).or_default() += 1;
    }
    let mut used: BTreeSet<String> = BTreeSet::new();
    source
        .iter()
        .map(|property| {
            let mut name = property.name.clone();
            if counts.get(property.name.as_str()).copied().unwrap_or(0) > 1 {
                if let Some(id) = property.id.as_ref() {
                    name = format!("{} [{}]", property.name, id.value);
                }
            }
            // Two parameters may share both name and identifier; an occurrence
            // suffix is the last resort that keeps the set schema-valid.
            if used.contains(&name) {
                let base = name.clone();
                for occurrence in 2.. {
                    name = format!("{base} ({occurrence})");
                    if !used.contains(&name) {
                        break;
                    }
                }
            }
            used.insert(name.clone());
            name
        })
        .collect()
}

fn push_property(
    file: &mut StepFile,
    property: &BimProperty,
    name: &str,
    lengths: Lengths,
) -> Option<EntityRef> {
    let nominal = nominal_value(property, lengths)?;
    Some(file.push(
        "IFCPROPERTYSINGLEVALUE",
        vec![
            string(name),
            optional_string(property.id.as_ref().map(external_id).as_deref()),
            nominal,
            omitted(),
        ],
    ))
}

fn nominal_value(property: &BimProperty, lengths: Lengths) -> Option<StepValue> {
    let typed = |name: &str, value: StepValue| StepValue::Typed {
        name: name.to_owned(),
        value: Box::new(value),
    };
    match &property.value {
        BimPropertyValue::Bool(value) => Some(typed("IFCBOOLEAN", StepValue::Boolean(*value))),
        BimPropertyValue::Integer(value) => Some(typed("IFCINTEGER", StepValue::Integer(*value))),
        BimPropertyValue::Number(number) => {
            number_value(number, property.specification.as_deref(), lengths)
        }
        BimPropertyValue::Text(value) => Some(typed("IFCLABEL", string(value))),
        BimPropertyValue::Reference(value) => Some(typed("IFCIDENTIFIER", string(&value.0))),
        BimPropertyValue::Bytes(_) | BimPropertyValue::Unknown(_) => None,
    }
}

fn number_value(
    number: &BimNumber,
    specification: Option<&str>,
    lengths: Lengths,
) -> Option<StepValue> {
    if !number.value.is_finite() {
        return None;
    }
    let typed = |name: &str, value: StepValue| StepValue::Typed {
        name: name.to_owned(),
        value: Box::new(value),
    };
    let unit = number.unit.as_ref();
    let measure = match unit.map(|unit| unit.id.as_str()) {
        Some("autodesk.unit.unit:meters-1.0.0") => Some("IFCLENGTHMEASURE"),
        Some("autodesk.unit.unit:squareMeters-1.0.1") => Some("IFCAREAMEASURE"),
        Some("autodesk.unit.unit:cubicMeters-1.0.1") => Some("IFCVOLUMEMEASURE"),
        Some("autodesk.unit.unit:radians-1.0.0") => Some("IFCPLANEANGLEMEASURE"),
        Some("autodesk.unit.unit:seconds-1.0.0") => Some("IFCTIMEMEASURE"),
        Some("autodesk.unit.unit:kilograms-1.0.0") => Some("IFCMASSMEASURE"),
        Some("autodesk.unit.unit:amperes-1.0.0") => Some("IFCELECTRICCURRENTMEASURE"),
        Some("autodesk.unit.unit:kelvin-1.0.0") => Some("IFCTHERMODYNAMICTEMPERATUREMEASURE"),
        Some("autodesk.unit.unit:candelas-1.0.0") => Some("IFCLUMINOUSINTENSITYMEASURE"),
        Some("autodesk.unit.unit:general-1.0.1") => Some("IFCREAL"),
        // Derived quantities, in the SI units `push_derived_units` declares.
        // A spec whose storage unit is not SI is deliberately absent: the
        // catalogue converts an internal value into its spec's storage unit
        // and no further, and Revit stores a heat transfer coefficient in
        // British thermal units. Writing that as an SI measure would restate
        // it as a number it is not, so it keeps the marked label below.
        Some("autodesk.unit.unit:kilogramsPerMeter-1.0.1") => Some("IFCMASSPERLENGTHMEASURE"),
        Some("autodesk.unit.unit:kilogramsPerCubicMeter-1.0.1") => Some("IFCMASSDENSITYMEASURE"),
        Some("autodesk.unit.unit:kilogramsPerSquareMeter-1.0.1") => Some("IFCAREADENSITYMEASURE"),
        Some("autodesk.unit.unit:squareMeterKelvinsPerWatt-1.0.1") => {
            Some("IFCTHERMALRESISTANCEMEASURE")
        }
        Some("autodesk.unit.unit:metersToTheFourthPower-1.0.1") => {
            Some("IFCMOMENTOFINERTIAMEASURE")
        }
        // Revit's currency spec carries no dimension and no currency name, and
        // neither does `IfcMonetaryMeasure` without an `IfcMonetaryUnit` to
        // name one. AR S1's author declared five project parameters with it -
        // `SP_этаж`, `SP_подъезд`, `SP_квартира` and two more, 16 006 values
        // on exported products - and a reader of those wants the number the
        // file states, not a label that has to be parsed back into one.
        Some("autodesk.unit.unit:currency-1.0.0") => Some("IFCMONETARYMEASURE"),
        _ => None,
    };
    if let Some(measure) = measure {
        // A property's length is stated in the file's own length unit, like
        // every other length in it. An area and a volume are not: the unit
        // assignment keeps them in square and cubic metres whatever the
        // length unit is, which is what Revit's own export does too.
        let value = if measure == "IFCLENGTHMEASURE" {
            lengths.value(number.value)
        } else {
            StepValue::Real(number.value)
        };
        return Some(typed(measure, value));
    }
    let suffix = unit.map_or_else(
        || specification.unwrap_or("unit unknown"),
        |unit| unit.name.as_str(),
    );
    Some(typed(
        "IFCLABEL",
        string(&format!("{} [{suffix}]", number.value)),
    ))
}

fn push_aggregate(
    file: &mut StepFile,
    options: &MetadataOptions,
    owner: EntityRef,
    identity: &str,
    parent: EntityRef,
    children: Vec<EntityRef>,
) {
    file.push(
        "IFCRELAGGREGATES",
        vec![
            global_id(options, identity),
            reference(owner),
            omitted(),
            omitted(),
            reference(parent),
            StepValue::List(children.into_iter().map(reference).collect()),
        ],
    );
}

fn push_containment(
    file: &mut StepFile,
    options: &MetadataOptions,
    owner: EntityRef,
    identity: &str,
    container: EntityRef,
    elements: Vec<EntityRef>,
) {
    file.push(
        "IFCRELCONTAINEDINSPATIALSTRUCTURE",
        vec![
            global_id(options, &format!("containment:{identity}")),
            reference(owner),
            omitted(),
            omitted(),
            StepValue::List(elements.into_iter().map(reference).collect()),
            reference(container),
        ],
    );
}

fn global_id(options: &MetadataOptions, identity: &str) -> StepValue {
    string(IfcGuid::from_namespace_and_name(options.model_namespace, identity.as_bytes()).as_str())
}

fn external_id(id: &BimExternalId) -> String {
    format!("{}:{}", id.system, id.value)
}

fn reference(value: EntityRef) -> StepValue {
    StepValue::Reference(value)
}

fn string(value: &str) -> StepValue {
    StepValue::String(value.to_owned())
}

fn optional_string(value: Option<&str>) -> StepValue {
    value.map_or_else(omitted, string)
}

const fn omitted() -> StepValue {
    StepValue::Omitted
}

fn parameter_value(value: f64) -> StepValue {
    StepValue::Typed {
        name: "IFCPARAMETERVALUE".to_owned(),
        value: Box::new(StepValue::Real(value)),
    }
}

fn enumeration(value: &str) -> StepValue {
    StepValue::Enumeration(value.to_owned())
}

#[cfg(test)]
mod tests {
    /// One real, written exactly as the emitter writes it. A test that spells
    /// a number out itself is a second formatter to keep in step, so it asks
    /// the one formatter instead.
    fn real(value: f64) -> String {
        let mut bytes = Vec::new();
        crate::write_real(&mut bytes, value).expect("a finite real");
        String::from_utf8(bytes).expect("ascii")
    }

    use bim_core::BimElementType;

    use bim_core::{
        BimCategory, BimLineSegment, BimMaterial, BimMaterialLayerSet, BimPlacement, BimSweptDisk,
        BimUnit,
    };

    use super::*;

    fn options() -> MetadataOptions {
        MetadataOptions {
            model_namespace: [7; 16],
            file_name: "sample.ifc".to_owned(),
            timestamp: "2026-09-04T12:00:00+06:00".to_owned(),
            creation_time: 1_788_506_400,
            project_name: "Test Project".to_owned(),
            site_name: "Site".to_owned(),
            building_name: "Building".to_owned(),
            settings: {
                // These fixtures are about the spatial tree, the units and
                // the property sets, and most of their elements carry no
                // body at all. The rule that holds such an element back is
                // tested where it belongs - see the tests named for it -
                // rather than by silently emptying every other fixture.
                let mut settings = ExportSettings::default();
                settings.elements_without_a_body = true;
                settings
            },
        }
    }

    /// A body read straight in the model's own coordinates, in metres.
    fn test_frame() -> GeometryFrame {
        GeometryFrame {
            lengths: Lengths::new(LengthUnit::Metre),
            storey_elevation: 0.0,
            placement: None,
        }
    }

    fn model() -> BimModel {
        let level_id = BimElementId("100".to_owned());
        BimModel {
            source: None,
            project: None,
            site: None,
            documents: Vec::new(),
            levels: vec![BimLevel {
                id: level_id.clone(),
                name: Some("Этаж 1".to_owned()),
                elevation: Some(BimNumber {
                    value: 3.048,
                    unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
                }),
            }],
            elements: vec![BimElement {
                id: BimElementId("200".to_owned()),
                document: None,
                element_type: BimElementType::Unknown,
                class_name: Some("Wall".to_owned()),
                name: Some("Wall 1".to_owned()),
                long_name: None,
                category: Some(BimCategory {
                    id: None,
                    name: "OST_Walls".to_owned(),
                }),
                level_id: Some(level_id),
                type_id: None,
                type_name: None,
                host_id: None,
                placement: None,
                geometry: None,
                properties: vec![BimProperty {
                    id: None,
                    name: "Height".to_owned(),
                    specification: Some("autodesk.spec.aec:length-2.0.0".to_owned()),
                    value: BimPropertyValue::Number(BimNumber {
                        value: 2.5,
                        unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
                    }),
                }],
                type_properties: Vec::new(),
                material_layers: None,
            }],
            relations: Vec::new(),
        }
    }

    fn element(id: &str, class_name: &str, category_name: &str) -> BimElement {
        BimElement {
            id: BimElementId(id.to_owned()),
            document: None,
            element_type: BimElementType::Unknown,
            class_name: Some(class_name.to_owned()),
            name: Some(format!("Element {id}")),
            long_name: None,
            category: Some(BimCategory {
                id: None,
                name: category_name.to_owned(),
            }),
            level_id: Some(BimElementId("100".to_owned())),
            type_id: None,
            type_name: None,
            host_id: None,
            placement: None,
            geometry: None,
            properties: Vec::new(),
            type_properties: Vec::new(),
            material_layers: None,
        }
    }

    fn metres(value: f64) -> BimNumber {
        BimNumber {
            value,
            unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
        }
    }

    fn layer(name: &str, thickness: f64) -> BimMaterialLayer {
        BimMaterialLayer {
            material: Some(BimMaterial {
                id: Some(BimExternalId {
                    system: "autodesk.revit.elementId".to_owned(),
                    value: name.to_owned(),
                }),
                name: Some(format!("Material {name}")),
                color: None,
            }),
            thickness: metres(thickness),
            is_core: false,
            is_structural: false,
            source_function: Some(1),
        }
    }

    /// Two walls of one type share one layer set and one relationship, and the
    /// material every layer names is written once however many layers name it.
    /// A window and a door declare attributes on both sides of
    /// `PredefinedType`, and STEP writes every one of them. Counting the
    /// arguments is the check that none is dropped and nothing lands in the
    /// wrong slot.
    #[test]
    fn writes_every_attribute_a_window_and_a_door_declare() {
        let mut model = model();
        for (id, element_type) in [
            ("10", BimElementType::Window),
            ("11", BimElementType::Door),
            ("12", BimElementType::Railing),
        ] {
            let mut product = element(id, "FamilyInstance", "OST_Windows");
            product.element_type = element_type;
            model.elements.push(product);
        }
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for (entity, attributes) in [("IFCWINDOW", 13), ("IFCDOOR", 13), ("IFCRAILING", 9)] {
            let line = text
                .lines()
                .find(|line| line.contains(&format!("={entity}(")))
                .unwrap_or_else(|| panic!("no {entity}"));
            let arguments = line
                .rsplit_once('(')
                .unwrap()
                .1
                .trim_end_matches(");")
                .matches(',')
                .count()
                + 1;
            assert_eq!(arguments, attributes, "{line}");
        }
    }

    /// A door names its host wall's id in `host_id`; this writes an
    /// `IfcOpeningElement` for it, voids the wall with it and fills it with
    /// the door - the three relationships a clash check reads a doorway from.
    #[test]
    fn writes_an_opening_that_voids_the_host_and_is_filled_by_the_element() {
        let mut model = model();
        let mut wall = element("10", "SWall", "OST_Walls");
        wall.element_type = BimElementType::Wall;
        let mut door = element("11", "FamilyInstance", "OST_Doors");
        door.element_type = BimElementType::Door;
        door.host_id = Some(BimElementId("10".to_owned()));
        model.elements = vec![wall, door];

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(text.matches("=IFCOPENINGELEMENT(").count(), 1);
        assert_eq!(text.matches("=IFCRELVOIDSELEMENT(").count(), 1);
        assert_eq!(text.matches("=IFCRELFILLSELEMENT(").count(), 1);

        let entity_number = |needle: &str| -> &str {
            text.lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no {needle}"))
                .split('=')
                .next()
                .unwrap()
        };
        let wall_entity = entity_number("=IFCWALL(");
        let door_entity = entity_number("=IFCDOOR(");
        let opening_entity = entity_number("=IFCOPENINGELEMENT(");
        let voids = text
            .lines()
            .find(|line| line.contains("=IFCRELVOIDSELEMENT("))
            .unwrap();
        let fills = text
            .lines()
            .find(|line| line.contains("=IFCRELFILLSELEMENT("))
            .unwrap();
        assert!(
            voids.contains(&format!("{wall_entity},{opening_entity})")),
            "{voids}"
        );
        assert!(
            fills.contains(&format!("{opening_entity},{door_entity})")),
            "{fills}"
        );
    }

    /// A door naming a host this export held out of the file - a category the
    /// class mapping table excludes, say - gets no opening: a void relating
    /// nothing to a door is not a fact this export can state.
    #[test]
    fn an_opening_is_left_out_where_its_host_writes_no_product() {
        let mut model = model();
        let mut door = element("11", "FamilyInstance", "OST_Doors");
        door.element_type = BimElementType::Door;
        door.host_id = Some(BimElementId("missing".to_owned()));
        model.elements = vec![door];

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(!text.contains("=IFCOPENINGELEMENT("));
        assert!(!text.contains("=IFCRELVOIDSELEMENT("));
        assert!(!text.contains("=IFCRELFILLSELEMENT("));
    }

    /// `--no-openings` leaves the relationships out entirely, for a caller
    /// that does not want them.
    #[test]
    fn openings_can_be_left_out() {
        let mut model = model();
        let mut wall = element("10", "SWall", "OST_Walls");
        wall.element_type = BimElementType::Wall;
        let mut door = element("11", "FamilyInstance", "OST_Doors");
        door.element_type = BimElementType::Door;
        door.host_id = Some(BimElementId("10".to_owned()));
        model.elements = vec![wall, door];
        let mut options = options();
        options.settings.openings = false;

        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(!text.contains("=IFCOPENINGELEMENT("));
        assert!(!text.contains("=IFCRELVOIDSELEMENT("));
        assert!(!text.contains("=IFCRELFILLSELEMENT("));
    }

    #[test]
    fn writes_one_layer_set_per_build_up_and_relates_every_product_to_it() {
        let mut model = model();
        let set = BimMaterialLayerSet {
            source_type_id: Some(BimElementId("700".to_owned())),
            name: Some("Wall 250".to_owned()),
            layers: vec![layer("1", 0.0125), layer("2", 0.225), layer("1", 0.0125)],
        };
        for id in ["10", "11"] {
            let mut wall = element(id, "SWall", "OST_Walls");
            wall.element_type = BimElementType::Wall;
            wall.material_layers = Some(set.clone());
            model.elements.push(wall);
        }
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("=IFCMATERIALLAYERSET(").count(), 1);
        assert_eq!(text.matches("=IFCMATERIALLAYER(").count(), 3);
        assert_eq!(text.matches("=IFCRELASSOCIATESMATERIAL(").count(), 1);
        // Two distinct materials over three layers.
        assert_eq!(text.matches("=IFCMATERIAL(").count(), 2);
        assert!(text.contains("'Wall 250'"));
        let relation = text
            .lines()
            .find(|line| line.contains("=IFCRELASSOCIATESMATERIAL("))
            .unwrap();
        assert_eq!(relation.matches('#').count(), 5, "{relation}");
    }

    /// A layer whose thickness is not in the file's length unit is dropped
    /// rather than written as a bare number in some other unit.
    #[test]
    fn drops_a_layer_whose_thickness_is_not_metric() {
        let mut model = model();
        let mut wall = element("10", "SWall", "OST_Walls");
        wall.element_type = BimElementType::Wall;
        let mut feet = layer("1", 0.75);
        feet.thickness.unit = Some(BimUnit::new("autodesk.unit.unit:feet-1.0.0", "Feet"));
        wall.material_layers = Some(BimMaterialLayerSet {
            source_type_id: None,
            name: None,
            layers: vec![feet],
        });
        model.elements.push(wall);
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("=IFCMATERIALLAYER("));
        assert!(!text.contains("=IFCMATERIALLAYERSET("));
        assert!(!text.contains("=IFCRELASSOCIATESMATERIAL("));
    }

    fn face_material(id: &str, name: &str) -> BimMaterial {
        BimMaterial {
            id: Some(BimExternalId {
                system: "autodesk.revit.elementId".to_owned(),
                value: id.to_owned(),
            }),
            name: Some(name.to_owned()),
            color: None,
        }
    }

    fn face_material_with_color(id: &str, name: &str, color: (u8, u8, u8)) -> BimMaterial {
        BimMaterial {
            color: Some(bim_core::BimColor {
                red: color.0,
                green: color.1,
                blue: color.2,
            }),
            ..face_material(id, name)
        }
    }

    /// A door or a window carries no build-up, so `associate` above never
    /// reaches it - but its own faces agree on one material, and that alone
    /// is a fact this export can state: one `IfcMaterial`, related straight
    /// to the product rather than through a layer set nothing here read.
    #[test]
    fn writes_a_single_material_where_every_face_of_a_bodyless_product_agrees() {
        let mut model = model();
        let mut door = element("10", "FamilyInstance", "OST_Doors");
        door.element_type = BimElementType::Door;
        let mut brep = box_brep(true);
        for face in &mut brep.faces {
            face.material = Some(Box::new(face_material("42", "Glass")));
        }
        door.geometry = Some(BimGeometry::Brep(brep));
        model.elements.push(door);

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(text.matches("=IFCMATERIAL(").count(), 1);
        assert!(text.contains("=IFCMATERIAL('Glass',$,$)"), "{text}");
        assert_eq!(text.matches("=IFCRELASSOCIATESMATERIAL(").count(), 1);
        assert!(!text.contains("=IFCMATERIALLAYERSET("));
        let relation = text
            .lines()
            .find(|line| line.contains("=IFCRELASSOCIATESMATERIAL("))
            .unwrap();
        // Product and material: two references, not the three a layer set's
        // relation carries an extra one of.
        assert_eq!(relation.matches('#').count(), 4, "{relation}");
    }

    /// A product whose faces name two different materials is a real
    /// multi-material body this export does not yet state - not a hint to
    /// guess one of the two and call it the product's material.
    #[test]
    fn writes_no_material_where_a_bodyless_products_faces_disagree() {
        let mut model = model();
        let mut door = element("10", "FamilyInstance", "OST_Doors");
        door.element_type = BimElementType::Door;
        let mut brep = box_brep(true);
        brep.faces[0].material = Some(Box::new(face_material("42", "Glass")));
        brep.faces[1].material = Some(Box::new(face_material("43", "Aluminium")));
        door.geometry = Some(BimGeometry::Brep(brep));
        model.elements.push(door);

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(!text.contains("=IFCMATERIAL("));
        assert!(!text.contains("=IFCRELASSOCIATESMATERIAL("));
    }

    /// Where the faces disagree, no product-level material is honest - but a
    /// face this export wrote its own `IfcAdvancedFace` for, and whose
    /// material carries a colour, is not left bare either: it gets an
    /// `IfcStyledItem` of its own, and two faces of the same colour share one
    /// `IfcSurfaceStyle`. `box_brep` recognises as a sweep and writes no
    /// per-face entity at all, so this uses `quarter_disc_brep` - curved,
    /// never a prism - twice over, the same shape both instances of
    /// `writes_a_complete_brep_as_an_advanced_brep` already prove becomes
    /// real `IfcAdvancedFace`s.
    #[test]
    fn styles_each_face_of_a_disagreeing_product_by_its_own_colour() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::SanitaryTerminal;
        let mut brep = quarter_disc_brep(true);
        brep.faces.push(quarter_disc_brep(true).faces.remove(0));
        brep.faces[0].material = Some(Box::new(face_material_with_color(
            "42",
            "Glass",
            (0xba, 0xbf, 0xc5),
        )));
        brep.faces[1].material = Some(Box::new(face_material_with_color(
            "43",
            "Aluminium",
            (0xba, 0xbf, 0xc5),
        )));
        model.elements[0].geometry = Some(BimGeometry::Brep(brep));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        // Still no product-level material: the two faces disagree.
        assert!(!text.contains("=IFCMATERIAL("), "{text}");
        assert!(!text.contains("=IFCRELASSOCIATESMATERIAL("));

        // One shared colour and style, styling two distinct faces.
        assert_eq!(text.matches("=IFCCOLOURRGB(").count(), 1, "{text}");
        assert_eq!(text.matches("=IFCSURFACESTYLESHADING(").count(), 1);
        assert_eq!(text.matches("=IFCSURFACESTYLE(").count(), 1);
        assert_eq!(text.matches("=IFCSTYLEDITEM(").count(), 2, "{text}");
        let colour = text
            .lines()
            .find(|line| line.contains("=IFCCOLOURRGB("))
            .expect("one shared colour");
        // 0xba, 0xbf, 0xc5 over 255, close enough that the writer's own
        // shortest-round-trip rounding cannot land elsewhere.
        assert!(colour.contains("0.7294117"), "{colour}");
        assert!(colour.contains("0.7490196"), "{colour}");
        assert!(colour.contains("0.7725490"), "{colour}");
        let advanced_faces: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("=IFCADVANCEDFACE("))
            .collect();
        assert_eq!(advanced_faces.len(), 2, "{advanced_faces:?}");
        for face in advanced_faces {
            let face_ref = face.split('=').next().unwrap();
            assert!(
                text.lines().any(|line| {
                    line.contains("=IFCSTYLEDITEM(") && line.contains(&format!("({face_ref},"))
                }),
                "no IfcStyledItem styles {face_ref}: {text}"
            );
        }
    }

    /// An element with its own layered build-up states that, not its faces'
    /// materials: a wall's own layer set is already the more complete
    /// statement, and a face material beside it would either repeat it or
    /// contradict it for no reason this export can tell apart.
    #[test]
    fn a_products_own_layers_are_preferred_over_its_faces_material() {
        let mut model = model();
        let mut wall = element("10", "SWall", "OST_Walls");
        wall.element_type = BimElementType::Wall;
        wall.material_layers = Some(BimMaterialLayerSet {
            source_type_id: None,
            name: Some("Wall 200".to_owned()),
            layers: vec![layer("1", 0.2)],
        });
        let mut brep = box_brep(true);
        for face in &mut brep.faces {
            face.material = Some(Box::new(face_material("42", "Glass")));
        }
        wall.geometry = Some(BimGeometry::Brep(brep));
        model.elements.push(wall);

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(text.matches("=IFCMATERIALLAYERSET(").count(), 1);
        assert_eq!(text.matches("=IFCRELASSOCIATESMATERIAL(").count(), 1);
        assert!(!text.contains("'Glass'"), "{text}");
    }

    #[test]
    fn writes_the_metadata_spatial_tree_proxy_and_property() {
        let file = metadata_ifc(&model(), &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for entity in [
            "IFCPROJECT",
            "IFCSITE",
            "IFCBUILDING",
            "IFCBUILDINGSTOREY",
            "IFCBUILDINGELEMENTPROXY",
            "IFCRELCONTAINEDINSPATIALSTRUCTURE",
            "IFCPROPERTYSET",
            "IFCPROPERTYSINGLEVALUE",
        ] {
            assert!(text.contains(&format!("={entity}(")), "missing {entity}");
        }
        assert!(text.contains("IFCLENGTHMEASURE(2.5)"));
        assert!(text.contains("=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.)"));
        assert!(text.contains("=IFCOWNERHISTORY(#3,#4,$,.ADDED.,1788506400,#3,#4,1788506400)"));
        assert!(text.contains("'\\X2\\042D04420430043600200031\\X0\\'"));
    }

    /// A number whose spec is a derived quantity is written as that quantity's
    /// own measure, in the SI unit the assignment declares for it. It used to
    /// leave as a label with the unit's name in brackets, which a reader has
    /// to parse back into a number before it can be compared with anything.
    ///
    /// The currency spec goes the same way. AR S1's author declared five
    /// project parameters with it - a storey number among them - and what a
    /// reader of those wants is the number the file states.
    #[test]
    fn a_derived_quantity_is_written_as_its_own_measure() {
        let mut model = model();
        for (name, value, unit, unit_name) in [
            (
                "SP_этаж",
                8.0,
                "autodesk.unit.unit:currency-1.0.0",
                "Currency",
            ),
            (
                "Mass per Unit Length",
                12.5,
                "autodesk.unit.unit:kilogramsPerMeter-1.0.1",
                "Kilograms per meter",
            ),
            (
                "Thermal Resistance (R)",
                1.25,
                "autodesk.unit.unit:squareMeterKelvinsPerWatt-1.0.1",
                "Square meter kelvins per watt",
            ),
            (
                // Revit stores this one in British thermal units, so the
                // catalogue converts it no further and it stays marked.
                "Heat Transfer Coefficient (U)",
                0.5,
                "autodesk.unit.unit:britishThermalUnitsPerHourSquareFootDegreeFahrenheit-1.0.1",
                "BTU per hour square foot degree Fahrenheit",
            ),
        ] {
            model.elements[0].properties.push(BimProperty {
                id: None,
                name: name.to_owned(),
                specification: None,
                value: BimPropertyValue::Number(BimNumber {
                    value,
                    unit: Some(BimUnit::new(unit, unit_name)),
                }),
            });
        }

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("IFCMONETARYMEASURE(8.)"), "currency");
        assert!(text.contains("IFCMASSPERLENGTHMEASURE(12.5)"));
        assert!(text.contains("IFCTHERMALRESISTANCEMEASURE(1.25)"));
        // The unit each of those is read in, declared once in the assignment.
        assert!(text.contains("=IFCDERIVEDUNIT((#") && text.contains(".MASSPERLENGTHUNIT.,$)"));
        assert!(text.contains(".THERMALRESISTANCEUNIT.,$)"));
        assert!(text.contains("=IFCDERIVEDUNITELEMENT(#"));
        // Not converted, so not restated as a number it is not.
        assert!(
            text.contains("'0.5 [BTU per hour square foot degree Fahrenheit]'"),
            "an imperial storage unit keeps its marked label"
        );
    }

    /// The length unit changes the numbers and states itself; it changes
    /// nothing else. The storey elevation, the property's length measure and
    /// the geometry all move together, while the area beside them does not -
    /// the unit assignment keeps areas and volumes metric whatever the length
    /// unit is, which is what Revit's own export of the corpus does.
    #[test]
    fn a_millimetre_export_states_its_unit_and_scales_every_length_by_it() {
        let mut model = model();
        model.elements[0].properties.push(BimProperty {
            id: None,
            name: "Area".to_owned(),
            specification: None,
            value: BimPropertyValue::Number(BimNumber {
                value: 12.0,
                unit: Some(BimUnit::new(
                    "autodesk.unit.unit:squareMeters-1.0.1",
                    "square metres",
                )),
            }),
        });
        let mut options = options();
        options.settings.length_unit = LengthUnit::Millimetre;

        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.)"));
        // Still square metres, beside a millimetre length.
        assert!(text.contains("=IFCSIUNIT(*,.AREAUNIT.,$,.SQUARE_METRE.)"));
        assert!(
            text.contains("IFCLENGTHMEASURE(2500.)"),
            "the property's 2.5 m should be written as 2500 mm"
        );
        assert!(
            text.contains("IFCAREAMEASURE(12.)"),
            "the property's 12 m2 should stay 12 m2"
        );
        // The storey's own elevation, 3.048 m, and the model's precision.
        assert!(
            text.contains("=IFCBUILDINGSTOREY(") && text.contains(",3048.)"),
            "the storey elevation should be written in millimetres"
        );
        assert!(text.contains("0.01,"), "precision in mm");
    }

    #[test]
    fn a_room_is_written_as_a_space_the_storey_decomposes() {
        let mut model = model();
        let mut room = model.elements[0].clone();
        room.id = BimElementId("300".to_owned());
        room.element_type = BimElementType::Space;
        room.class_name = Some("RoomElem".to_owned());
        room.name = Some("204".to_owned());
        room.long_name = Some("Комната".to_owned());
        room.category = None;
        model.elements.push(room);

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        // Named by its number, called by its name, and a spatial element's
        // attributes: LongName, CompositionType, PredefinedType, and no Tag.
        let space = text
            .lines()
            .find(|line| line.contains("=IFCSPACE("))
            .expect("the room should be written as a space");
        assert!(space.contains("'204'"), "{space}");
        assert!(space.contains("'RoomElem'"), "{space}");
        assert!(space.ends_with(".ELEMENT.,.NOTDEFINED.,$);"), "{space}");

        // The storey decomposes it. A space is part of the spatial structure,
        // so it must not also be contained in it like an element.
        let space_reference = space.split('=').next().expect("an entity id");
        let names = |entity: &str| {
            text.lines()
                .filter(|line| line.contains(entity))
                .filter(|line| {
                    line.contains(&format!("{space_reference},"))
                        || line.contains(&format!("{space_reference})"))
                })
                .count()
        };
        assert_eq!(names("=IFCRELAGGREGATES("), 1, "aggregated exactly once");
        assert_eq!(
            names("=IFCRELCONTAINEDINSPATIALSTRUCTURE("),
            0,
            "a space is decomposed by its storey, not contained in it"
        );
        // The wall beside it is still contained, so the split is per element
        // and not a change of relationship for everything.
        let wall = text
            .lines()
            .find(|line| line.contains("=IFCBUILDINGELEMENTPROXY("))
            .expect("the wall should still be a proxy");
        let wall_reference = wall.split('=').next().expect("an entity id");
        assert!(text.lines().any(|line| {
            line.contains("=IFCRELCONTAINEDINSPATIALSTRUCTURE(")
                && (line.contains(&format!("{wall_reference},"))
                    || line.contains(&format!("{wall_reference})")))
        }));
    }

    #[test]
    fn a_type_s_properties_go_in_a_set_of_their_own() {
        let mut model = model();
        let mut element = model.elements[0].clone();
        element.type_properties = vec![BimProperty {
            id: None,
            name: "Manufacturer".to_owned(),
            specification: None,
            value: BimPropertyValue::Text("SANEXT".to_owned()),
        }];
        model.elements = vec![element];
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        // Both sets are written, under names that keep them apart, and the
        // type's value is not merged into the element's own set.
        assert!(text.contains("'Rivet Properties'"));
        assert!(text.contains("'Rivet Type Properties'"));
        assert_eq!(text.matches("=IFCPROPERTYSET(").count(), 2);
        assert_eq!(text.matches("=IFCRELDEFINESBYPROPERTIES(").count(), 2);
        assert!(text.contains("'SANEXT'"));

        // An element whose type stores nothing gets no second set.
        model.elements[0].type_properties.clear();
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("=IFCPROPERTYSET(").count(), 1);
        assert!(!text.contains("'Rivet Type Properties'"));
    }

    /// Two elements of one Revit type: one type object, one relationship
    /// carrying both, and the type's parameters stated once - on the type.
    #[test]
    fn writes_one_type_per_revit_type_and_relates_every_element_to_it() {
        let mut model = model();
        let mut first = model.elements[0].clone();
        first.element_type = BimElementType::Wall;
        first.type_id = Some(BimElementId("900".to_owned()));
        first.type_name = Some("Basic Wall: 200mm".to_owned());
        first.type_properties = vec![BimProperty {
            id: None,
            name: "Manufacturer".to_owned(),
            specification: None,
            value: BimPropertyValue::Text("SANEXT".to_owned()),
        }];
        let mut second = first.clone();
        second.id = BimElementId("201".to_owned());
        model.elements = vec![first, second];

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(text.matches("=IFCWALLTYPE(").count(), 1);
        assert_eq!(text.matches("=IFCRELDEFINESBYTYPE(").count(), 1);
        let relation = text
            .lines()
            .find(|line| line.contains("=IFCRELDEFINESBYTYPE("))
            .expect("the type relationship");
        assert_eq!(relation.matches('#').count(), 5, "{relation}");

        // The type is named and tagged by the record it was read from, and it
        // holds the type's parameters in `HasPropertySets`.
        let written = text
            .lines()
            .find(|line| line.contains("=IFCWALLTYPE("))
            .expect("the wall type");
        assert!(written.contains("'Basic Wall: 200mm'"), "{written}");
        assert!(written.contains(",'900',"), "{written}");
        assert!(text.contains("'Rivet Type Properties'"));
        assert_eq!(text.matches("'Rivet Type Properties'").count(), 1);
        // Stated on the type, so no element repeats it: the property
        // relationships left are the two elements' own parameters and the one
        // IFC common set both of them share.
        assert_eq!(text.matches("=IFCRELDEFINESBYPROPERTIES(").count(), 3);
    }

    /// A space names no family type - Revit has none to declare for a room -
    /// but Revit's own export of AR S1 still gives every one of its 553 spaces
    /// its own `IfcSpaceType`, never shared even between two identical rooms.
    /// Measured against that file: it is over a third of the type-relation
    /// count, and this export wrote none of it before this behaviour existed.
    #[test]
    fn gives_every_space_its_own_type_when_it_declares_none() {
        let mut model = model();
        let mut first = model.elements[0].clone();
        first.element_type = BimElementType::Space;
        first.category = None;
        first.name = Some("101".to_owned());
        first.long_name = Some("Office".to_owned());
        let mut second = first.clone();
        second.id = BimElementId("201".to_owned());
        second.name = Some("102".to_owned());
        second.long_name = Some("Office".to_owned());
        model.elements = vec![first, second];

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        // Two spaces, identically named, still get two types: the room's own
        // identity stands in for the type it has none of.
        assert_eq!(text.matches("=IFCSPACETYPE(").count(), 2);
        assert_eq!(text.matches("=IFCRELDEFINESBYTYPE(").count(), 2);
        let written: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("=IFCSPACETYPE("))
            .collect();
        assert_eq!(written.len(), 2);
        // Named after the room, which is what tells the two apart in a
        // viewer - `Tag`, not `Name`, is what a shared family type would
        // otherwise carry there.
        assert!(
            written.iter().all(|line| line.contains("'Office'")),
            "{written:?}"
        );
        assert_ne!(
            written[0], written[1],
            "two rooms of the same name still write two distinct types"
        );
    }

    /// The attribute layout, measured against the IFC Revit itself exported
    /// from AR S1: nine `IfcElementType` attributes and then the entity's own.
    /// A wall type ends at `PredefinedType` for ten, a space type adds
    /// `LongName` for eleven, and a door type adds three for thirteen.
    #[test]
    fn a_type_carries_the_attributes_revit_writes_for_it() {
        let attributes = |line: &str| {
            let arguments = line
                .split_once('(')
                .and_then(|(_, rest)| rest.rsplit_once(')'))
                .expect("an entity line")
                .0;
            let mut depth = 0;
            let mut quoted = false;
            let mut count = 1;
            for character in arguments.chars() {
                match character {
                    '\'' => quoted = !quoted,
                    '(' if !quoted => depth += 1,
                    ')' if !quoted => depth -= 1,
                    ',' if !quoted && depth == 0 => count += 1,
                    _ => {}
                }
            }
            count
        };
        for (element_type, entity, expected) in [
            (BimElementType::Wall, "=IFCWALLTYPE(", 10),
            (BimElementType::Door, "=IFCDOORTYPE(", 13),
            (BimElementType::Window, "=IFCWINDOWTYPE(", 13),
            (BimElementType::Space, "=IFCSPACETYPE(", 11),
            (BimElementType::Unknown, "=IFCBUILDINGELEMENTPROXYTYPE(", 10),
        ] {
            let mut model = model();
            let mut element = model.elements[0].clone();
            element.element_type = element_type;
            element.category = None;
            element.type_id = Some(BimElementId("900".to_owned()));
            model.elements = vec![element];
            let file = metadata_ifc(&model, &options()).unwrap();
            let mut bytes = Vec::new();
            file.write_to(&mut bytes).unwrap();
            let text = String::from_utf8(bytes).unwrap();
            let line = text
                .lines()
                .find(|line| line.contains(entity))
                .unwrap_or_else(|| panic!("missing {entity}"));
            assert_eq!(attributes(line), expected, "{line}");
        }
    }

    /// A box of known size: 1 x 2 x 3 metres at the origin, so six cubic
    /// metres of volume and twenty-two square metres of surface.
    fn box_brep(complete: bool) -> BimBrep {
        let point = |x: f64, y: f64, z: f64| BimPoint3 {
            coordinates: [x, y, z],
            unit: BimUnit::metres(),
        };
        let quad = |corners: [[f64; 3]; 4]| {
            let edge = |from: [f64; 3], to: [f64; 3]| BimBrepEdge {
                start: point(from[0], from[1], from[2]),
                end: point(to[0], to[1], to[2]),
                curve: BimBrepCurve::Line,
            };
            BimBrepFace {
                surface: BimBrepSurface::Plane {
                    origin: point(corners[0][0], corners[0][1], corners[0][2]),
                    x_axis: [1.0, 0.0, 0.0],
                    y_axis: [0.0, 1.0, 0.0],
                },
                loops: vec![vec![
                    edge(corners[0], corners[1]),
                    edge(corners[1], corners[2]),
                    edge(corners[2], corners[3]),
                    edge(corners[3], corners[0]),
                ]],
                material: None,
            }
        };
        let (x, y, z) = (1.0, 2.0, 3.0);
        BimBrep {
            complete,
            faces: vec![
                quad([[0.0, 0.0, 0.0], [x, 0.0, 0.0], [x, y, 0.0], [0.0, y, 0.0]]),
                quad([[0.0, 0.0, z], [0.0, y, z], [x, y, z], [x, 0.0, z]]),
                quad([[0.0, 0.0, 0.0], [0.0, y, 0.0], [0.0, y, z], [0.0, 0.0, z]]),
                quad([[x, 0.0, 0.0], [x, 0.0, z], [x, y, z], [x, y, 0.0]]),
                quad([[0.0, 0.0, 0.0], [0.0, 0.0, z], [x, 0.0, z], [x, 0.0, 0.0]]),
                quad([[0.0, y, 0.0], [x, y, 0.0], [x, y, z], [0.0, y, z]]),
            ],
        }
    }

    /// The quantities are measured from the body this file carries, in the
    /// units the file states for them - cubic and square metres, whatever the
    /// length unit is. Off unless asked for, as Revit's own switch is.
    #[test]
    fn measures_base_quantities_from_the_exported_solid() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].geometry = Some(BimGeometry::Brep(box_brep(true)));
        let mut options = options();
        options.settings.property_sets.base_quantities = true;
        // The unit does not touch them: an area is square metres in a
        // millimetre file too, which is what the unit assignment says.
        options.settings.length_unit = LengthUnit::Millimetre;

        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("=IFCELEMENTQUANTITY("));
        assert!(text.contains("'Qto_WallBaseQuantities'"));
        assert!(
            text.contains("=IFCQUANTITYVOLUME('NetVolume',$,$,6.,$)"),
            "one by two by three metres is six cubic metres"
        );
        // A wall's quantity set has no name for a total surface area, so none
        // is written; a proxy's does.
        assert!(!text.contains("NetSurfaceArea"));

        model.elements[0].element_type = BimElementType::Unknown;
        model.elements[0].category = None;
        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("'Qto_BuildingElementProxyQuantities'"));
        assert!(
            text.contains("=IFCQUANTITYAREA('NetSurfaceArea',$,$,22.,$)"),
            "twenty-two square metres of surface"
        );
    }

    /// The rest of the quantity set, from the frame the body itself
    /// declares: the upright face pair carrying the most area is what the
    /// element runs across, and its length, height and side area follow.
    ///
    /// The fixture is one by two by three metres, so the face pair carrying
    /// the most area is the two-by-three one; the wall therefore runs two
    /// metres along, one across and three up.
    #[test]
    fn measures_the_whole_quantity_set_in_the_bodys_own_frame() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].geometry = Some(BimGeometry::Brep(box_brep(true)));
        let mut options = options();
        options.settings.property_sets.base_quantities = true;

        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        for expected in [
            "=IFCQUANTITYLENGTH('Length',$,$,2.,$)",
            "=IFCQUANTITYLENGTH('Width',$,$,1.,$)",
            "=IFCQUANTITYLENGTH('Height',$,$,3.,$)",
            "=IFCQUANTITYAREA('GrossFootprintArea',$,$,2.,$)",
            "=IFCQUANTITYAREA('NetFootprintArea',$,$,2.,$)",
            "=IFCQUANTITYAREA('NetSideArea',$,$,6.,$)",
            // Nothing was cut out of this body, so its gross side is its net
            // side and the gross one can be stated.
            "=IFCQUANTITYAREA('GrossSideArea',$,$,6.,$)",
            "=IFCQUANTITYVOLUME('NetVolume',$,$,6.,$)",
            "=IFCQUANTITYVOLUME('GrossVolume',$,$,6.,$)",
        ] {
            assert!(text.contains(expected), "{expected} is missing from {text}");
        }
    }

    /// Two elements carrying the same body write that body once and reach it
    /// through a map apiece; two elements carrying different bodies write
    /// both. The placement stays the element's own, so the shared body stands
    /// where each element does.
    #[test]
    fn one_body_is_written_once_and_placed_by_every_element_that_has_it() {
        let mut model = model();
        let mut second = model.elements[0].clone();
        second.id = BimElementId("201".to_owned());
        second.name = Some("Wall 2".to_owned());
        model.elements.push(second);
        for element in &mut model.elements {
            element.geometry = Some(BimGeometry::Brep(box_brep(true)));
        }

        let (file, report) = metadata_ifc_reported(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(report.mapped_bodies, 1, "the second element maps the first");
        // The box is recognised as the prism it is, so the shared body is
        // one swept solid rather than one boundary representation.
        assert_eq!(text.matches("=IFCEXTRUDEDAREASOLID(").count(), 1);
        assert_eq!(text.matches("=IFCREPRESENTATIONMAP(").count(), 1);
        assert_eq!(text.matches("=IFCMAPPEDITEM(").count(), 2);
        assert_eq!(text.matches("'MappedRepresentation'").count(), 2);
        // Each element still carries its own placement and its own product.
        assert_eq!(text.matches("=IFCPRODUCTDEFINITIONSHAPE(").count(), 2);

        // A body of its own is a map of its own.
        model.elements[1].geometry = Some(BimGeometry::Brep(box_brep(false)));
        let (file, report) = metadata_ifc_reported(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(report.mapped_bodies, 0);
        assert_eq!(text.matches("=IFCREPRESENTATIONMAP(").count(), 2);
    }

    /// Two elements of one type, sharing a body, relate their type to the
    /// `IfcRepresentationMap` behind it through `RepresentationMaps` - what
    /// lets a reader recognise many occurrences of one family as one shared
    /// definition, the way `IfcMappedItem` already lets it recognise one
    /// occurrence.
    #[test]
    fn a_shared_body_reaches_its_type_through_representation_maps() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].type_id = Some(BimElementId("900".to_owned()));
        model.elements[0].type_name = Some("Basic Wall: 200mm".to_owned());
        model.elements[0].geometry = Some(BimGeometry::Brep(box_brep(true)));
        let mut second = model.elements[0].clone();
        second.id = BimElementId("201".to_owned());
        model.elements.push(second);

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(text.matches("=IFCREPRESENTATIONMAP(").count(), 1);
        let map_ref = text
            .lines()
            .find(|line| line.contains("=IFCREPRESENTATIONMAP("))
            .and_then(|line| line.split('=').next())
            .expect("the map's own reference")
            .to_owned();

        let written = text
            .lines()
            .find(|line| line.contains("=IFCWALLTYPE("))
            .expect("the wall type");
        assert!(
            written.contains(&format!("({map_ref})")),
            "RepresentationMaps should list the shared map {map_ref}: {written}"
        );

        // A type whose instances write no mapped body at all - shared bodies
        // turned off - states no `RepresentationMaps` rather than an empty
        // list: there is no map to point at.
        let mut unshared = options();
        unshared.settings.shared_bodies = false;
        let file = metadata_ifc(&model, &unshared).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("=IFCREPRESENTATIONMAP("));
        let written = text
            .lines()
            .find(|line| line.contains("=IFCWALLTYPE("))
            .expect("the wall type");
        // The only references left on the line are the type's own and
        // `OwnerHistory`: no property set (the fixture states no type
        // parameter) and no `RepresentationMaps`.
        assert_eq!(written.matches('#').count(), 2, "{written}");
    }

    /// An element this export carries no body for is not written at all, and
    /// the report says how many were held back. A space is exempt: a room is
    /// a product whether or not a body was read for it.
    #[test]
    fn an_element_with_no_body_is_left_out_and_counted() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].geometry = None;
        let mut settled = options();
        settled.settings.elements_without_a_body = false;

        let (file, report) = metadata_ifc_reported(&model, &settled).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("=IFCWALL("));
        assert_eq!(report.bodiless_elements, 1);

        // A box is what is known about where the element is, not what it is.
        model.elements[0].geometry = Some(BimGeometry::BoundingBox(BimBoundingBox {
            min: metres_point([0.0, 0.0, 0.0]),
            max: metres_point([1.0, 2.0, 3.0]),
        }));
        let (file, report) = metadata_ifc_reported(&model, &settled).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("=IFCWALL("));
        assert_eq!(report.bodiless_elements, 1);

        // Asked for, it is written as it was before.
        model.elements[0].geometry = None;
        let (file, report) = metadata_ifc_reported(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("=IFCWALL("));
        assert_eq!(report.bodiless_elements, 0);

        // And a room with no body is still a room.
        model.elements[0].element_type = BimElementType::Space;
        let (file, report) = metadata_ifc_reported(&model, &settled).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("=IFCSPACE("));
        assert_eq!(report.bodiless_elements, 0);
    }

    /// `Width` is the one base quantity the source states rather than one
    /// measured from the body, so it is written from the layer table, in the
    /// file's length unit, and for an element whose body could not be measured
    /// at all. An entity whose quantity set has no name for it gets none.
    #[test]
    fn writes_the_build_ups_own_thickness_as_width() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].material_layers = Some(BimMaterialLayerSet {
            source_type_id: Some(BimElementId("700".to_owned())),
            name: Some("Wall 250".to_owned()),
            layers: vec![layer("1", 0.0125), layer("2", 0.225), layer("1", 0.0125)],
        });
        let mut options = options();
        options.settings.property_sets.base_quantities = true;

        // No geometry at all: the layer table still states the thickness.
        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("'Qto_WallBaseQuantities'"));
        assert!(
            text.contains("=IFCQUANTITYLENGTH('Width',$,$,0.25,$)"),
            "12.5 + 225 + 12.5 millimetres is a quarter of a metre: {text}"
        );

        // A length rides in the file's unit, unlike a volume or an area.
        options.settings.length_unit = LengthUnit::Millimetre;
        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.contains("=IFCQUANTITYLENGTH('Width',$,$,250.,$)"),
            "the same quarter metre in a millimetre file: {text}"
        );

        // A proxy's quantity set names no width, so the same build-up on one
        // writes nothing.
        options.settings.length_unit = LengthUnit::Metre;
        model.elements[0].element_type = BimElementType::Unknown;
        model.elements[0].category = None;
        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("IFCQUANTITYLENGTH"), "{text}");
    }

    /// A shell that is not closed, or one with a face this cannot measure
    /// exactly, is left unmeasured rather than estimated.
    #[test]
    fn refuses_to_measure_a_body_it_cannot_measure_exactly() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        let mut measuring = options();
        measuring.settings.property_sets.base_quantities = true;

        for geometry in [
            BimGeometry::Brep(box_brep(false)),
            BimGeometry::Brep(quarter_disc_brep(true)),
        ] {
            model.elements[0].geometry = Some(geometry);
            let file = metadata_ifc(&model, &measuring).unwrap();
            let mut bytes = Vec::new();
            file.write_to(&mut bytes).unwrap();
            let text = String::from_utf8(bytes).unwrap();
            assert!(!text.contains("=IFCELEMENTQUANTITY("));
        }

        // And with the switch off, nothing is measured at all.
        model.elements[0].geometry = Some(BimGeometry::Brep(box_brep(true)));
        let mut unmeasuring = options();
        unmeasuring.settings.property_sets.base_quantities = false;
        let file = metadata_ifc(&model, &unmeasuring).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("=IFCELEMENTQUANTITY("));
    }

    /// What the setup says about the project reaches the file: the three
    /// spatial roots take the names it gives them, the project carries its
    /// long name and phase, and the building carries the postal address -
    /// which is where Revit's own export puts it too.
    #[test]
    fn the_project_settings_name_the_spatial_tree_and_address_the_building() {
        let mut with_project = options();
        with_project.settings.project = crate::ProjectSettings {
            name: Some("SRG-DP-RP".to_owned()),
            long_name: Some("Residential complex, phase 2".to_owned()),
            phase: Some("Detail design".to_owned()),
            site_name: Some("Plot 219B".to_owned()),
            building_name: Some("Section 1".to_owned()),
            address_lines: vec!["Raiymbek 219B".to_owned()],
            town: Some("Almaty".to_owned()),
            region: None,
            postal_code: Some("050000".to_owned()),
            country: Some("KZ".to_owned()),
        };

        let file = metadata_ifc(&model(), &with_project).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        let project = text
            .lines()
            .find(|line| line.contains("=IFCPROJECT("))
            .expect("the project");
        assert!(project.contains("'SRG-DP-RP'"), "{project}");
        assert!(
            project.contains("'Residential complex, phase 2'"),
            "{project}"
        );
        assert!(project.contains("'Detail design'"), "{project}");
        assert!(text.contains("=IFCSITE('") && text.contains("'Plot 219B'"));
        let building = text
            .lines()
            .find(|line| line.contains("=IFCBUILDING("))
            .expect("the building");
        assert!(building.contains("'Section 1'"), "{building}");
        let address = text
            .lines()
            .find(|line| line.contains("=IFCPOSTALADDRESS("))
            .expect("the address");
        assert!(address.contains("('Raiymbek 219B')"), "{address}");
        assert!(
            address.contains("'Almaty'") && address.contains("'050000'"),
            "{address}"
        );

        // With nothing given, the file is what it was before the setting
        // existed: the source's own stem, and no address at all.
        let file = metadata_ifc(&model(), &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("'Test Project'"));
        assert!(!text.contains("=IFCPOSTALADDRESS("));
    }

    /// `IfcSite.RefLatitude`/`RefLongitude`/`RefElevation` are written from
    /// `BimModel.site` when the model carries one, in IFC's own
    /// degrees-minutes-seconds-plus-fraction form. The degrees are AR S1's
    /// own `GeoSite` record, converted from the radians the file stores;
    /// `s1_revit.ifc`'s own `IfcSite` states the same location as
    /// `(42,24,53,508911)`/`(-71,-15,-29,-58837)`/`0.`, which this matches
    /// exactly - not just to within rounding.
    #[test]
    fn writes_the_sites_own_location_as_a_compound_angle() {
        let mut sited = model();
        sited.site = Some(bim_core::BimSiteLocation {
            latitude_degrees: 0.740_279_021_367_38 * (180.0 / std::f64::consts::PI),
            longitude_degrees: -1.243_687_973_267_624_5 * (180.0 / std::f64::consts::PI),
            elevation: Some(BimNumber {
                value: 0.0,
                unit: Some(bim_core::BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
            }),
        });

        let file = metadata_ifc(&sited, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        let site = text
            .lines()
            .find(|line| line.contains("=IFCSITE("))
            .expect("the site");
        assert!(
            site.contains("(42,24,53,508911)"),
            "latitude should match Revit's own export exactly: {site}"
        );
        assert!(
            site.contains("(-71,-15,-29,-58837)"),
            "longitude should match Revit's own export exactly: {site}"
        );
    }

    /// No `BimModel.site` - most files, which never touch
    /// *Manage > Location* or whose alternates disagree - leaves
    /// `RefLatitude`/`RefLongitude`/`RefElevation` unset, as before this
    /// feature existed.
    #[test]
    fn writes_no_site_location_where_the_model_carries_none() {
        let file = metadata_ifc(&model(), &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let site = text
            .lines()
            .find(|line| line.contains("=IFCSITE("))
            .expect("the site");
        assert!(site.ends_with(",$,$,$,$,$);"), "{site}");
    }

    /// A mapping table decides what a category becomes, ahead of the built-in
    /// mapping, and can keep one out of the file altogether - which is what
    /// Revit's own `Not Exported` does.
    #[test]
    fn a_class_mapping_table_decides_what_a_category_becomes() {
        let mut model = model();
        let mut beam = model.elements[0].clone();
        beam.id = BimElementId("201".to_owned());
        beam.type_id = Some(BimElementId("900".to_owned()));
        beam.category = Some(BimCategory {
            id: Some(BimExternalId {
                system: "revit.builtincategory".to_owned(),
                value: "-2001320".to_owned(),
            }),
            name: "OST_StructuralFraming".to_owned(),
        });
        let mut hidden = beam.clone();
        hidden.id = BimElementId("202".to_owned());
        hidden.category = Some(BimCategory {
            id: None,
            name: "OST_GenericModel".to_owned(),
        });
        model.elements = vec![beam, hidden];

        let mut options = options();
        options.settings.set_class_mapping(
            ClassMapping::from_table(
                "OST_StructuralFraming\t\tIfcBeam\tJOIST\nOST_GenericModel\t\tNot Exported\t",
            )
            .expect("a readable table"),
        );

        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        // The mapped category is written as what the table says, with the kind
        // the table gives it in the slot the schema declares for it.
        let beam = text
            .lines()
            .find(|line| line.contains("=IFCBEAM("))
            .expect("the mapped beam");
        assert!(beam.ends_with(".JOIST.);"), "{beam}");
        // Its type follows the entity it was written as.
        assert!(text.contains("=IFCBEAMTYPE("));
        // Without the table both would have been proxies; with it, one is a
        // beam and the other is not in the file at all.
        assert!(!text.contains("=IFCBUILDINGELEMENTPROXY("));
        assert_eq!(
            text.matches("=IFCRELCONTAINEDINSPATIALSTRUCTURE(").count(),
            1
        );
        let containment = text
            .lines()
            .find(|line| line.contains("=IFCRELCONTAINEDINSPATIALSTRUCTURE("))
            .expect("the containment");
        // One product in it - the beam - beside its own reference, the owner
        // and the storey that contains it.
        assert_eq!(containment.matches('#').count(), 4, "{containment}");
    }

    /// Every entity this exporter can name is in the schema table, so no
    /// element is ever written with its own attributes silently left off.
    /// `IfcSpace` is the exception the writer already knows about: it is a
    /// spatial element rather than an element, and `push_space` writes its
    /// attributes itself.
    #[test]
    fn every_entity_this_exporter_writes_is_in_the_schema_table() {
        for element_type in [
            BimElementType::PipeSegment,
            BimElementType::PipeFitting,
            BimElementType::SanitaryTerminal,
            BimElementType::AirTerminal,
            BimElementType::FireSuppressionTerminal,
            BimElementType::Alarm,
            BimElementType::CableCarrierFitting,
            BimElementType::DuctSegment,
            BimElementType::CableCarrierSegment,
            BimElementType::DistributionElement,
            BimElementType::DistributionFlowElement,
            BimElementType::Wall,
            BimElementType::Slab,
            BimElementType::Roof,
            BimElementType::Stair,
            BimElementType::StairFlight,
            BimElementType::CurtainWall,
            BimElementType::Railing,
            BimElementType::Column,
            BimElementType::Member,
            BimElementType::Plate,
            BimElementType::Window,
            BimElementType::Door,
            BimElementType::Unknown,
        ] {
            let name = ifc_entity_name(element_type);
            assert!(
                IFC4_ELEMENTS.iter().any(|entity| entity.name == name),
                "{name} is not in the IFC4 element table"
            );
            if let Some((type_name, table)) = type_entity_for(name) {
                assert!(
                    table.iter().any(|entity| entity.name == type_name),
                    "{type_name} is not in its schema table"
                );
            }
        }
        assert_eq!(ifc_entity_name(BimElementType::Space), "IFCSPACE");
        let (space_type, table) = type_entity_for("IFCSPACE").expect("a space has a type entity");
        assert!(table.iter().any(|entity| entity.name == space_type));
    }

    /// IFC's own set, written once per type and related to every element of
    /// it. `Reference` is the element's type name, which is what Revit's own
    /// export of AR S1 puts there for 11 545 of its 11 895 products.
    #[test]
    fn writes_the_ifc_common_set_once_per_type_with_the_reference_revit_writes() {
        let mut model = model();
        let mut first = model.elements[0].clone();
        first.element_type = BimElementType::Wall;
        first.type_id = Some(BimElementId("900".to_owned()));
        first.type_name = Some("Finish: concrete t=10".to_owned());
        let mut second = first.clone();
        second.id = BimElementId("201".to_owned());
        // A third element of another type, and a fourth with no type name at
        // all, which carries no set rather than an empty one.
        let mut third = first.clone();
        third.id = BimElementId("202".to_owned());
        third.element_type = BimElementType::Slab;
        third.type_name = Some("Floor: floating t=90".to_owned());
        let mut fourth = first.clone();
        fourth.id = BimElementId("203".to_owned());
        fourth.type_name = None;
        model.elements = vec![first, second, third, fourth];

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(text.matches("'Pset_WallCommon'").count(), 1);
        assert_eq!(text.matches("'Pset_SlabCommon'").count(), 1);
        let wall_set = text
            .lines()
            .find(|line| line.contains("'Pset_WallCommon'"))
            .expect("the wall's common set");
        // One property in it, and it is the reference.
        assert_eq!(wall_set.matches('#').count(), 3, "{wall_set}");
        assert!(text.contains("'Reference',$,IFCIDENTIFIER('Finish: concrete t=10')"));

        // The set is related once, to both walls of that type.
        let relation = text
            .lines()
            .filter(|line| line.contains("=IFCRELDEFINESBYPROPERTIES("))
            .find(|line| {
                line.contains(&format!(
                    "#{}",
                    wall_set
                        .split('=')
                        .next()
                        .unwrap_or_default()
                        .trim_start_matches('#')
                ))
            })
            .map(str::to_owned);
        assert!(relation.is_some(), "the common set should be related");

        // Turning it off leaves nothing of it behind.
        let mut options = options();
        options.settings.property_sets.ifc_common = false;
        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("Pset_WallCommon"));
    }

    /// `IfcDistributionFlowElementType` is ABSTRACT in IFC4, so an element
    /// typed as that supertype gets no type object at all and keeps its type's
    /// parameters on itself. `ifcopenshell.validate` refused 26 of them on
    /// SMALL before this; the schema says the same thing.
    #[test]
    fn refuses_to_write_a_type_the_schema_declares_abstract() {
        let mut model = model();
        let mut element = model.elements[0].clone();
        element.element_type = BimElementType::DistributionFlowElement;
        element.category = None;
        element.type_id = Some(BimElementId("900".to_owned()));
        element.type_properties = vec![BimProperty {
            id: None,
            name: "Manufacturer".to_owned(),
            specification: None,
            value: BimPropertyValue::Text("SANEXT".to_owned()),
        }];
        model.elements = vec![element];

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("=IFCDISTRIBUTIONFLOWELEMENT("));
        assert!(!text.contains("=IFCDISTRIBUTIONFLOWELEMENTTYPE("));
        assert!(!text.contains("=IFCRELDEFINESBYTYPE("));
        // Nothing holds the type's parameters now, so the element does.
        assert!(text.contains("'Rivet Type Properties'"));

        // The concrete sibling is written.
        model.elements[0].element_type = BimElementType::DistributionElement;
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("=IFCDISTRIBUTIONELEMENTTYPE("));
    }

    /// With types off, nothing is written that a reader would have to know
    /// about, and the type's parameters go back onto the element itself.
    #[test]
    fn types_can_be_left_out_and_the_parameters_return_to_the_element() {
        let mut model = model();
        let mut element = model.elements[0].clone();
        element.element_type = BimElementType::Wall;
        element.type_id = Some(BimElementId("900".to_owned()));
        element.type_properties = vec![BimProperty {
            id: None,
            name: "Manufacturer".to_owned(),
            specification: None,
            value: BimPropertyValue::Text("SANEXT".to_owned()),
        }];
        model.elements = vec![element];
        let mut options = options();
        options.settings.types = false;

        let file = metadata_ifc(&model, &options).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(!text.contains("=IFCWALLTYPE("));
        assert!(!text.contains("=IFCRELDEFINESBYTYPE("));
        assert!(text.contains("'Rivet Type Properties'"));
        assert_eq!(text.matches("=IFCRELDEFINESBYPROPERTIES(").count(), 2);
    }

    #[test]
    fn qualifies_property_names_that_collide_within_one_set() {
        let mut model = model();
        let mut element = model.elements[0].clone();
        // Two distinct Revit built-ins that share the display name
        // "Description", plus one name that does not collide.
        element.properties = vec![
            BimProperty {
                id: Some(BimExternalId {
                    system: "autodesk.revit.builtInParameter".to_owned(),
                    value: "-1010103".to_owned(),
                }),
                name: "Description".to_owned(),
                specification: None,
                value: BimPropertyValue::Text("\u{412}25".to_owned()),
            },
            BimProperty {
                id: Some(BimExternalId {
                    system: "autodesk.revit.builtInParameter".to_owned(),
                    value: "-1150481".to_owned(),
                }),
                name: "Description".to_owned(),
                specification: None,
                value: BimPropertyValue::Text(String::new()),
            },
            BimProperty {
                id: None,
                name: "Manufacturer".to_owned(),
                specification: None,
                value: BimPropertyValue::Text("SANEXT".to_owned()),
            },
        ];
        model.elements = vec![element];
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        // Both colliding parameters survive under names that tell them apart,
        // and neither keeps the bare colliding name.
        assert!(text.contains("'Description [-1010103]'"));
        assert!(text.contains("'Description [-1150481]'"));
        assert!(!text.contains("('Description',"));
        // A name that does not collide is left exactly as declared.
        assert!(text.contains("('Manufacturer',"));
    }

    #[test]
    fn keeps_colliding_property_names_unique_without_an_identifier() {
        // Nothing distinguishes these but their order, so the occurrence
        // suffix is what keeps the set schema-valid.
        let source = vec![
            BimProperty {
                id: None,
                name: "Calculation Rules".to_owned(),
                specification: None,
                value: BimPropertyValue::Integer(1),
            },
            BimProperty {
                id: None,
                name: "Calculation Rules".to_owned(),
                specification: None,
                value: BimPropertyValue::Integer(2),
            },
        ];
        let names = unique_property_names(&source);
        assert_eq!(names, ["Calculation Rules", "Calculation Rules (2)"]);
        assert_ne!(names[0], names[1]);
    }

    #[test]
    fn writes_a_stair_flight_with_its_predefined_type_in_the_right_slot() {
        // IfcStairFlight declares NumberOfRisers, NumberOfTreads, RiserHeight
        // and TreadLength between IfcElement's eight attributes and its
        // PredefinedType. Without those four placeholders the enumeration
        // lands in TreadLength and the file fails schema validation.
        let mut model = model();
        let mut element = model.elements[0].clone();
        element.element_type = BimElementType::StairFlight;
        model.elements = vec![element];
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        let line = text
            .lines()
            .find(|line| line.contains("=IFCSTAIRFLIGHT("))
            .expect("no stair flight written");
        assert!(line.ends_with("$,$,$,$,.NOTDEFINED.);"), "{line}");
        // A wall keeps the plain shape: PredefinedType straight after Tag.
        model.elements[0].element_type = BimElementType::Wall;
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains("=IFCWALL("))
            .expect("no wall written");
        assert!(line.ends_with(",.NOTDEFINED.);"), "{line}");
        assert!(!line.contains("$,$,$,$,.NOTDEFINED."), "{line}");
    }

    #[test]
    fn dispatches_supported_elements_and_keeps_an_unknown_proxy() {
        let mut model = model();
        model.elements = vec![
            element("201", "RbsPipeCurve", "OST_PipeCurves"),
            element("202", "FamilyInstance", "OST_PipeFitting"),
            element("203", "FamilyInstance", "OST_PlumbingFixtures"),
            element("204", "FamilyInstance", "OST_DuctTerminal"),
            element("205", "FamilyInstance", "OST_Sprinklers"),
            element("206", "Wall", "OST_Walls"),
        ];

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for entity in [
            "IFCPIPESEGMENT",
            "IFCPIPEFITTING",
            "IFCSANITARYTERMINAL",
            "IFCAIRTERMINAL",
            "IFCFIRESUPPRESSIONTERMINAL",
            "IFCBUILDINGELEMENTPROXY",
        ] {
            assert_eq!(
                text.matches(&format!("={entity}(")).count(),
                1,
                "unexpected count for {entity}"
            );
        }
    }

    #[test]
    fn honors_a_preclassified_format_neutral_type() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::AirTerminal;

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("=IFCAIRTERMINAL("));
        assert!(!text.contains("=IFCBUILDINGELEMENTPROXY("));
    }

    #[test]
    fn writes_a_metric_swept_disk_relative_to_its_storey() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::PipeSegment;
        model.elements[0].geometry = Some(BimGeometry::SweptDisk(BimSweptDisk {
            directrix: BimLineSegment {
                start: BimPoint3 {
                    coordinates: [1.0, 2.0, 3.5],
                    unit: BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters"),
                },
                end: BimPoint3 {
                    coordinates: [1.0, 2.0, 4.5],
                    unit: BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters"),
                },
            },
            radius: BimNumber {
                value: 0.01,
                unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
            },
        }));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for entity in [
            "IFCPOLYLINE",
            "IFCSWEPTDISKSOLID",
            "IFCSHAPEREPRESENTATION",
            "IFCPRODUCTDEFINITIONSHAPE",
            "IFCPIPESEGMENT",
        ] {
            assert!(text.contains(&format!("={entity}(")), "missing {entity}");
        }
        assert!(text.contains("'Body','AdvancedSweptSolid'"));
        // World Z=3.5 m is represented 0.452 m above the 3.048 m storey.
        let local_start = format!("({},{},{})", real(1.0), real(2.0), real(3.5 - 3.048));
        assert!(text.contains(&local_start));
    }

    #[test]
    fn writes_an_axis_only_fitting_without_inventing_a_body() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::PipeFitting;
        model.elements[0].placement = Some(BimPlacement {
            origin: BimPoint3 {
                coordinates: [1.0, 2.0, 3.0],
                unit: BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters"),
            },
            reference_direction: [1.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
        });
        model.elements[0].geometry = Some(BimGeometry::AxisLine(BimLineSegment {
            start: BimPoint3 {
                coordinates: [1.0, 2.0, 3.5],
                unit: BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters"),
            },
            end: BimPoint3 {
                coordinates: [1.0, 2.0, 4.5],
                unit: BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters"),
            },
        }));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("=IFCPIPEFITTING("));
        assert!(text.contains("'Axis','Curve3D'"));
        assert!(text.contains("=IFCPRODUCTDEFINITIONSHAPE("));
        assert!(!text.contains("=IFCSWEPTDISKSOLID("));
        assert!(!text.contains("'Body'"));
        let relative_origin = format!("({},{},{})", real(1.0), real(2.0), real(3.0 - 3.048));
        assert!(text.contains(&relative_origin));
        assert!(text.contains("(0.,0.,0.5)"));
    }

    #[test]
    fn writes_verified_extents_as_a_box_not_a_body() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::SanitaryTerminal;
        model.elements[0].placement = Some(BimPlacement {
            origin: BimPoint3 {
                coordinates: [10.0, 20.0, 30.0],
                unit: BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters"),
            },
            reference_direction: [1.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
        });
        // The box is carried in the model's project coordinates, like every
        // other geometry, and comes out in the product's own frame.
        model.elements[0].geometry = Some(BimGeometry::BoundingBox(BimBoundingBox {
            min: metres_point([9.9, 19.8, 29.7]),
            max: metres_point([10.1, 20.2, 30.3]),
        }));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("=IFCSANITARYTERMINAL("));
        assert!(text.contains("=IFCBOUNDINGBOX("));
        assert!(text.contains("'Box','BoundingBox'"));
        assert!(!text.contains("'Body'"));
        assert!(text.contains("(-0.1,-0.2,-0.3)"));
        assert!(text.contains(",0.2,0.4,0.6)"));
    }

    #[test]
    fn a_box_takes_its_storey_elevation_off_once() {
        // An element with no placement of its own hangs off the storey, whose
        // own placement carries the elevation. A box corner that still holds
        // that elevation is the elevation twice, which is what put AR S1's
        // slabs, roof and curtain walls at twice their height.
        let mut model = model();
        model.elements[0].element_type = BimElementType::Slab;
        model.elements[0].placement = None;
        model.elements[0].geometry = Some(BimGeometry::BoundingBox(BimBoundingBox {
            min: metres_point([1.0, 2.0, 3.048]),
            max: metres_point([1.2, 2.4, 3.148]),
        }));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("=IFCBOUNDINGBOX("));
        assert!(text.contains("(1.,2.,0.)"));
        assert!(!text.contains("(1.,2.,3.048)"));
    }

    fn metres_point(coordinates: [f64; 3]) -> BimPoint3 {
        BimPoint3 {
            coordinates,
            unit: BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters"),
        }
    }

    fn metres_number(value: f64) -> BimNumber {
        BimNumber {
            value,
            unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
        }
    }

    /// A quarter-disc: one planar face bounded by a quarter-circle arc and
    /// two straight radii, at the model's storey elevation so the local
    /// coordinates equal the world ones.
    fn quarter_disc_brep(complete: bool) -> BimBrep {
        let arc = BimBrepEdge {
            start: metres_point([2.0, 0.0, 3.048]),
            end: metres_point([0.0, 2.0, 3.048]),
            curve: BimBrepCurve::Arc(Box::new(bim_core::BimBrepArc {
                center: metres_point([0.0, 0.0, 3.048]),
                x_axis: [1.0, 0.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                radius: metres_number(2.0),
                start_angle: 0.0,
                end_angle: std::f64::consts::FRAC_PI_2,
            })),
        };
        let radius_a = BimBrepEdge {
            start: metres_point([0.0, 2.0, 3.048]),
            end: metres_point([0.0, 0.0, 3.048]),
            curve: BimBrepCurve::Line,
        };
        let radius_b = BimBrepEdge {
            start: metres_point([0.0, 0.0, 3.048]),
            end: metres_point([2.0, 0.0, 3.048]),
            curve: BimBrepCurve::Line,
        };
        BimBrep {
            faces: vec![BimBrepFace {
                surface: BimBrepSurface::Plane {
                    origin: metres_point([0.0, 0.0, 3.048]),
                    x_axis: [1.0, 0.0, 0.0],
                    y_axis: [0.0, 1.0, 0.0],
                },
                loops: vec![vec![arc, radius_a, radius_b]],
                material: None,
            }],
            complete,
        }
    }

    /// A box is a sweep of its own footprint, and is written as one: the
    /// profile states in four points what the shell states in six faces, and
    /// the solid is the same solid.
    #[test]
    fn writes_a_box_as_the_sweep_of_its_footprint() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].geometry = Some(BimGeometry::Brep(box_brep(true)));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("'Body','SweptSolid'"), "{text}");
        assert!(text.contains("=IFCARBITRARYCLOSEDPROFILEDEF(.AREA.,$,"));
        assert!(
            !text.contains("=IFCADVANCEDBREP(") && !text.contains("=IFCADVANCEDFACE("),
            "the shell is not written beside the sweep"
        );
        // 1 x 2 x 3 metres, standing on its footprint: the depth is the
        // height, and the profile is the metre-by-two-metre base.
        let solid = text
            .lines()
            .find(|line| line.contains("=IFCEXTRUDEDAREASOLID("))
            .expect("a swept solid");
        assert!(solid.ends_with(",3.);"), "{solid}");
        let profile = text
            .lines()
            .find(|line| line.contains("=IFCPOLYLINE("))
            .expect("a profile curve");
        // Four corners, and the first stated again to close the curve.
        assert_eq!(profile.matches('#').count() - 1, 5, "{profile}");
    }

    /// The one thing a sweep may not do is stand in for a shell that is not
    /// closed. An incomplete box is the same six faces with one of them
    /// missing, as far as anything downstream can tell.
    #[test]
    fn writes_an_incomplete_box_as_the_open_shell_it_is() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].geometry = Some(BimGeometry::Brep(box_brep(false)));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("=IFCEXTRUDEDAREASOLID("), "{text}");
        assert!(text.contains("'Body','SurfaceModel'"));
    }

    /// Every item under one `RepresentationType` has to be what the type says.
    /// Two boxes are two sweeps; a box beside something that is not one keeps
    /// the pair on the boundary-representation path rather than mixing them.
    #[test]
    fn writes_an_assembly_of_boxes_as_sweeps_and_a_mixed_one_as_shells() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::Wall;
        model.elements[0].geometry =
            Some(BimGeometry::Assembly(vec![box_brep(true), box_brep(true)]));
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("'Body','SweptSolid'"), "{text}");
        assert_eq!(text.matches("=IFCEXTRUDEDAREASOLID(").count(), 2);

        model.elements[0].geometry = Some(BimGeometry::Assembly(vec![
            box_brep(true),
            quarter_disc_brep(true),
        ]));
        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("=IFCEXTRUDEDAREASOLID("), "{text}");
        assert!(text.contains("'Body','AdvancedBrep'"));
    }

    #[test]
    fn writes_a_complete_brep_as_an_advanced_brep() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::SanitaryTerminal;
        model.elements[0].geometry = Some(BimGeometry::Brep(quarter_disc_brep(true)));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for entity in [
            "IFCADVANCEDBREP",
            "IFCCLOSEDSHELL",
            "IFCADVANCEDFACE",
            "IFCPLANE",
            "IFCCIRCLE",
            "IFCLINE",
            "IFCEDGECURVE",
            "IFCORIENTEDEDGE",
            "IFCEDGELOOP",
            "IFCFACEOUTERBOUND",
            "IFCVERTEXPOINT",
        ] {
            assert!(text.contains(&format!("={entity}(")), "missing {entity}");
        }
        assert!(!text.contains("=IFCOPENSHELL("));
        assert!(!text.contains("=IFCSHELLBASEDSURFACEMODEL("));
        assert!(text.contains("'Body','AdvancedBrep'"));
        // World Z=3.048 m sits exactly at the storey elevation.
        assert!(text.contains("(2.,0.,0.)"));
    }

    #[test]
    fn writes_a_sampled_brep_edge_as_an_ifc_polyline() {
        let mut brep = quarter_disc_brep(true);
        brep.faces[0].loops[0][0].curve = BimBrepCurve::Polyline(Box::new([
            metres_point([2.0, 0.0, 3.048]),
            metres_point([1.4, 1.4, 3.048]),
            metres_point([0.0, 2.0, 3.048]),
        ]));
        let mut model = model();
        model.elements[0].element_type = BimElementType::SanitaryTerminal;
        model.elements[0].geometry = Some(BimGeometry::Brep(brep));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("=IFCPOLYLINE("), "{text}");
        assert!(text.contains("(1.4,1.4,0.)"), "{text}");
        assert!(text.contains("=IFCADVANCEDBREP("), "{text}");
    }

    #[test]
    fn writes_an_incomplete_brep_as_an_open_shell() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::SanitaryTerminal;
        model.elements[0].geometry = Some(BimGeometry::Brep(quarter_disc_brep(false)));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("=IFCOPENSHELL("));
        assert!(text.contains("=IFCSHELLBASEDSURFACEMODEL("));
        assert!(!text.contains("=IFCADVANCEDBREP("));
        assert!(!text.contains("=IFCCLOSEDSHELL("));
        assert!(text.contains("'Body','SurfaceModel'"));
    }

    #[test]
    fn omits_geometry_for_a_brep_with_no_resolved_faces() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::SanitaryTerminal;
        model.elements[0].geometry = Some(BimGeometry::Brep(BimBrep {
            faces: Vec::new(),
            complete: false,
        }));

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("=IFCADVANCEDBREP("));
        assert!(!text.contains("=IFCSHELLBASEDSURFACEMODEL("));
    }

    /// One revolved face's surface, as the STEP text of the entity it became.
    /// The face's boundary is a straight edge between two heights on the
    /// profile, which is all a cone needs to bound its sweep and which every
    /// other surface here ignores. World coordinates throughout: no storey
    /// elevation, no placement.
    fn revolved_surface_text(profile: BimBrepProfile, heights: (f64, f64)) -> String {
        let mut file = StepFile::new(step_header(&options()));
        let face = BimBrepFace {
            surface: BimBrepSurface::Revolution {
                center: metres_point([0.0, 0.0, 0.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                profile: Box::new(profile),
            },
            loops: vec![vec![BimBrepEdge {
                start: metres_point([1.0, 0.0, heights.0]),
                end: metres_point([1.0, 0.0, heights.1]),
                curve: BimBrepCurve::Line,
            }]],
            material: None,
        };
        push_brep_surface(&mut file, &face, test_frame()).expect("the surface was written");
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn writes_a_revolved_line_as_the_cone_it_sweeps() {
        // The line `[1,0,0] + v * [1,0,1]`: radius 1 where it crosses z = 0,
        // opening upwards at 45 degrees, on a face reaching from z = 0 to
        // z = 2 - so radius 1 to radius 3, and a 5% margin past each.
        let text = revolved_surface_text(
            BimBrepProfile::Line {
                origin: metres_point([1.0, 0.0, 0.0]),
                direction: [1.0, 0.0, 1.0],
            },
            (0.0, 2.0),
        );
        assert!(text.contains("=IFCSURFACEOFREVOLUTION("), "{text}");
        assert!(
            text.contains("=IFCARBITRARYOPENPROFILEDEF(.CURVE.,$,"),
            "{text}"
        );
        assert!(text.contains("=IFCAXIS1PLACEMENT("), "{text}");
        // The profile's two ends, in the profile plane: (radius, height).
        assert!(text.contains("((0.9,-0.1))"), "{text}");
        assert!(text.contains("((3.1,2.1))"), "{text}");
        // IFC4 has no conical surface at all.
    }

    #[test]
    fn states_the_revolution_axis_in_the_surfaces_own_position() {
        // A cone whose centre is off the element's origin and whose axis is
        // not the element's `y`: the two conditions under which the element's
        // coordinates and `Position`'s stop agreeing. The axis must still be
        // `Position`'s `y` through its origin, and the profile must be two
        // edges - see `push_revolution_axis`.
        let mut file = StepFile::new(step_header(&options()));
        let face = BimBrepFace {
            surface: BimBrepSurface::Revolution {
                center: metres_point([5.0, 6.0, 7.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                profile: Box::new(BimBrepProfile::Line {
                    origin: metres_point([1.0, 0.0, 0.0]),
                    direction: [1.0, 0.0, 1.0],
                }),
            },
            loops: vec![vec![BimBrepEdge {
                start: metres_point([6.0, 6.0, 7.0]),
                end: metres_point([8.0, 6.0, 9.0]),
                curve: BimBrepCurve::Line,
            }]],
            material: None,
        };
        push_brep_surface(&mut file, &face, test_frame()).expect("the surface was written");
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let entities = text
            .lines()
            .filter_map(|line| {
                let (id, body) = line.strip_prefix('#')?.split_once('=')?;
                Some((format!("#{id}"), body.trim_end_matches(';').to_owned()))
            })
            .collect::<BTreeMap<_, _>>();
        let arguments = |body: &str, entity: &str| {
            body.strip_prefix(entity)
                .and_then(|rest| rest.strip_prefix('('))
                .and_then(|rest| rest.strip_suffix(')'))
                .map(|rest| rest.split(',').map(str::to_owned).collect::<Vec<_>>())
        };
        let surface = entities
            .values()
            .find_map(|body| arguments(body, "IFCSURFACEOFREVOLUTION"))
            .expect("a surface of revolution");
        let axis = arguments(&entities[&surface[2]], "IFCAXIS1PLACEMENT").expect("an axis");
        assert_eq!(entities[&axis[0]], "IFCCARTESIANPOINT((0.,0.,0.))");
        assert_eq!(entities[&axis[1]], "IFCDIRECTION((0.,1.,0.))");
        // The placement carries the centre, so the axis need not.
        assert!(text.contains("=IFCCARTESIANPOINT((5.,6.,7.))"), "{text}");
        let profile =
            arguments(&entities[&surface[0]], "IFCARBITRARYOPENPROFILEDEF").expect("a profile");
        let polyline = entities[&profile[2]].clone();
        assert_eq!(polyline.matches('#').count(), 3, "{polyline}");
    }

    #[test]
    fn reads_a_cone_declared_from_its_own_apex() {
        // The profile line starts on the axis: the point names no direction
        // out of it, so the line's own does, and this is an ordinary cone
        // whose apex is where the profile begins.
        let text = revolved_surface_text(
            BimBrepProfile::Line {
                origin: metres_point([0.0, 0.0, 0.0]),
                direction: [1.0, 0.0, 1.0],
            },
            (0.0, 2.0),
        );
        assert!(text.contains("=IFCSURFACEOFREVOLUTION("), "{text}");
        // The apex, and the far end a 5% margin past the face's reach.
        assert!(text.contains("((0.,0.))"), "{text}");
        assert!(text.contains("((2.1,2.1))"), "{text}");
    }

    #[test]
    fn stops_a_cone_at_the_apex_rather_than_past_it() {
        // The same cone on a face that reaches below the apex at z = -1.
        // Sweeping the profile past the axis would double the cone back on
        // itself, so the profile stops on the point.
        let text = revolved_surface_text(
            BimBrepProfile::Line {
                origin: metres_point([1.0, 0.0, 0.0]),
                direction: [1.0, 0.0, 1.0],
            },
            (-3.0, 2.0),
        );
        assert!(text.contains("=IFCSURFACEOFREVOLUTION("), "{text}");
        assert!(text.contains("((0.,-1.))"), "{text}");
    }

    #[test]
    fn writes_a_line_parallel_to_the_axis_as_a_cylinder() {
        // Revolving a line that never changes its distance from the axis
        // sweeps a cylinder, which IFC4 has as an elementary surface.
        let text = revolved_surface_text(
            BimBrepProfile::Line {
                origin: metres_point([1.5, 0.0, 0.0]),
                direction: [0.0, 0.0, 1.0],
            },
            (0.0, 1.0),
        );
        assert!(text.contains("=IFCCYLINDRICALSURFACE("), "{text}");
        assert!(text.contains("1.5"), "{text}");
    }

    #[test]
    fn writes_a_line_square_to_the_axis_as_a_plane() {
        // A line that only moves outwards sweeps the flat annulus its own
        // plane already describes.
        let text = revolved_surface_text(
            BimBrepProfile::Line {
                origin: metres_point([1.0, 0.0, 4.0]),
                direction: [1.0, 0.0, 0.0],
            },
            (4.0, 4.0),
        );
        assert!(text.contains("=IFCPLANE("), "{text}");
        assert!(!text.contains("=IFCSURFACEOFREVOLUTION("));
        // The plane sits at the height the line runs at, not at the frame's
        // own origin.
        assert!(text.contains("0.,0.,4."));
    }

    #[test]
    fn writes_an_off_axis_arc_as_a_torus() {
        let text = revolved_surface_text(
            BimBrepProfile::Arc {
                center: metres_point([2.0, 0.0, 0.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 0.0, 1.0],
                radius: metres_number(0.5),
            },
            (-0.5, 0.5),
        );
        assert!(text.contains("=IFCTOROIDALSURFACE("), "{text}");
        // Major radius then minor, in that order.
        assert!(text.contains("2.,0.5);"), "{text}");
    }

    #[test]
    fn writes_a_torus_with_no_hole_left_as_a_revolved_circle() {
        // Minor radius equal to major: the profile circle touches the axis and
        // the hole has closed, which `IfcToroidalSurface` may not hold.
        let text = revolved_surface_text(
            BimBrepProfile::Arc {
                center: metres_point([0.5, 0.0, 0.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 0.0, 1.0],
                radius: metres_number(0.5),
            },
            (-0.5, 0.5),
        );
        assert!(text.contains("=IFCSURFACEOFREVOLUTION("), "{text}");
        assert!(text.contains("=IFCTRIMMEDCURVE("), "{text}");
        assert!(!text.contains("=IFCTOROIDALSURFACE("));
        // The profile circle, in the plane that holds the axis: centred a
        // major radius out, and swept a whole turn in radians.
        assert!(text.contains("((0.5,0.))"), "{text}");
        assert!(
            text.contains(&format!(
                "IFCPARAMETERVALUE({})),.T.,.PARAMETER.);",
                real(std::f64::consts::TAU)
            )),
            "{text}"
        );
    }

    #[test]
    fn writes_an_arc_centred_on_the_axis_as_a_sphere() {
        let text = revolved_surface_text(
            BimBrepProfile::Arc {
                center: metres_point([0.0, 0.0, 1.0]),
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 0.0, 1.0],
                radius: metres_number(0.75),
            },
            (0.25, 1.75),
        );
        assert!(text.contains("=IFCSPHERICALSURFACE("), "{text}");
        assert!(text.contains("0.75);"), "{text}");
        assert!(!text.contains("=IFCTOROIDALSURFACE("));
    }

    #[test]
    fn writes_a_distribution_supertype_without_a_predefined_type() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::DistributionElement;

        let file = metadata_ifc(&model, &options()).unwrap();
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains("=IFCDISTRIBUTIONELEMENT("))
            .expect("the mapped supertype was not written");
        // `IfcDistributionElement` stops above the typed leaves and declares no
        // `PredefinedType`, so the trailing enumeration must not be written.
        assert!(!line.contains("NOTDEFINED"), "{line}");
        assert!(!text.contains("=IFCBUILDINGELEMENTPROXY("));
    }

    #[test]
    fn refuses_to_invent_a_storey_elevation() {
        let mut model = model();
        model.levels[0].elevation = None;
        assert!(matches!(
            metadata_ifc(&model, &options()),
            Err(MetadataError::MissingLevelElevation(_))
        ));
    }

    #[test]
    fn refuses_to_treat_an_unknown_elevation_unit_as_metres() {
        let mut model = model();
        model.levels[0].elevation.as_mut().unwrap().unit = None;
        assert!(matches!(
            metadata_ifc(&model, &options()),
            Err(MetadataError::UnsupportedLevelUnit { .. })
        ));
    }

    #[test]
    fn refuses_duplicate_source_identifiers() {
        let mut duplicate_elements = model();
        duplicate_elements
            .elements
            .push(duplicate_elements.elements[0].clone());
        assert!(matches!(
            metadata_ifc(&duplicate_elements, &options()),
            Err(MetadataError::DuplicateElementId(_))
        ));

        let mut duplicate_levels = model();
        duplicate_levels
            .levels
            .push(duplicate_levels.levels[0].clone());
        assert!(matches!(
            metadata_ifc(&duplicate_levels, &options()),
            Err(MetadataError::DuplicateLevelId(_))
        ));
    }
}
