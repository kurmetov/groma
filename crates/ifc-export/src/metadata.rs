use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use bim_core::{
    BimBoundingBox, BimBrep, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepSurface, BimElement,
    BimElementId, BimElementType, BimExternalId, BimGeometry, BimLevel, BimLineSegment, BimModel,
    BimNumber, BimPlacement, BimPoint3, BimProperty, BimPropertyValue,
};

use crate::{EntityRef, IfcGuid, StepFile, StepHeader, StepValue, mapping::resolved_element_type};

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
    let mut containment: BTreeMap<String, (EntityRef, Vec<EntityRef>)> = BTreeMap::new();
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
        let entity = push_element(
            file,
            element,
            options,
            owner,
            placement,
            ElementGeometryContext {
                representation_context,
                storey_elevation: placement_elevation,
                placement: metric_placement,
            },
        );
        containment
            .entry(container_identity)
            .or_insert_with(|| (container, Vec::new()))
            .1
            .push(entity);
        push_property_set(file, element, entity, options, owner);
    }
    for (identity, (container, elements)) in containment {
        push_containment(file, options, owner, &identity, container, elements);
    }
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
    if entity.has_predefined_type {
        attributes.push(enumeration("NOTDEFINED"));
    }
    file.push(entity.name, attributes)
}

/// The IFC4 product entity for a normalized type. The distribution supertypes
/// are instantiable but, unlike the typed leaves, declare no `PredefinedType`.
struct ProductEntity {
    name: &'static str,
    has_predefined_type: bool,
}

fn product_entity(element_type: BimElementType) -> ProductEntity {
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
        BimElementType::Unknown => ("IFCBUILDINGELEMENTPROXY", true),
    };
    ProductEntity {
        name,
        has_predefined_type,
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
    for face in &brep.faces {
        faces.push(push_advanced_face(
            file,
            face,
            placement_elevation,
            metric_placement,
        )?);
    }
    let face_list = StepValue::List(faces.into_iter().map(reference).collect());
    let (item, representation_type) = if brep.complete {
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
    let surface = push_brep_surface(file, &face.surface, placement_elevation, metric_placement)?;
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

fn push_brep_surface(
    file: &mut StepFile,
    surface: &BimBrepSurface,
    placement_elevation: f64,
    metric_placement: Option<MetricPlacement>,
) -> Option<EntityRef> {
    match surface {
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
    }
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

fn push_cartesian_point(file: &mut StepFile, coordinates: [f64; 3]) -> EntityRef {
    file.push(
        "IFCCARTESIANPOINT",
        vec![StepValue::List(
            coordinates.into_iter().map(StepValue::Real).collect(),
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
    let properties = source
        .iter()
        .filter_map(|property| push_property(file, property))
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

fn push_property(file: &mut StepFile, property: &BimProperty) -> Option<EntityRef> {
    let nominal = nominal_value(property)?;
    Some(file.push(
        "IFCPROPERTYSINGLEVALUE",
        vec![
            string(&property.name),
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

fn enumeration(value: &str) -> StepValue {
    StepValue::Enumeration(value.to_owned())
}

#[cfg(test)]
mod tests {
    use bim_core::{BimCategory, BimLineSegment, BimPlacement, BimSweptDisk, BimUnit};

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
        }
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
