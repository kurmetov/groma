use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use bim_core::{
    BimBoundingBox, BimBrep, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepProfile,
    BimBrepSurface, BimElement, BimElementId, BimElementType, BimExternalId, BimGeometry, BimLevel,
    BimLineSegment, BimMaterialLayer, BimModel, BimNumber, BimPlacement, BimPoint3, BimProperty,
    BimPropertyValue,
};

use crate::{EntityRef, IfcGuid, StepFile, StepHeader, StepValue, mapping::resolved_element_type};

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
    validate_model(model)?;
    let mut file = StepFile::new(step_header(options));
    let ownership = push_ownership(&mut file, options.creation_time);
    let context = push_context(&mut file);
    let units = push_units(&mut file);
    let project = push_project(&mut file, options, ownership, context, units);
    let origin_axis = push_axis(&mut file, 0.0);
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
        options,
        ownership,
        building,
        building_placement,
        &storeys,
        &placements,
        &elevations,
        context,
        origin_axis,
    );
    Ok(file)
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

fn push_context(file: &mut StepFile) -> EntityRef {
    let axis = push_axis(file, 0.0);
    file.push(
        "IFCGEOMETRICREPRESENTATIONCONTEXT",
        vec![
            omitted(),
            string("Model"),
            StepValue::Integer(3),
            StepValue::Real(1.0e-5),
            reference(axis),
            omitted(),
        ],
    )
}

fn push_axis(file: &mut StepFile, elevation: f64) -> EntityRef {
    let point = file.push(
        "IFCCARTESIANPOINT",
        vec![StepValue::List(vec![
            StepValue::Real(0.0),
            StepValue::Real(0.0),
            StepValue::Real(elevation),
        ])],
    );
    file.push(
        "IFCAXIS2PLACEMENT3D",
        vec![reference(point), omitted(), omitted()],
    )
}

