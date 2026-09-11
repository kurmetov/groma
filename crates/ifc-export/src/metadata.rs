use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use bim_convert::{ifc_entity_name, resolved_element_type};
use bim_core::{
    BimBoundingBox, BimBrep, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepProfile,
    BimBrepSurface, BimElement, BimElementId, BimExternalId, BimGeometry, BimLevel, BimLineSegment,
    BimMaterialLayer, BimModel, BimNumber, BimPlacement, BimPoint3, BimProperty, BimPropertyValue,
};

use crate::{
    ClassMapping, EntityRef, ExportSettings, IfcGuid, LengthUnit, Mapped, StepFile, StepHeader,
    StepValue,
    extrusion::{self, NotAPrism, SolidReport},
    ifc4_entities::{
        Attribute, IFC4_BASE_QUANTITY_SETS, IFC4_COMMON_PROPERTY_SETS, IFC4_ELEMENT_TYPES,
        IFC4_ELEMENTS, IFC4_SPATIAL_ELEMENT_TYPES, Ifc4Entity,
    },
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
    let site = push_site(&mut file, options, ownership, site_placement);
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
        building,
        building_placement,
        &storeys,
        &placements,
        &elevations,
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

fn step_header(options: &MetadataOptions) -> StepHeader {
    StepHeader {
        description: vec![format!(
            "ViewDefinition [{}]",
            options.settings.view_definition.view_definition()
        )],
        file_name: options.file_name.clone(),
        timestamp: options.timestamp.clone(),
        authors: vec!["Rivet".to_owned()],
        organizations: vec!["Rivet".to_owned()],
        preprocessor_version: format!("Rivet {}", env!("CARGO_PKG_VERSION")),
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
        let unit = file.push(
            "IFCSIUNIT",
            vec![
                StepValue::Derived,
                enumeration(unit_type),
                prefix.map_or_else(omitted, enumeration),
                enumeration(name),
            ],
        );
        units.push(reference(unit));
    }
    file.push("IFCUNITASSIGNMENT", vec![StepValue::List(units)])
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
) -> EntityRef {
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
            omitted(),
            omitted(),
            omitted(),
            omitted(),
            omitted(),
        ],
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

#[allow(clippy::too_many_arguments)]
fn push_elements(
    file: &mut StepFile,
    elements: &[BimElement],
    report: &mut SolidReport,
    context: WriteContext<'_>,
    building: EntityRef,
    building_placement: EntityRef,
    storeys: &BTreeMap<BimElementId, EntityRef>,
    storey_placements: &BTreeMap<BimElementId, EntityRef>,
    storey_elevations: &BTreeMap<BimElementId, f64>,
    representation_context: EntityRef,
    origin_axis: EntityRef,
) {
    let mut materials = MaterialLibrary::default();
    let mut types = TypeLibrary::default();
    let mut common_sets = CommonPropertySets::default();
    let mut containment: BTreeMap<String, (EntityRef, Vec<EntityRef>)> = BTreeMap::new();
    // A space is part of the spatial structure, so its storey decomposes it
    // rather than containing it. Kept apart from the first pass so the two
    // relationships never carry the same product.
    let mut decomposition: BTreeMap<String, (EntityRef, Vec<EntityRef>)> = BTreeMap::new();
    for element in elements {
        // What the element is written as is settled before anything is
        // written: a category the mapping table keeps out of the file leaves
        // nothing behind it - no product, no placement, no property set and no
        // type - and deciding afterwards would leave the placement stranded.
        let Some(written_as) = resolve_entity(element, context.options.settings.class_mapping())
        else {
            continue;
        };
        let (container, container_identity) = element
            .level_id
            .as_ref()
            .and_then(|id| Some((storeys.get(id).copied()?, format!("storey:{}", id.0))))
            .unwrap_or_else(|| (building, "building".to_owned()));
        let frame = GeometryFrame {
            lengths: context.lengths,
            storey_elevation: element
                .level_id
                .as_ref()
                .and_then(|id| storey_elevations.get(id).copied())
                .unwrap_or(0.0),
            placement: element
                .placement
                .as_ref()
                .and_then(validated_metric_placement),
        };
        let parent_placement = element
            .level_id
            .as_ref()
            .and_then(|id| storey_placements.get(id).copied())
            .unwrap_or(building_placement);
        let placement = push_element_placement(file, parent_placement, origin_axis, frame);
        let geometry_context = ElementGeometryContext {
            representation_context,
            frame,
        };
        let spatial = written_as.name == "IFCSPACE";
        let entity = if spatial {
            push_space(file, element, context, placement, geometry_context, report)
        } else {
            push_element(
                file,
                element,
                &written_as,
                context,
                placement,
                geometry_context,
                report,
            )
        };
        if spatial {
            &mut decomposition
        } else {
            &mut containment
        }
        .entry(container_identity)
        .or_insert_with(|| (container, Vec::new()))
        .1
        .push(entity);
        // The type comes first: where it holds the type's parameters, the
        // element does not repeat them.
        let type_carries_properties = context.options.settings.types
            && types.associate(file, element, entity, written_as.name, context);
        push_property_set(file, element, entity, context, type_carries_properties);
        if context.options.settings.property_sets.ifc_common {
            common_sets.associate(file, element, entity, written_as.name, context);
        }
        if context.options.settings.property_sets.base_quantities {
            push_quantities(file, element, entity, written_as.name, context);
        }
        if !spatial {
            materials.associate(file, element, entity, context.lengths);
        }
    }
    common_sets.push_relations(file, context);
    types.push_relations(file, context);
    materials.push_associations(file, context.options, context.owner);
    for (identity, (container, elements)) in containment {
        push_containment(
            file,
            context.options,
            context.owner,
            &identity,
            container,
            elements,
        );
    }
    for (identity, (container, spaces)) in decomposition {
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
    report: &mut SolidReport,
) -> EntityRef {
    let representation = element.geometry.as_ref().and_then(|geometry| {
        push_geometry(
            file,
            geometry,
            geometry_context.representation_context,
            geometry_context.frame,
            report,
        )
    });
    file.push(
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
    )
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

fn push_element(
    file: &mut StepFile,
    element: &BimElement,
    written_as: &ResolvedEntity<'_>,
    context: WriteContext<'_>,
    placement: EntityRef,
    geometry_context: ElementGeometryContext,
    report: &mut SolidReport,
) -> EntityRef {
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
            report,
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
    file.push(entity, attributes)
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
    entity: EntityRef,
    products: Vec<EntityRef>,
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
        context: WriteContext<'_>,
    ) -> bool {
        let Some(type_id) = element.type_id.as_ref() else {
            return false;
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
                let mut attributes = vec![
                    global_id(context.options, &identity),
                    reference(context.owner),
                    optional_string(element.type_name.as_deref()),
                    omitted(),
                    omitted(),
                    properties.map_or_else(omitted, |pset| StepValue::List(vec![reference(pset)])),
                    omitted(),
                    string(&type_id.0),
                    omitted(),
                ];
                attributes.extend(declared_attributes(table, entity));
                let written = file.push(entity, attributes);
                entry.insert(TypeEntry {
                    entity: written,
                    products: Vec::new(),
                    carries_properties: properties.is_some(),
                })
            }
        };
        entry.products.push(product);
        // Type parameters are read from the type record, so every element of
        // one carries the same ones and the set on the type states them all.
        // An element whose parameters are *not* on the type says so, and
        // writes its own set as it did before types were exported.
        entry.carries_properties || element.type_properties.is_empty()
    }

    /// One `IfcRelDefinesByType` per type.
    fn push_relations(self, file: &mut StepFile, context: WriteContext<'_>) {
        for ((type_id, entity), entry) in self.types {
            if entry.products.is_empty() {
                continue;
            }
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
                    reference(entry.entity),
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
    report: &mut SolidReport,
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
        BimGeometry::Brep(brep) => push_brep(file, brep, representation_context, frame, report),
        BimGeometry::Assembly(parts) => {
            push_assembly(file, parts, representation_context, frame, report)
        }
    }
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
fn polygon_of(edges: &[BimBrepEdge], frame: GeometryFrame) -> Result<extrusion::Polygon, NotAPrism> {
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
fn push_profile_curve(
    file: &mut StepFile,
    lengths: Lengths,
    boundary: &[[f64; 2]],
) -> EntityRef {
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
) -> Option<(EntityRef, &'static str)> {
    if brep.faces.is_empty() {
        return None;
    }
    let mut faces = Vec::with_capacity(brep.faces.len());
    // A face this cannot write leaves the shell open instead of discarding
    // the whole body, which is what the incomplete-shell path is for. The
    // closed-solid claim then has to account for it: a shell missing a face
    // the source does declare is not closed, however the face was lost.
    let mut wrote_every_face = true;
    for face in &brep.faces {
        match push_advanced_face(file, face, frame) {
            Some(written) => faces.push(written),
            None => wrote_every_face = false,
        }
    }
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
    Some((item, representation_type))
}

/// One body's shape representation: the sweep it is, where it is one, and
/// otherwise the item [`push_brep_item`] wrote.
fn push_brep(
    file: &mut StepFile,
    brep: &BimBrep,
    representation_context: EntityRef,
    frame: GeometryFrame,
    report: &mut SolidReport,
) -> Option<EntityRef> {
    let read = prism_of(brep, frame);
    report.saw(read.as_ref().map_err(|refusal| *refusal), brep.faces.len());
    note_curves(brep, report);
    let (item, representation_type) = match read {
        Ok(prism) => (
            push_extruded_area_solid(file, &prism, frame.lengths),
            "SweptSolid",
        ),
        Err(_) => push_brep_item(file, brep, frame)?,
    };
    let body = push_body_representation(
        file,
        representation_context,
        representation_type,
        vec![item],
    );
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
    report: &mut SolidReport,
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
        report.saw(outcome.as_ref().map_err(|refusal| *refusal), part.faces.len());
        note_curves(part, report);
    }
    if let Ok(prisms) = read.into_iter().collect::<Result<Vec<_>, _>>() {
        let items = prisms
            .iter()
            .map(|prism| push_extruded_area_solid(file, prism, frame.lengths))
            .collect();
        let body = push_body_representation(file, representation_context, "SweptSolid", items);
        return Some(file.push(
            "IFCPRODUCTDEFINITIONSHAPE",
            vec![omitted(), omitted(), StepValue::List(vec![reference(body)])],
        ));
    }
    let mut items = Vec::with_capacity(parts.len());
    for part in parts {
        match push_brep_item(file, part, frame) {
            Some((item, "AdvancedBrep")) => items.push(item),
            _ => return None,
        }
    }
    if items.is_empty() {
        return None;
    }
    let body = push_body_representation(file, representation_context, "AdvancedBrep", items);
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
    match profile {
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
    let curve = {
        let points = ends.map(|height| {
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
    // lie. Both the placement and the axis are in the element's coordinates,
    // not the profile's - measured against `ifcopenshell`, which builds the
    // cone the profile describes for the one reading and a different surface
    // for the other.
    let position = push_local_axis(
        file,
        revolution.center,
        cross(radial, revolution.z_axis),
        radial,
        frame,
    )?;
    let axis = push_axis_placement_1d(file, revolution.center, revolution.z_axis, frame)?;
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
    // A full turn, in the radians this file declares its angles in.
    let trimmed = file.push(
        "IFCTRIMMEDCURVE",
        vec![
            reference(circle),
            StepValue::List(vec![parameter_value(0.0)]),
            StepValue::List(vec![parameter_value(std::f64::consts::TAU)]),
            StepValue::Boolean(true),
            enumeration("PARAMETER"),
        ],
    );
    let profile = file.push(
        "IFCARBITRARYOPENPROFILEDEF",
        vec![enumeration("CURVE"), omitted(), reference(trimmed)],
    );
    let position = push_local_axis(
        file,
        revolution.center,
        cross(radial, revolution.z_axis),
        radial,
        frame,
    )?;
    let axis = push_axis_placement_1d(file, revolution.center, revolution.z_axis, frame)?;
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
    let min = metric_coordinates(&bounds.min)?;
    let max = metric_coordinates(&bounds.max)?;
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
    let mut world = metric_coordinates(point)?;
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

/// An axis with a position but no reference direction: what a surface of
/// revolution turns about.
fn push_axis_placement_1d(
    file: &mut StepFile,
    origin: &BimPoint3,
    axis_world: [f64; 3],
    frame: GeometryFrame,
) -> Option<EntityRef> {
    let point = local_coordinates(origin, frame)?;
    let axis = local_direction(axis_world, frame.placement);
    if point
        .into_iter()
        .chain(axis)
        .any(|value| !value.is_finite())
    {
        return None;
    }
    let point_ref = push_cartesian_point(file, frame.lengths, point);
    let axis_ref = push_direction(file, axis);
    Some(file.push(
        "IFCAXIS1PLACEMENT",
        vec![reference(point_ref), reference(axis_ref)],
    ))
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

/// What has been measured from an element's own exported body.
///
/// Not read from the source: Revit computes its quantities when it exports and
/// stores none of them, and joining every numeric parameter this decode
/// recovers against every quantity in Revit's own export of AR S1 - as it
/// stands, in feet turned to millimetres and the other way - found no carrier
/// for a single one. So they are measured here, from the solid this file
/// carries, and they describe that solid and nothing else.
///
/// Measured against Revit's own `Qto_..BaseQuantities` on the 8 047 closed,
/// planar-faced solids of AR S1 that both files hold: **7 482 reproduce
/// Revit's `NetVolume` to within a thousandth**, and 565 do not, of which 558
/// are walls whose body we export larger than Revit exports its own - a
/// difference in the body, not in the measurement, and the same difference a
/// reader would see by looking at the two solids.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MeasuredQuantities {
    /// The volume the closed shell encloses, in cubic metres.
    net_volume: f64,
    /// The area of every face of it, in square metres.
    net_surface_area: f64,
}

/// Measure a body, where it is one this can measure exactly.
///
/// Only a closed shell of planar faces bounded by straight edges: a curved
/// face would have to be tessellated, and a tessellation is an approximation
/// whose error nothing here bounds. Anything else is left unmeasured rather
/// than estimated.
fn measure(geometry: &BimGeometry) -> Option<MeasuredQuantities> {
    let BimGeometry::Brep(brep) = geometry else {
        return None;
    };
    if !brep.complete || brep.faces.is_empty() {
        return None;
    }
    // Six times the signed volume and twice the area, so the division happens
    // once at the end.
    let mut six_volume = 0.0_f64;
    let mut two_area = 0.0_f64;
    for face in &brep.faces {
        if !matches!(face.surface, BimBrepSurface::Plane { .. }) {
            return None;
        }
        for loop_edges in &face.loops {
            let mut corners = Vec::with_capacity(loop_edges.len());
            for edge in loop_edges {
                if !matches!(edge.curve, BimBrepCurve::Line) {
                    return None;
                }
                corners.push(metric_coordinates(&edge.start)?);
            }
            // The divergence theorem over the face, fan-triangulated from its
            // first corner. A hole is wound against the loop it is in, so its
            // triangles take themselves back out of both sums.
            let Some((origin, rest)) = corners.split_first() else {
                continue;
            };
            for pair in rest.windows(2) {
                let first = subtract(pair[0], *origin);
                let second = subtract(pair[1], *origin);
                let normal = cross(first, second);
                six_volume += dot(*origin, normal);
                two_area += dot(normal, normal).sqrt();
            }
        }
    }
    let net_volume = six_volume.abs() / 6.0;
    let net_surface_area = two_area / 2.0;
    (net_volume.is_finite() && net_surface_area.is_finite() && net_volume > 0.0).then_some(
        MeasuredQuantities {
            net_volume,
            net_surface_area,
        },
    )
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
    let Some(measured) = element.geometry.as_ref().and_then(measure) else {
        return;
    };
    let Some((set_name, allowed)) = base_quantity_set(product_entity) else {
        return;
    };
    let mut quantities = Vec::new();
    if allowed.contains(&"NetVolume") {
        quantities.push(reference(file.push(
            "IFCQUANTITYVOLUME",
            vec![
                string("NetVolume"),
                omitted(),
                omitted(),
                StepValue::Real(measured.net_volume),
                omitted(),
            ],
        )));
    }
    if allowed.contains(&"NetSurfaceArea") {
        quantities.push(reference(file.push(
            "IFCQUANTITYAREA",
            vec![
                string("NetSurfaceArea"),
                omitted(),
                omitted(),
                StepValue::Real(measured.net_surface_area),
                omitted(),
            ],
        )));
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
    /// layers is written once.
    materials: BTreeMap<String, EntityRef>,
    /// `IfcMaterialLayerSet` by the set's identity, with the products carrying
    /// it. `BTreeMap` rather than a hash so the output stays deterministic.
    layer_sets: BTreeMap<String, (EntityRef, Vec<EntityRef>)>,
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

    /// One `IfcRelAssociatesMaterial` per distinct build-up.
    fn push_associations(self, file: &mut StepFile, options: &MetadataOptions, owner: EntityRef) {
        for (identity, (layer_set, products)) in self.layer_sets {
            if products.is_empty() {
                continue;
            }
            file.push(
                "IFCRELASSOCIATESMATERIAL",
                vec![
                    global_id(options, &format!("material-relation:{identity}")),
                    reference(owner),
                    omitted(),
                    omitted(),
                    StepValue::List(products.into_iter().map(reference).collect()),
                    reference(layer_set),
                ],
            );
        }
    }
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
    let material =
        layer.material.as_ref().and_then(|material| {
            let name = material.name.as_deref()?;
            let identity = material.id.as_ref().map_or_else(
                || format!("name:{name}"),
                |id| format!("id:{}", external_id(id)),
            );
            Some(*materials.entry(identity).or_insert_with(|| {
                file.push("IFCMATERIAL", vec![string(name), omitted(), omitted()])
            }))
        });
    Some(file.push(
        "IFCMATERIALLAYER",
        vec![
            material.map_or_else(omitted, reference),
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
            settings: ExportSettings::default(),
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
        let file = metadata_ifc(&model, &options()).unwrap();
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
    fn writes_verified_local_extents_as_a_box_not_a_body() {
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
        let metres = BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters");
        model.elements[0].geometry = Some(BimGeometry::BoundingBox(BimBoundingBox {
            min: BimPoint3 {
                coordinates: [-0.1, -0.2, -0.3],
                unit: metres.clone(),
            },
            max: BimPoint3 {
                coordinates: [0.1, 0.2, 0.3],
                unit: metres,
            },
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
            curve: BimBrepCurve::Arc(bim_core::BimBrepArc {
                center: metres_point([0.0, 0.0, 3.048]),
                x_axis: [1.0, 0.0, 0.0],
                z_axis: [0.0, 0.0, 1.0],
                radius: metres_number(2.0),
                start_angle: 0.0,
                end_angle: std::f64::consts::FRAC_PI_2,
            }),
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
        model.elements[0].geometry = Some(BimGeometry::Assembly(vec![
            box_brep(true),
            box_brep(true),
        ]));
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
        brep.faces[0].loops[0][0].curve = BimBrepCurve::Polyline(vec![
            metres_point([2.0, 0.0, 3.048]),
            metres_point([1.4, 1.4, 3.048]),
            metres_point([0.0, 2.0, 3.048]),
        ]);
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
                profile,
            },
            loops: vec![vec![BimBrepEdge {
                start: metres_point([1.0, 0.0, heights.0]),
                end: metres_point([1.0, 0.0, heights.1]),
                curve: BimBrepCurve::Line,
            }]],
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