fn push_units(file: &mut StepFile) -> EntityRef {
    let mut units = Vec::new();
    for (unit_type, prefix, name) in [
        ("LENGTHUNIT", None, "METRE"),
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
    file.push(
        "IFCPROJECT",
        vec![
            global_id(options, "project"),
            reference(owner),
            string(&options.project_name),
            omitted(),
            omitted(),
            omitted(),
            omitted(),
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
            string(&options.site_name),
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
    file.push(
        "IFCBUILDING",
        vec![
            global_id(options, "building"),
            reference(owner),
            string(&options.building_name),
            omitted(),
            omitted(),
            reference(placement),
            omitted(),
            omitted(),
            enumeration("ELEMENT"),
            omitted(),
            omitted(),
            omitted(),
        ],
    )
}

fn push_storeys(
    file: &mut StepFile,
    levels: &[BimLevel],
    options: &MetadataOptions,
    owner: EntityRef,
    building_placement: EntityRef,
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
        let axis = push_axis(file, elevation);
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
                StepValue::Real(elevation),
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
    options: &MetadataOptions,
    owner: EntityRef,
    building: EntityRef,
    building_placement: EntityRef,
    storeys: &BTreeMap<BimElementId, EntityRef>,
    storey_placements: &BTreeMap<BimElementId, EntityRef>,
    storey_elevations: &BTreeMap<BimElementId, f64>,
    representation_context: EntityRef,
    origin_axis: EntityRef,
) {
    let mut materials = MaterialLibrary::default();
    let mut containment: BTreeMap<String, (EntityRef, Vec<EntityRef>)> = BTreeMap::new();
    // A space is part of the spatial structure, so its storey decomposes it
    // rather than containing it. Kept apart from the first pass so the two
    // relationships never carry the same product.
    let mut decomposition: BTreeMap<String, (EntityRef, Vec<EntityRef>)> = BTreeMap::new();
    for element in elements {
        let (container, container_identity) = element
            .level_id
            .as_ref()
            .and_then(|id| Some((storeys.get(id).copied()?, format!("storey:{}", id.0))))
            .unwrap_or_else(|| (building, "building".to_owned()));
        let parent_placement = element
            .level_id
            .as_ref()
            .and_then(|id| storey_placements.get(id).copied())
            .unwrap_or(building_placement);
        let placement_elevation = element
            .level_id
            .as_ref()
            .and_then(|id| storey_elevations.get(id).copied())
            .unwrap_or(0.0);
        let metric_placement = element
            .placement
            .as_ref()
            .and_then(validated_metric_placement);
        let placement = push_element_placement(
            file,
            parent_placement,
            origin_axis,
            placement_elevation,
            metric_placement,
        );
        let geometry_context = ElementGeometryContext {
            representation_context,
            storey_elevation: placement_elevation,
            placement: metric_placement,
        };
        let spatial = resolved_element_type(element).is_spatial();
        let entity = if spatial {
            push_space(file, element, options, owner, placement, geometry_context)
        } else {
            push_element(file, element, options, owner, placement, geometry_context)
        };
        let relationship = if spatial {
            &mut decomposition
        } else {
            &mut containment
        };
        relationship
            .entry(container_identity)
            .or_insert_with(|| (container, Vec::new()))
            .1
            .push(entity);
        push_property_set(file, element, entity, options, owner);
        if !spatial {
            materials.associate(file, element, entity);
        }
    }
    materials.push_associations(file, options, owner);
    for (identity, (container, elements)) in containment {
        push_containment(file, options, owner, &identity, container, elements);
    }
    for (identity, (container, spaces)) in decomposition {
        push_aggregate(
            file,
            options,
            owner,
            &format!("spaces:{identity}"),
            container,
            spaces,
        );
    }
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
    options: &MetadataOptions,
    owner: EntityRef,
    placement: EntityRef,
    geometry_context: ElementGeometryContext,
) -> EntityRef {
    let representation = element.geometry.as_ref().and_then(|geometry| {
        push_geometry(
            file,
            geometry,
            geometry_context.representation_context,
            geometry_context.storey_elevation,
            geometry_context.placement,
        )
    });
    file.push(
        "IFCSPACE",
        vec![
            global_id(options, &format!("element:{}", element.id.0)),
            reference(owner),
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

#[derive(Clone, Copy)]
struct ElementGeometryContext {
    representation_context: EntityRef,
    storey_elevation: f64,
    placement: Option<MetricPlacement>,
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
    storey_elevation: f64,
    placement: Option<MetricPlacement>,
) -> EntityRef {
    let relative = placement.map_or(origin_axis, |placement| {
        let mut origin = placement.origin;
        origin[2] -= storey_elevation;
        let point = push_cartesian_point(file, origin);
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
    options: &MetadataOptions,
    owner: EntityRef,
    placement: EntityRef,
    geometry_context: ElementGeometryContext,
) -> EntityRef {
    let object_type = element.class_name.as_deref().or_else(|| {
        element
            .category
            .as_ref()
            .map(|category| category.name.as_str())
    });
    let entity = product_entity(resolved_element_type(element));
    let representation = element.geometry.as_ref().and_then(|geometry| {
        push_geometry(
            file,
            geometry,
            geometry_context.representation_context,
            geometry_context.storey_elevation,
            geometry_context.placement,
        )
    });
    let mut attributes = vec![
        global_id(options, &format!("element:{}", element.id.0)),
        reference(owner),
        string(element.name.as_deref().unwrap_or(&element.id.0)),
        omitted(),
        optional_string(object_type),
        reference(placement),
        representation.map_or_else(omitted, reference),
        string(&element.id.0),
    ];
    // A few entities declare their own attributes between `IfcElement`'s eight
    // and `PredefinedType`; those are left unset rather than invented, but they
    // must still be written or `PredefinedType` lands in the wrong slot.
    for _ in 0..entity.attributes_before_predefined_type {
        attributes.push(omitted());
    }
    if entity.has_predefined_type {
        attributes.push(enumeration("NOTDEFINED"));
    }
    // STEP writes every attribute an entity declares, set or not, so the ones
    // after `PredefinedType` are written unset rather than left off.
    for _ in 0..entity.attributes_after_predefined_type {
        attributes.push(omitted());
    }
    file.push(entity.name, attributes)
}

/// The IFC4 product entity for a normalized type. The distribution supertypes
/// are instantiable but, unlike the typed leaves, declare no `PredefinedType`.
struct ProductEntity {
    name: &'static str,
    has_predefined_type: bool,
    /// Attributes the entity declares between `IfcElement.Tag` and its
    /// `PredefinedType`. `IfcStairFlight` has four - `NumberOfRisers`,
    /// `NumberOfTreads`, `RiserHeight` and `TreadLength` - and `IfcWindow` and
    /// `IfcDoor` two apiece, `OverallHeight` and `OverallWidth`.
    attributes_before_predefined_type: usize,
    /// Attributes the entity declares after its `PredefinedType`: a window's
    /// `PartitioningType` and `UserDefinedPartitioningType`, a door's
    /// `OperationType` and `UserDefinedOperationType`.
    attributes_after_predefined_type: usize,
}

fn product_entity(element_type: BimElementType) -> ProductEntity {
    let mut attributes_before_predefined_type = 0;
    let mut attributes_after_predefined_type = 0;
    let (name, has_predefined_type) = match element_type {
        BimElementType::PipeSegment => ("IFCPIPESEGMENT", true),
        BimElementType::PipeFitting => ("IFCPIPEFITTING", true),
        BimElementType::SanitaryTerminal => ("IFCSANITARYTERMINAL", true),
        BimElementType::AirTerminal => ("IFCAIRTERMINAL", true),
        BimElementType::FireSuppressionTerminal => ("IFCFIRESUPPRESSIONTERMINAL", true),
        BimElementType::Alarm => ("IFCALARM", true),
        BimElementType::CableCarrierFitting => ("IFCCABLECARRIERFITTING", true),
        BimElementType::DistributionElement => ("IFCDISTRIBUTIONELEMENT", false),
        BimElementType::DistributionFlowElement => ("IFCDISTRIBUTIONFLOWELEMENT", false),
        BimElementType::Wall => ("IFCWALL", true),
        BimElementType::Slab => ("IFCSLAB", true),
        BimElementType::Roof => ("IFCROOF", true),
        BimElementType::Stair => ("IFCSTAIR", true),
        BimElementType::StairFlight => {
            attributes_before_predefined_type = 4;
            ("IFCSTAIRFLIGHT", true)
        }
        BimElementType::CurtainWall => ("IFCCURTAINWALL", true),
        BimElementType::Railing => ("IFCRAILING", true),
        BimElementType::Column => ("IFCCOLUMN", true),
        BimElementType::Member => ("IFCMEMBER", true),
        BimElementType::Plate => ("IFCPLATE", true),
        // A window and a door carry their overall size before the predefined
        // type and a partitioning or operation type after it. None of the four
        // is read from the source, so all four are written unset.
        BimElementType::Window => {
            attributes_before_predefined_type = 2;
            attributes_after_predefined_type = 2;
            ("IFCWINDOW", true)
        }
        BimElementType::Door => {
            attributes_before_predefined_type = 2;
            attributes_after_predefined_type = 2;
            ("IFCDOOR", true)
        }
        BimElementType::Unknown => ("IFCBUILDINGELEMENTPROXY", true),
        // A space never reaches here: `push_elements` sends a spatial type to
        // `push_space`, whose attributes are a spatial element's rather than
        // an element's. The match must still be total, and naming the entity
        // is better than a panic.
        BimElementType::Space => ("IFCSPACE", false),
    };
    ProductEntity {
        name,
        has_predefined_type,
        attributes_before_predefined_type,
        attributes_after_predefined_type,
    }
}

fn push_geometry(
    file: &mut StepFile,
    geometry: &BimGeometry,
    representation_context: EntityRef,
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    match geometry {
        BimGeometry::AxisLine(line) => {
            let (_, axis) = push_axis_line(
                file,
                line,
                representation_context,
                placement_elevation,
                metric_placement,
            )?;
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
            let (directrix, axis) = push_axis_line(
                file,
                &swept_disk.directrix,
                representation_context,
                placement_elevation,
                metric_placement,
            )?;
            let solid = file.push(
                "IFCSWEPTDISKSOLID",
                vec![
                    reference(directrix),
                    StepValue::Real(swept_disk.radius.value),
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
        BimGeometry::BoundingBox(bounds) => push_bounding_box(file, bounds, representation_context),
        BimGeometry::Brep(brep) => push_brep(
            file,
            brep,
            representation_context,
            placement_elevation,
            metric_placement,
        ),
    }
}

/// `IfcAdvancedBrep`/`IfcClosedShell` when [`BimBrep::complete`] holds, so a
/// closed-solid claim always corresponds to every source face resolving;
/// otherwise `IfcShellBasedSurfaceModel`/`IfcOpenShell` over whichever faces
/// did resolve, which is schema-valid for a shell known to be incomplete.
fn push_brep(
    file: &mut StepFile,
    brep: &BimBrep,
    representation_context: EntityRef,
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
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
        match push_advanced_face(file, face, placement_elevation, metric_placement) {
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
    let body = file.push(
        "IFCSHAPEREPRESENTATION",
        vec![
            reference(representation_context),
            string("Body"),
            string(representation_type),
            StepValue::List(vec![reference(item)]),
        ],
    );
    Some(file.push(
        "IFCPRODUCTDEFINITIONSHAPE",
        vec![omitted(), omitted(), StepValue::List(vec![reference(body)])],
    ))
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
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    let surface = push_brep_surface(file, face, placement_elevation, metric_placement)?;
    if face.loops.is_empty() {
        return None;
    }
    let mut bounds = Vec::with_capacity(face.loops.len());
    for (index, loop_edges) in face.loops.iter().enumerate() {
        let edge_loop = push_edge_loop(file, loop_edges, placement_elevation, metric_placement)?;
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
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    match &face.surface {
        BimBrepSurface::Plane {
            origin,
            x_axis,
            y_axis,
        } => {
            let normal = cross(*x_axis, *y_axis);
            let axis = push_local_axis(
                file,
                origin,
                normal,
                *x_axis,
                placement_elevation,
                metric_placement,
            )?;
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
            let axis = push_local_axis(
                file,
                center,
                *z_axis,
                *x_axis,
                placement_elevation,
                metric_placement,
            )?;
            Some(file.push(
                "IFCCYLINDRICALSURFACE",
                vec![reference(axis), StepValue::Real(radius.value)],
            ))
        }
        surface @ BimBrepSurface::Revolution { .. } => push_revolved_surface(
            file,
            surface,
            &face.loops,
            placement_elevation,
            metric_placement,
        ),
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
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
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
    let frame = RevolvedFrame {
        center,
        x_axis: *x_axis,
        y_axis: *y_axis,
        z_axis: *z_axis,
    };
    match profile {
        BimBrepProfile::Line { origin, direction } => push_revolved_line_surface(
            file,
            &frame,
            (origin.coordinates, *direction),
            loops,
            placement_elevation,
            metric_placement,
        ),
        BimBrepProfile::Arc { center, radius, .. } => push_revolved_arc_surface(
            file,
            &frame,
            (center.coordinates, radius),
            placement_elevation,
            metric_placement,
        ),
    }
}

/// A line turned about the frame's axis: a cone, or - where it does not slant -
/// the cylinder or the flat annulus that slant would degenerate into.
fn push_revolved_line_surface(
    file: &mut StepFile,
    frame: &RevolvedFrame,
    (point, direction): ([f64; 3], [f64; 3]),
    loops: &[Vec<BimBrepEdge>],
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
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
    let radial = frame.direction(radial_local);
    // How fast the radius grows as the profile climbs.
    let outward = direction[0] * radial_local[0] + direction[1] * radial_local[1];
    let rise = direction[2];
    let position = frame.point([0.0, 0.0, point[2]]);
    if rise.abs() <= REVOLVED_AXIS_TOLERANCE_METRES {
        // The line is perpendicular to the axis: an annulus, which is flat.
        let axis = push_local_axis(
            file,
            &position,
            frame.z_axis,
            radial,
            placement_elevation,
            metric_placement,
        )?;
        return Some(file.push("IFCPLANE", vec![reference(axis)]));
    }
    if outward.abs() <= REVOLVED_AXIS_TOLERANCE_METRES {
        // Parallel to the axis: a cylinder of that radius.
        let axis = push_local_axis(
            file,
            &position,
            frame.z_axis,
            radial,
            placement_elevation,
            metric_placement,
        )?;
        return Some(file.push(
            "IFCCYLINDRICALSURFACE",
            vec![reference(axis), StepValue::Real(radius)],
        ));
    }
    // IFC4 has no conical surface - `IfcConicalSurface` is ISO 10303-42's, and
    // `ifcopenshell.validate` refuses it - so the cone is written as what the
    // record already says it is: the profile line, revolved about the frame's
    // axis. `IfcSurfaceOfRevolution` sweeps a *bounded* curve, and the bound
    // is the face's own boundary measured along that axis: every point of it
    // lies on the surface, so the span they cover is the span the face needs.
    let slope = outward / rise;
    let (low, high) = axial_span(frame, loops)?;
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
        let points =
            ends.map(|height| push_cartesian_point_2d(file, [profile_radius(height), height]));
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
        frame.center,
        cross(radial, frame.z_axis),
        radial,
        placement_elevation,
        metric_placement,
    )?;
    let axis = push_axis_placement_1d(
        file,
        frame.center,
        frame.z_axis,
        placement_elevation,
        metric_placement,
    )?;
    Some(file.push(
        "IFCSURFACEOFREVOLUTION",
        vec![reference(profile), reference(position), reference(axis)],
    ))
}

/// How far a revolved face's boundary reaches along the frame's axis, measured
/// from the frame's own origin. `None` when the face has no boundary to
/// measure or a point of it is not in metres.
fn axial_span(frame: &RevolvedFrame, loops: &[Vec<BimBrepEdge>]) -> Option<(f64, f64)> {
    let center = metric_coordinates(frame.center)?;
    let mut span: Option<(f64, f64)> = None;
    for edge in loops.iter().flatten() {
        for point in [&edge.start, &edge.end] {
            let height = dot(subtract(metric_coordinates(point)?, center), frame.z_axis);
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
    frame: &RevolvedFrame,
    (point, radius): ([f64; 3], &BimNumber),
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
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
    let position = frame.point([0.0, 0.0, point[2]]);
    if major <= REVOLVED_AXIS_TOLERANCE_METRES {
        // The arc is centred on the axis: a sphere.
        let axis = push_local_axis(
            file,
            &position,
            frame.z_axis,
            frame.x_axis,
            placement_elevation,
            metric_placement,
        )?;
        return Some(file.push(
            "IFCSPHERICALSURFACE",
            vec![reference(axis), StepValue::Real(radius.value)],
        ));
    }
    let radial = frame.direction([point[0] / major, point[1] / major, 0.0]);
    if radius.value >= major * (1.0 - TORUS_RADIUS_MARGIN) {
        // The profile circle reaches the axis or crosses it, and
        // `IfcToroidalSurface` requires a minor radius strictly under the
        // major one - so this torus, which is a real shape with no hole left
        // in it, is written the way the cone is: the profile revolved.
        return push_revolved_arc_as_revolution(
            file,
            frame,
            (radial, [major, point[2]], radius.value),
            placement_elevation,
            metric_placement,
        );
    }
    let axis = push_local_axis(
        file,
        &position,
        frame.z_axis,
        radial,
        placement_elevation,
        metric_placement,
    )?;
    Some(file.push(
        "IFCTOROIDALSURFACE",
        vec![
            reference(axis),
            StepValue::Real(major),
            StepValue::Real(radius.value),
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
    frame: &RevolvedFrame,
    (radial, center, radius): ([f64; 3], [f64; 2], f64),
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    let profile_center = push_cartesian_point_2d(file, center);
    let profile_direction = push_direction_2d(file, [1.0, 0.0]);
    let profile_position = file.push(
        "IFCAXIS2PLACEMENT2D",
        vec![reference(profile_center), reference(profile_direction)],
    );
    let circle = file.push(
        "IFCCIRCLE",
        vec![reference(profile_position), StepValue::Real(radius)],
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
        frame.center,
        cross(radial, frame.z_axis),
        radial,
        placement_elevation,
        metric_placement,
    )?;
    let axis = push_axis_placement_1d(
        file,
        frame.center,
        frame.z_axis,
        placement_elevation,
        metric_placement,
    )?;
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
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    if edges.is_empty() {
        return None;
    }
    let mut vertices = Vec::with_capacity(edges.len());
    for edge in edges {
        let corner = local_coordinates(&edge.start, placement_elevation, metric_placement)?;
        if corner.into_iter().any(|value| !value.is_finite()) {
            return None;
        }
        let point = push_cartesian_point(file, corner);
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
            placement_elevation,
            metric_placement,
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
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    let start = local_coordinates(&edge.start, placement_elevation, metric_placement)?;
    let end = local_coordinates(&edge.end, placement_elevation, metric_placement)?;
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
                vec![reference(direction_ref), StepValue::Real(1.0)],
            );
            // `IfcLine.Pnt` describes the underlying infinite line, not a
            // topological vertex, so it needs its own value, not the shared
            // `IfcVertexPoint`'s.
            let line_point = push_cartesian_point(file, start);
            file.push("IFCLINE", vec![reference(line_point), reference(vector)])
        }
        BimBrepCurve::Arc(arc) => {
            if arc.radius.unit.as_ref()?.id != "autodesk.unit.unit:meters-1.0.0"
                || !arc.radius.value.is_finite()
                || arc.radius.value <= 0.0
            {
                return None;
            }
            let axis = push_local_axis(
                file,
                &arc.center,
                arc.z_axis,
                arc.x_axis,
                placement_elevation,
                metric_placement,
            )?;
            file.push(
                "IFCCIRCLE",
                vec![reference(axis), StepValue::Real(arc.radius.value)],
            )
        }
        BimBrepCurve::Polyline(points) => {
            if points.len() < 3 {
                return None;
            }
            let mut point_refs = Vec::with_capacity(points.len());
            for point in points {
                let coordinates = local_coordinates(point, placement_elevation, metric_placement)?;
                if coordinates.into_iter().any(|value| !value.is_finite()) {
                    return None;
                }
                point_refs.push(reference(push_cartesian_point(file, coordinates)));
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
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    let point = local_coordinates(origin, placement_elevation, metric_placement)?;
    let axis = local_direction(axis_world, metric_placement);
    let ref_direction = local_direction(ref_direction_world, metric_placement);
    if point
        .into_iter()
        .chain(axis)
        .chain(ref_direction)
        .any(|value| !value.is_finite())
    {
        return None;
    }
    let point_ref = push_cartesian_point(file, point);
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
    let corner = push_cartesian_point(file, min);
    let item = file.push(
        "IFCBOUNDINGBOX",
        vec![
            reference(corner),
            StepValue::Real(dimensions[0]),
            StepValue::Real(dimensions[1]),
            StepValue::Real(dimensions[2]),
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
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<(EntityRef, EntityRef)> {
    let start = local_coordinates(&line.start, placement_elevation, metric_placement)?;
    let end = local_coordinates(&line.end, placement_elevation, metric_placement)?;
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
    let start = push_cartesian_point(file, start);
    let end = push_cartesian_point(file, end);
    let line = file.push(
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

fn local_coordinates(
    point: &BimPoint3,
    storey_elevation: f64,
    placement: Option<MetricPlacement>,
) -> Option<[f64; 3]> {
    let mut world = metric_coordinates(point)?;
    if let Some(placement) = placement {
        let delta = subtract(world, placement.origin);
        let local_y = cross(placement.axis, placement.reference_direction);
        return Some([
            dot(delta, placement.reference_direction),
            dot(delta, local_y),
            dot(delta, placement.axis),
        ]);
    }
    if !storey_elevation.is_finite() {
        return None;
    }
    world[2] -= storey_elevation;
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
fn push_cartesian_point_2d(file: &mut StepFile, coordinates: [f64; 2]) -> EntityRef {
    file.push(
        "IFCCARTESIANPOINT",
        vec![StepValue::List(
            coordinates.into_iter().map(StepValue::Real).collect(),
        )],
    )
}

/// An axis with a position but no reference direction: what a surface of
/// revolution turns about.
fn push_axis_placement_1d(
    file: &mut StepFile,
    origin: &BimPoint3,
    axis_world: [f64; 3],
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    let point = local_coordinates(origin, placement_elevation, metric_placement)?;
    let axis = local_direction(axis_world, metric_placement);
    if point
        .into_iter()
        .chain(axis)
        .any(|value| !value.is_finite())
    {
        return None;
    }
    let point_ref = push_cartesian_point(file, point);
    let axis_ref = push_direction(file, axis);
    Some(file.push(
        "IFCAXIS1PLACEMENT",
        vec![reference(point_ref), reference(axis_ref)],
    ))
}

fn push_cartesian_point(file: &mut StepFile, coordinates: [f64; 3]) -> EntityRef {
    file.push(
        "IFCCARTESIANPOINT",
        vec![StepValue::List(
            coordinates.into_iter().map(StepValue::Real).collect(),
        )],
    )
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
    fn associate(&mut self, file: &mut StepFile, element: &BimElement, product: EntityRef) {
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
                    .filter_map(|layer| push_material_layer(file, &mut self.materials, layer))
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
            StepValue::Real(layer.thickness.value),
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
    options: &MetadataOptions,
    owner: EntityRef,
) {
    push_named_property_set(
        file,
        &element.properties,
        product,
        options,
        owner,
        "Rivet Properties",
        &format!("properties:{}", element.id.0),
        &format!("properties-relation:{}", element.id.0),
    );
    // The type's values go in a set of their own. Both sets hang off the same
    // product - the element's type is not itself exported as an
    // `IfcTypeProduct` - so the set name is what keeps "set on this element"
    // and "set on its type" apart for a reader.
    push_named_property_set(
        file,
        &element.type_properties,
        product,
        options,
        owner,
        "Rivet Type Properties",
        &format!("type-properties:{}", element.id.0),
        &format!("type-properties-relation:{}", element.id.0),
    );
}

#[allow(clippy::too_many_arguments)] // Two identifiers and a name, all distinct.
fn push_named_property_set(
    file: &mut StepFile,
    source: &[BimProperty],
    product: EntityRef,
    options: &MetadataOptions,
    owner: EntityRef,
    name: &str,
    key: &str,
    relation_key: &str,
) {
    let names = unique_property_names(source);
    let properties = source
        .iter()
        .zip(&names)
        .filter_map(|(property, name)| push_property(file, property, name))
        .map(reference)
        .collect::<Vec<_>>();
    if properties.is_empty() {
        return;
    }
    let pset = file.push(
        "IFCPROPERTYSET",
        vec![
            global_id(options, key),
            reference(owner),
            string(name),
            omitted(),
            StepValue::List(properties),
        ],
    );
    file.push(
        "IFCRELDEFINESBYPROPERTIES",
        vec![
            global_id(options, relation_key),
            reference(owner),
            omitted(),
            omitted(),
            StepValue::List(vec![reference(product)]),
            reference(pset),
        ],
    );
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

fn push_property(file: &mut StepFile, property: &BimProperty, name: &str) -> Option<EntityRef> {
    let nominal = nominal_value(property)?;
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

fn nominal_value(property: &BimProperty) -> Option<StepValue> {
    let typed = |name: &str, value: StepValue| StepValue::Typed {
        name: name.to_owned(),
        value: Box::new(value),
    };
    match &property.value {
        BimPropertyValue::Bool(value) => Some(typed("IFCBOOLEAN", StepValue::Boolean(*value))),
        BimPropertyValue::Integer(value) => Some(typed("IFCINTEGER", StepValue::Integer(*value))),
        BimPropertyValue::Number(number) => number_value(number, property.specification.as_deref()),
        BimPropertyValue::Text(value) => Some(typed("IFCLABEL", string(value))),
        BimPropertyValue::Reference(value) => Some(typed("IFCIDENTIFIER", string(&value.0))),
        BimPropertyValue::Bytes(_) | BimPropertyValue::Unknown(_) => None,
    }
}

fn number_value(number: &BimNumber, specification: Option<&str>) -> Option<StepValue> {
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
        return Some(typed(measure, StepValue::Real(number.value)));
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
        }
    }

    fn model() -> BimModel {
        let level_id = BimElementId("100".to_owned());
        BimModel {
            source: None,
            levels: vec![BimLevel {
                id: level_id.clone(),
                name: Some("Этаж 1".to_owned()),
                elevation: Some(BimNumber {
                    value: 3.048,
                    unit: Some(BimUnit {
                        id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                        name: "Meters".to_owned(),
                    }),
                }),
            }],
            elements: vec![BimElement {
                id: BimElementId("200".to_owned()),
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
                placement: None,
                geometry: None,
                properties: vec![BimProperty {
                    id: None,
                    name: "Height".to_owned(),
                    specification: Some("autodesk.spec.aec:length-2.0.0".to_owned()),
                    value: BimPropertyValue::Number(BimNumber {
                        value: 2.5,
                        unit: Some(BimUnit {
                            id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                            name: "Meters".to_owned(),
                        }),
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
            unit: Some(BimUnit {
                id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                name: "Meters".to_owned(),
            }),
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
        feet.thickness.unit = Some(BimUnit {
            id: "autodesk.unit.unit:feet-1.0.0".to_owned(),
            name: "Feet".to_owned(),
        });
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
        assert!(text.contains("IFCLENGTHMEASURE(2.500000000000000e0)"));
        assert!(text.contains("=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.)"));
        assert!(text.contains("=IFCOWNERHISTORY(#3,#4,$,.ADDED.,1788506400,#3,#4,1788506400)"));
        assert!(text.contains("'\\X2\\042D04420430043600200031\\X0\\'"));
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
                    unit: BimUnit {
                        id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                        name: "Meters".to_owned(),
                    },
                },
                end: BimPoint3 {
                    coordinates: [1.0, 2.0, 4.5],
                    unit: BimUnit {
                        id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                        name: "Meters".to_owned(),
                    },
                },
            },
            radius: BimNumber {
                value: 0.01,
                unit: Some(BimUnit {
                    id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                    name: "Meters".to_owned(),
                }),
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
        let local_start = format!("({:.15e},{:.15e},{:.15e})", 1.0, 2.0, 3.5 - 3.048);
        assert!(text.contains(&local_start));
    }

    #[test]
    fn writes_an_axis_only_fitting_without_inventing_a_body() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::PipeFitting;
        model.elements[0].placement = Some(BimPlacement {
            origin: BimPoint3 {
                coordinates: [1.0, 2.0, 3.0],
                unit: BimUnit {
                    id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                    name: "Meters".to_owned(),
                },
            },
            reference_direction: [1.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
        });
        model.elements[0].geometry = Some(BimGeometry::AxisLine(BimLineSegment {
            start: BimPoint3 {
                coordinates: [1.0, 2.0, 3.5],
                unit: BimUnit {
                    id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                    name: "Meters".to_owned(),
                },
            },
            end: BimPoint3 {
                coordinates: [1.0, 2.0, 4.5],
                unit: BimUnit {
                    id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                    name: "Meters".to_owned(),
                },
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
        let relative_origin = format!("({:.15e},{:.15e},{:.15e})", 1.0, 2.0, 3.0 - 3.048);
        assert!(text.contains(&relative_origin));
        assert!(text.contains("(0.000000000000000e0,0.000000000000000e0,5.000000000000000e-1)"));
    }

    #[test]
    fn writes_verified_local_extents_as_a_box_not_a_body() {
        let mut model = model();
        model.elements[0].element_type = BimElementType::SanitaryTerminal;
        model.elements[0].placement = Some(BimPlacement {
            origin: BimPoint3 {
                coordinates: [10.0, 20.0, 30.0],
                unit: BimUnit {
                    id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                    name: "Meters".to_owned(),
                },
            },
            reference_direction: [1.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
        });
        let metres = BimUnit {
            id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
            name: "Meters".to_owned(),
        };
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
        assert!(
            text.contains("(-1.000000000000000e-1,-2.000000000000000e-1,-3.000000000000000e-1)")
        );
        assert!(text.contains(",2.000000000000000e-1,4.000000000000000e-1,6.000000000000000e-1)"));
    }

    fn metres_point(coordinates: [f64; 3]) -> BimPoint3 {
        BimPoint3 {
            coordinates,
            unit: BimUnit {
                id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                name: "Meters".to_owned(),
            },
        }
    }

    fn metres_number(value: f64) -> BimNumber {
        BimNumber {
            value,
            unit: Some(BimUnit {
                id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                name: "Meters".to_owned(),
            }),
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
        assert!(text.contains("(2.000000000000000e0,0.000000000000000e0,0.000000000000000e0)"));
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
        assert!(
            text.contains("(1.400000000000000e0,1.400000000000000e0,0.000000000000000e0)"),
            "{text}"
        );
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
        push_brep_surface(&mut file, &face, 0.0, None).expect("the surface was written");
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
        assert!(
            text.contains("((9.000000000000000e-1,-1.000000000000000e-1))"),
            "{text}"
        );
        assert!(
            text.contains("((3.100000000000000e0,2.100000000000000e0))"),
            "{text}"
        );
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
        assert!(
            text.contains("((0.000000000000000e0,0.000000000000000e0))"),
            "{text}"
        );
        assert!(
            text.contains("((2.100000000000000e0,2.100000000000000e0))"),
            "{text}"
        );
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
        assert!(
            text.contains("((0.000000000000000e0,-1.000000000000000e0))"),
            "{text}"
        );
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
        assert!(text.contains("1.500000000000000e0"), "{text}");
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
        assert!(text.contains("0.000000000000000e0,0.000000000000000e0,4.000000000000000e0"));
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
        assert!(
            text.contains("2.000000000000000e0,5.000000000000000e-1);"),
            "{text}"
        );
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
        assert!(
            text.contains("((5.000000000000000e-1,0.000000000000000e0))"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "IFCPARAMETERVALUE({:.15e})),.T.,.PARAMETER.);",
                std::f64::consts::TAU
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
        assert!(text.contains("7.500000000000000e-1);"), "{text}");
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
