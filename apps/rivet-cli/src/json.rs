//! `export-json`'s writers.
//!
//! These write the Revit intermediate itself, not the canonical model: every
//! decoded face, the boxes a body was checked against, and the readings the
//! decode could not resolve. That is why they take an
//! [`rvt_import::ExportedElement`] rather than a `BimElement`.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{self, Write},
    path::Path,
};

use bim_core::{
    BimBrep, BimBrepCurve, BimBrepProfile, BimBrepRuling, BimBrepSurface, BimGeometry,
    BimMaterialLayerSet, BimPoint3, BimProperty, BimPropertyValue,
};
use revit_catalog::Catalog;
use rvt_model::{
    FamilyInstancePlacementFields, GElementBounds, GInstanceTransformFields, ParameterValue,
    RvtPoint3,
};
use rvt_schema::Schema;
// The whole semantic reconstruction moved into `rvt-import`. Glob-imported
// because the probe commands below read the same intermediate the pipeline
// builds, and naming each item here would be a second list to keep in step.
use rvt_import::{ExportedElement, RecoveredElements, normalize_element, normalize_placed_brep};

pub(crate) struct ExportMetadata<'a> {
    pub(crate) schema: Option<&'a Schema>,
    pub(crate) partition_paths: &'a [String],
    pub(crate) parameter_names: &'a BTreeMap<i32, String>,
    pub(crate) parameter_specs: &'a BTreeMap<i32, String>,
    pub(crate) catalog: Option<Catalog>,
    /// Write every decoded section rather than the selection the IFC export
    /// would make. See [`write_model_json`] and [`write_body_json`].
    pub(crate) full: bool,
}

pub(crate) fn write_exported_elements(
    mut writer: Box<dyn Write>,
    path: &Path,
    recovered: &RecoveredElements,
    metadata: &ExportMetadata<'_>,
    limit: Option<usize>,
) -> io::Result<usize> {
    let elements = &recovered.elements;
    if metadata.full {
        write_model_json(&mut writer, path, recovered, metadata)?;
    }
    let mut written = 0_usize;
    for (id, element) in elements {
        if limit.is_some_and(|limit| written >= limit) {
            break;
        }
        written += 1;
        write_element_json(&mut writer, *id, element, elements, metadata)?;
    }
    writer.flush()?;
    Ok(written)
}

/// Emit one element as a JSON object on its own line. Fields that were not
/// recovered are omitted rather than written as a guessed value.
/// Resolve the references that name something to that name. A consumer
/// otherwise has to join the whole file to itself to answer "which storey is
/// this on", and that join is exactly what a downstream index cannot do
/// cheaply. The referenced element keeps its own record, so the name's
/// provenance stays recoverable through the identifier written alongside.
pub(crate) fn write_resolved_reference_names(
    writer: &mut impl Write,
    element: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
) -> io::Result<()> {
    for (key, referenced) in [
        ("level_name", element.level_id),
        ("type_name", element.type_element_id),
        (
            "family_name",
            element.family_id.or(element.header_family_id),
        ),
    ] {
        let Some(name) = referenced
            .and_then(|id| u32::try_from(id).ok())
            .and_then(|id| elements.get(&id))
            .and_then(|referenced| referenced.name.as_ref())
        else {
            continue;
        };
        write!(writer, ",\"{key}\":\"{}\"", json_escape(&name.0))?;
    }
    Ok(())
}

/// The model line `--full` writes ahead of the elements: what the file is,
/// which sections the decode recovered, and how much each one holds.
///
/// It is an index rather than a summary - every count here is the size of a
/// section the following lines carry in full, so a reader can tell an empty
/// section from one this decode never reaches.
pub(crate) fn write_model_json(
    writer: &mut impl Write,
    path: &Path,
    recovered: &RecoveredElements,
    metadata: &ExportMetadata<'_>,
) -> io::Result<()> {
    let tally = ModelTally::of(&recovered.elements);
    write!(writer, "{{\"kind\":\"model\"")?;
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        write!(writer, ",\"file\":\"{}\"", json_escape(name))?;
    }
    if let Some(release) = recovered.release {
        write!(writer, ",\"revit_release\":{release}")?;
    }
    write!(
        writer,
        ",\"parameter_catalog\":{},\"parameter_values_schema_bound\":{}",
        recovered.catalog.is_some(),
        recovered.parameter_values_schema_bound
    )?;
    if let Some(schema) = metadata.schema {
        write!(
            writer,
            ",\"schema\":{{\"classes\":{},\"properties\":{}}}",
            schema.classes.len(),
            schema.property_count
        )?;
    }
    write!(writer, ",\"partitions\":[")?;
    for (index, partition) in metadata.partition_paths.iter().enumerate() {
        let separator = if index > 0 { "," } else { "" };
        write!(writer, "{separator}\"{}\"", json_escape(partition))?;
    }
    write!(writer, "]")?;
    write!(
        writer,
        ",\"elements\":{},\"records\":{},\"with_class\":{},\"with_category\":{},\
         \"with_level\":{},\"with_name\":{},\"with_type\":{},\"moribund\":{}",
        recovered.elements.len(),
        tally.records,
        tally.with_class,
        tally.with_category,
        tally.with_level,
        tally.with_name,
        tally.with_type,
        tally.moribund
    )?;
    write!(
        writer,
        ",\"parameters\":{{\"values\":{},\"type_values\":{},\"named_definitions\":{},\
         \"specs\":{}}}",
        tally.parameter_values,
        tally.type_parameter_values,
        metadata.parameter_names.len(),
        metadata.parameter_specs.len()
    )?;
    write!(
        writer,
        ",\"bodies\":{{\"elements\":{},\"records\":{},\"placed\":{},\"complete\":{},\
         \"faces\":{},\"excluded_faces\":{},\"edges\":{},\"failed_edges\":{}}}",
        tally.body_elements,
        tally.body_records,
        tally.placed_bodies,
        tally.complete_bodies,
        tally.faces,
        tally.excluded_faces,
        tally.edges,
        tally.failed_edges
    )?;
    write!(
        writer,
        ",\"placements\":{{\"ginstance_transforms\":{},\"verified_symbol_links\":{}}}",
        tally.transforms, tally.verified_symbol_links
    )?;
    write!(
        writer,
        ",\"units\":{{\"length\":\"meters\",\"source\":\"Revit internal feet\",\
         \"angle\":\"radians\"}}"
    )?;
    write_model_classes_json(writer, &tally.classes, metadata)?;
    writeln!(writer, "}}")
}

/// The class histogram: which kinds of record this file holds and how many
/// elements of each, most first. This is the index into the element lines.
pub(crate) fn write_model_classes_json(
    writer: &mut impl Write,
    classes: &BTreeMap<u16, usize>,
    metadata: &ExportMetadata<'_>,
) -> io::Result<()> {
    let mut by_count = classes.iter().collect::<Vec<_>>();
    by_count.sort_by(|left, right| right.1.cmp(left.1).then(left.0.cmp(right.0)));
    write!(writer, ",\"classes\":[")?;
    for (index, (class_index, count)) in by_count.iter().enumerate() {
        let separator = if index > 0 { "," } else { "" };
        write!(writer, "{separator}{{\"index\":{class_index}")?;
        if let Some(class) = metadata
            .schema
            .and_then(|schema| schema.class_by_index(**class_index))
        {
            write!(writer, ",\"name\":\"{}\"", json_escape(&class.name))?;
        }
        write!(writer, ",\"elements\":{count}}}")?;
    }
    write!(writer, "]")
}

/// How much each recovered section holds, counted once for the model line.
#[derive(Default)]
pub(crate) struct ModelTally {
    pub(crate) records: usize,
    pub(crate) with_class: usize,
    pub(crate) with_category: usize,
    pub(crate) with_level: usize,
    pub(crate) with_name: usize,
    pub(crate) with_type: usize,
    pub(crate) moribund: usize,
    pub(crate) parameter_values: usize,
    pub(crate) type_parameter_values: usize,
    pub(crate) transforms: usize,
    pub(crate) verified_symbol_links: usize,
    pub(crate) body_elements: usize,
    pub(crate) body_records: usize,
    pub(crate) placed_bodies: usize,
    pub(crate) complete_bodies: usize,
    pub(crate) faces: usize,
    pub(crate) excluded_faces: usize,
    pub(crate) edges: usize,
    pub(crate) failed_edges: usize,
    pub(crate) classes: BTreeMap<u16, usize>,
}

impl ModelTally {
    pub(crate) fn of(elements: &BTreeMap<u32, ExportedElement>) -> Self {
        let mut tally = Self::default();
        for element in elements.values() {
            tally.records += element.record_count;
            tally.with_class += usize::from(element.class_index.is_some());
            tally.with_category += usize::from(element.category.is_some());
            tally.with_level += usize::from(element.level_id.is_some());
            tally.with_name += usize::from(element.name.is_some());
            tally.with_type += usize::from(element.type_element_reference().is_some());
            tally.parameter_values += element.parameters.len();
            tally.type_parameter_values += element.type_parameters.len();
            tally.moribund += usize::from(element.moribund);
            tally.transforms += usize::from(element.ginstance_transform.is_some());
            tally.verified_symbol_links += usize::from(element.verified_symbol_bounds.is_some());
            if let Some(class_index) = element.class_index {
                *tally.classes.entry(class_index).or_default() += 1;
            }
            if let Some(brep) = &element.brep {
                tally.body_elements += 1;
                tally.body_records += element.brep_records;
                tally.placed_bodies += usize::from(element.brep_is_placed);
                tally.complete_bodies += usize::from(brep.excluded_faces.is_empty());
                tally.faces += brep.faces.len();
                tally.excluded_faces += brep.excluded_faces.len();
                tally.edges += brep
                    .faces
                    .iter()
                    .flat_map(|face| face.loops.iter())
                    .map(Vec::len)
                    .sum::<usize>();
                tally.failed_edges += brep.failed_edges.len();
            }
        }
        tally
    }
}

/// A finite `f64` as JSON, and `null` for anything else. A decoded coordinate
/// can be infinite or NaN where a reading went wrong, and writing that
/// verbatim produces a file no JSON parser accepts.
/// An external identifier as a JSON value: a number where the source's
/// identifier is one, and a quoted string otherwise. Revit's are decimal, and
/// the rest of this export writes them unquoted; this keeps that without
/// assuming it of a namespace that might not be numeric.
pub(crate) fn json_identifier(value: &str) -> String {
    value.parse::<i64>().map_or_else(
        |_| format!("\"{}\"", json_escape(value)),
        |id| id.to_string(),
    )
}

pub(crate) fn json_number(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        "null".to_owned()
    }
}

pub(crate) fn write_axis_json(
    writer: &mut impl Write,
    name: &str,
    axis: [f64; 3],
) -> io::Result<()> {
    write!(
        writer,
        ",\"{name}\":[{},{},{}]",
        json_number(axis[0]),
        json_number(axis[1]),
        json_number(axis[2])
    )
}

pub(crate) fn write_point_json(
    writer: &mut impl Write,
    name: &str,
    point: &BimPoint3,
) -> io::Result<()> {
    write_axis_json(writer, name, point.coordinates)
}

/// One profile curve. A revolved surface holds its profile in the surface's
/// own frame; a ruled surface holds both of its profiles in world
/// coordinates, because that is where the source states them.
pub(crate) fn write_brep_profile_json(
    writer: &mut impl Write,
    profile: &BimBrepProfile,
) -> io::Result<()> {
    match profile {
        BimBrepProfile::Line { origin, direction } => {
            write!(writer, "{{\"kind\":\"line\"")?;
            write_point_json(writer, "origin_meters", origin)?;
            write_axis_json(writer, "direction", *direction)?;
            write!(writer, "}}")?;
        }
        BimBrepProfile::Arc {
            center,
            x_axis,
            y_axis,
            radius,
        } => {
            write!(writer, "{{\"kind\":\"arc\"")?;
            write_point_json(writer, "center_meters", center)?;
            write_axis_json(writer, "x_axis", *x_axis)?;
            write_axis_json(writer, "y_axis", *y_axis)?;
            write!(writer, ",\"radius_meters\":{}}}", json_number(radius.value))?;
        }
    }
    Ok(())
}

/// One side of a ruled surface. `start`/`end` are the profile's own parameter
/// interval, which the surface's `u` in [0, 1] is normalised onto.
pub(crate) fn write_brep_ruling_json(
    writer: &mut impl Write,
    ruling: &BimBrepRuling,
) -> io::Result<()> {
    match ruling {
        BimBrepRuling::Point(point) => {
            write!(writer, "{{\"kind\":\"point\"")?;
            write_point_json(writer, "point_meters", point)?;
            write!(writer, "}}")?;
        }
        BimBrepRuling::Curve {
            profile,
            start,
            end,
        } => {
            write!(writer, "{{\"kind\":\"curve\",\"profile\":")?;
            write_brep_profile_json(writer, profile)?;
            write!(
                writer,
                ",\"start\":{},\"end\":{}}}",
                json_number(*start),
                json_number(*end)
            )?;
        }
    }
    Ok(())
}

/// One face's surface. The frame of a revolved surface is in world
/// coordinates and its profile is in that frame's own, exactly as the surface
/// holds them.
pub(crate) fn write_brep_surface_json(
    writer: &mut impl Write,
    surface: &BimBrepSurface,
) -> io::Result<()> {
    match surface {
        BimBrepSurface::Plane {
            origin,
            x_axis,
            y_axis,
        } => {
            write!(writer, "{{\"kind\":\"plane\"")?;
            write_point_json(writer, "origin_meters", origin)?;
            write_axis_json(writer, "x_axis", *x_axis)?;
            write_axis_json(writer, "y_axis", *y_axis)?;
            write!(writer, "}}")?;
        }
        BimBrepSurface::Cylinder {
            center,
            x_axis,
            y_axis,
            z_axis,
            radius,
        } => {
            write!(writer, "{{\"kind\":\"cylinder\"")?;
            write_point_json(writer, "center_meters", center)?;
            write_axis_json(writer, "x_axis", *x_axis)?;
            write_axis_json(writer, "y_axis", *y_axis)?;
            write_axis_json(writer, "z_axis", *z_axis)?;
            write!(writer, ",\"radius_meters\":{}}}", json_number(radius.value))?;
        }
        // The frame is in world coordinates and the profile is in the
        // frame's own, exactly as the surface holds them; naming the
        // profile's numbers `_meters` too keeps that visible without
        // pretending they are world points.
        BimBrepSurface::Revolution {
            center,
            x_axis,
            y_axis,
            z_axis,
            profile,
        } => {
            write!(writer, "{{\"kind\":\"revolution\"")?;
            write_point_json(writer, "center_meters", center)?;
            write_axis_json(writer, "x_axis", *x_axis)?;
            write_axis_json(writer, "y_axis", *y_axis)?;
            write_axis_json(writer, "z_axis", *z_axis)?;
            write!(writer, ",\"profile\":")?;
            write_brep_profile_json(writer, profile)?;
            write!(writer, "}}")?;
        }
        // Both profiles are world points here, not frame-local ones, because
        // the source states them that way.
        BimBrepSurface::Ruled { first, second } => {
            write!(writer, "{{\"kind\":\"ruled\",\"first\":")?;
            write_brep_ruling_json(writer, first)?;
            write!(writer, ",\"second\":")?;
            write_brep_ruling_json(writer, second)?;
            write!(writer, "}}")?;
        }
    }
    Ok(())
}

/// Every face of one body: its surface, its outer loop and its holes, and each
/// edge's own curve. This is the whole of what [`rvt_model::brep`] recovered -
/// the counts beside it are counts of exactly these.
///
/// Written under `boundary` rather than `faces` because `faces` is already the
/// count both objects carry, and one key cannot be both.
pub(crate) fn write_brep_faces_json(writer: &mut impl Write, brep: &BimBrep) -> io::Result<()> {
    write!(writer, ",\"boundary\":[")?;
    for (face_index, face) in brep.faces.iter().enumerate() {
        let separator = if face_index > 0 { "," } else { "" };
        write!(writer, "{separator}{{\"surface\":")?;
        write_brep_surface_json(writer, &face.surface)?;
        write!(writer, ",\"loops\":[")?;
        for (loop_index, edges) in face.loops.iter().enumerate() {
            let separator = if loop_index > 0 { "," } else { "" };
            write!(writer, "{separator}[")?;
            for (edge_index, edge) in edges.iter().enumerate() {
                let separator = if edge_index > 0 { "," } else { "" };
                // Opened inline because every helper below writes its own
                // leading comma, and this is the object's first member.
                write!(
                    writer,
                    "{separator}{{\"start_meters\":[{},{},{}]",
                    json_number(edge.start.coordinates[0]),
                    json_number(edge.start.coordinates[1]),
                    json_number(edge.start.coordinates[2])
                )?;
                write_point_json(writer, "end_meters", &edge.end)?;
                match &edge.curve {
                    BimBrepCurve::Line => write!(writer, ",\"curve\":{{\"kind\":\"line\"}}")?,
                    BimBrepCurve::Arc(arc) => {
                        write!(writer, ",\"curve\":{{\"kind\":\"arc\"")?;
                        write_point_json(writer, "center_meters", &arc.center)?;
                        write_axis_json(writer, "x_axis", arc.x_axis)?;
                        write_axis_json(writer, "z_axis", arc.z_axis)?;
                        write!(
                            writer,
                            ",\"radius_meters\":{},\"start_angle\":{},\"end_angle\":{}}}",
                            json_number(arc.radius.value),
                            json_number(arc.start_angle),
                            json_number(arc.end_angle)
                        )?;
                    }
                    BimBrepCurve::Polyline(points) => {
                        write!(
                            writer,
                            ",\"curve\":{{\"kind\":\"polyline\",\"points_meters\":["
                        )?;
                        for (index, point) in points.iter().enumerate() {
                            let separator = if index > 0 { "," } else { "" };
                            write!(
                                writer,
                                "{separator}[{},{},{}]",
                                json_number(point.coordinates[0]),
                                json_number(point.coordinates[1]),
                                json_number(point.coordinates[2])
                            )?;
                        }
                        write!(writer, "]}}")?;
                    }
                }
                write!(writer, "}}")?;
            }
            write!(writer, "]")?;
        }
        write!(writer, "]}}")?;
    }
    write!(writer, "]")
}

/// Everything the decode recovered for one element that the export's own
/// selection does not carry: the body in the frame its record wrote it in,
/// the boxes it was checked against, and the centerline readings kept as
/// candidates. Written only under `--full`.
pub(crate) fn write_decoded_sections_json(
    writer: &mut impl Write,
    element: &ExportedElement,
    geometry: Option<&BimGeometry>,
) -> io::Result<()> {
    write_body_json(
        writer,
        element,
        matches!(geometry, Some(BimGeometry::Brep(_))),
    )?;
    write_bounds_json(writer, element)?;
    write_curve_candidates_json(writer, element)?;
    if let Some(spec) = &element.parameter_spec {
        write!(writer, ",\"parameter_spec\":\"{}\"", json_escape(spec))?;
    }
    Ok(())
}

/// The body decoded from this element's own `GElement` record, in the frame
/// that record wrote it in, together with the account of what the decode could
/// not read: every excluded face and failed edge with the reason given for it.
///
/// `geometry` above is the export's *selection* - one representation, chosen,
/// placed, and only when a verified route reached it. This is the decode
/// itself, so a body that is incomplete, unplaced or attached to nothing is
/// still visible here rather than silently absent.
pub(crate) fn write_body_json(
    writer: &mut impl Write,
    element: &ExportedElement,
    already_written_as_geometry: bool,
) -> io::Result<()> {
    let Some(brep) = &element.brep else {
        return Ok(());
    };
    let loops = brep
        .faces
        .iter()
        .map(|face| face.loops.len())
        .sum::<usize>();
    let (mut edges, mut arcs, mut polylines) = (0_usize, 0_usize, 0_usize);
    for edge in brep
        .faces
        .iter()
        .flat_map(|face| face.loops.iter())
        .flat_map(|edge_loop| edge_loop.iter())
    {
        edges += 1;
        arcs += usize::from(matches!(edge.curve, rvt_model::BrepCurve::Arc(_)));
        polylines += usize::from(matches!(edge.curve, rvt_model::BrepCurve::Polyline(_)));
    }
    write!(
        writer,
        ",\"body\":{{\"records\":{},\"placed\":{},\"complete\":{},\"faces\":{},\"loops\":{loops},\
         \"edges\":{edges},\"arc_edges\":{arcs},\"polyline_edges\":{polylines},\"excluded_faces\":{},\"failed_edges\":{}",
        element.brep_records,
        element.brep_is_placed,
        brep.excluded_faces.is_empty(),
        brep.faces.len(),
        brep.excluded_faces.len(),
        brep.failed_edges.len()
    )?;
    if !brep.excluded_faces.is_empty() {
        write!(writer, ",\"excluded\":[")?;
        for (index, exclusion) in brep.excluded_faces.iter().enumerate() {
            let separator = if index > 0 { "," } else { "" };
            write!(
                writer,
                "{separator}{{\"face\":{},\"reason\":\"{}\"}}",
                exclusion.face_id,
                json_escape(exclusion.reason)
            )?;
        }
        write!(writer, "]")?;
    }
    if !brep.failed_edges.is_empty() {
        write!(writer, ",\"failed\":[")?;
        for (index, failure) in brep.failed_edges.iter().enumerate() {
            let separator = if index > 0 { "," } else { "" };
            write!(
                writer,
                "{separator}{{\"edge\":{},\"reason\":\"{}\"",
                failure.edge_id,
                json_escape(failure.reason)
            )?;
            if let Some(gap) = failure
                .gap_feet
                .and_then(revit_catalog::internal_feet_to_metres)
            {
                write!(writer, ",\"gap_mm\":{}", json_number(gap * 1000.0))?;
            }
            write!(writer, "}}")?;
        }
        write!(writer, "]")?;
    }
    let residuals = element.brep_box_residuals;
    for (key, residual) in [
        ("exact", residuals.exact),
        ("graph", residuals.graph),
        ("near_duplicate", residuals.near_duplicate),
        ("graph_from_exact", residuals.graph_from_exact),
    ] {
        if let Some(residual) = residual {
            write!(
                writer,
                ",\"box_residual_{key}_feet\":{}",
                json_number(residual)
            )?;
        }
    }
    // The coordinates themselves, unless `geometry` already carried this same
    // body placed: a placed body is written there in world coordinates and
    // repeating it here would double the file for nothing.
    if already_written_as_geometry {
        write!(writer, ",\"boundary_in\":\"geometry\"")?;
    } else if let Some(local) = normalize_placed_brep(brep) {
        write!(writer, ",\"frame\":\"record\"")?;
        write_brep_faces_json(writer, &local)?;
    } else {
        // Some coordinate of this body is not finite, so no metric body can be
        // written for it. Said rather than dropped.
        write!(
            writer,
            ",\"frame\":\"record\",\"metric_conversion\":\"refused\""
        )?;
    }
    write!(writer, "}}")
}

/// The boxes this element's record declares, and what they were checked
/// against. `geometry` may carry one of these as the element's only shape;
/// these are the raw readings behind that, including for elements whose
/// geometry the export did not select.
pub(crate) fn write_bounds_json(
    writer: &mut impl Write,
    element: &ExportedElement,
) -> io::Result<()> {
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value);
    let box_json = |bounds: &GElementBounds| {
        let mut text = String::new();
        let (min, max) = (bounds.min.map(metres), bounds.max.map(metres));
        let ([Some(x0), Some(y0), Some(z0)], [Some(x1), Some(y1), Some(z1)]) = (min, max) else {
            return None;
        };
        let _ = write!(
            text,
            "{{\"min_meters\":[{x0},{y0},{z0}],\"max_meters\":[{x1},{y1},{z1}],\"offset\":{}}}",
            bounds.offset
        );
        Some(text)
    };
    for (key, bounds) in [
        ("declared_bounds", element.geometry_bounds.as_ref()),
        ("placement_bounds", element.placement_bounds.as_ref()),
        (
            "graph_bounds",
            element.geometry_graph.as_ref().map(|graph| &graph.bounds),
        ),
        (
            "tight_bounds",
            element
                .geometry_graph
                .as_ref()
                .map(|graph| &graph.tight_bounds),
        ),
    ] {
        if let Some(text) = bounds.and_then(box_json) {
            write!(writer, ",\"{key}\":{text}")?;
        }
    }
    if let Some(graph) = &element.geometry_graph {
        write!(writer, ",\"graph_nodes\":{}", graph.top_level_nodes.len())?;
    }
    if let Some(symbol) = &element.verified_symbol_bounds {
        write!(
            writer,
            ",\"verified_symbol\":{{\"id\":{}",
            symbol.symbol_element_id
        )?;
        if let Some(text) = box_json(&symbol.bounds) {
            write!(writer, ",\"bounds\":{text}")?;
        }
        write!(writer, "}}")?;
    }
    Ok(())
}

/// The centerline readings, kept as candidates rather than promoted: a pipe's
/// own line and the fitting centerlines, whether or not the export used them.
pub(crate) fn write_curve_candidates_json(
    writer: &mut impl Write,
    element: &ExportedElement,
) -> io::Result<()> {
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value);
    let segment = |start: RvtPoint3, end: RvtPoint3| {
        let (start, end) = (
            start.coordinates_feet.map(metres),
            end.coordinates_feet.map(metres),
        );
        let ([Some(x0), Some(y0), Some(z0)], [Some(x1), Some(y1), Some(z1)]) = (start, end) else {
            return None;
        };
        Some(format!(
            "\"start_meters\":[{x0},{y0},{z0}],\"end_meters\":[{x1},{y1},{z1}]"
        ))
    };
    if let Some(line) = element.pipe_line_candidate {
        if let Some(text) = segment(line.start, line.end) {
            write!(writer, ",\"pipe_line\":{{{text}")?;
            if let Some(diameter) = metres(line.nominal_diameter_feet) {
                write!(
                    writer,
                    ",\"nominal_diameter_meters\":{}",
                    json_number(diameter)
                )?;
            }
            write!(writer, ",\"offset\":{}}}", line.line_offset)?;
        }
    }
    for (key, candidate) in [
        ("fitting_center_line", element.fitting_center_line_candidate),
        ("fitting_axis", element.fitting_axis_candidate),
    ] {
        let Some(candidate) = candidate else { continue };
        if let Some(text) = segment(candidate.start, candidate.end) {
            write!(
                writer,
                ",\"{key}\":{{{text},\"owner_element_id\":{}}}",
                candidate.owner_element_id
            )?;
        }
    }
    if !element.family_instance_placement_candidates.is_empty() {
        write!(
            writer,
            ",\"family_instance_frame_candidates\":{}",
            element.family_instance_placement_candidates.len()
        )?;
    }
    Ok(())
}

/// The identifiers an element carries, each written only where it has one.
pub(crate) fn write_element_id_fields(
    writer: &mut impl Write,
    element: &ExportedElement,
) -> io::Result<()> {
    for (key, value) in [
        ("category", element.category),
        ("level_id", element.level_id),
        ("family_id", element.family_id.or(element.header_family_id)),
        ("type_id", element.type_element_id),
        ("owner_view_id", element.owner_view_id),
        ("created_phase_id", element.created_phase_id),
        ("design_option_id", element.design_option_id),
        ("unplaced_owner_id", element.unplaced_owner_id),
        ("design_option_set_id", element.design_option_set_id),
        ("main_design_option_id", element.main_design_option_id),
        ("host_id", element.host_id),
    ] {
        if let Some(value) = value {
            write!(writer, ",\"{key}\":{value}")?;
        }
    }
    Ok(())
}

pub(crate) fn write_element_json(
    writer: &mut impl Write,
    id: u32,
    element: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
    metadata: &ExportMetadata<'_>,
) -> io::Result<()> {
    let normalized = normalize_element(
        id,
        element,
        elements,
        metadata.schema,
        metadata.parameter_names,
        metadata.parameter_specs,
        metadata.catalog,
    );
    write!(writer, "{{\"id\":{id}")?;
    if let Some(class_index) = element.class_index {
        write!(writer, ",\"class_index\":{class_index}")?;
        if let Some(name) = metadata
            .schema
            .and_then(|schema| schema.class_by_index(class_index))
        {
            write!(writer, ",\"class\":\"{}\"", json_escape(&name.name))?;
        }
    }
    if let Some(source) = element.category_source {
        write!(writer, ",\"category_source\":\"{source}\"")?;
    }
    write_element_id_fields(writer, element)?;
    write_resolved_reference_names(writer, element, elements)?;
    if let Some(category) = &normalized.category {
        write!(
            writer,
            ",\"category_name\":\"{}\"",
            json_escape(&category.name)
        )?;
    }
    write_geometry_json(writer, normalized.geometry.as_ref(), metadata.full)?;
    if metadata.full {
        write_decoded_sections_json(writer, element, normalized.geometry.as_ref())?;
    }
    write_family_instance_placement(writer, element.family_instance_placement)?;
    write_ginstance_transform(writer, element.ginstance_transform)?;
    if element.moribund {
        write!(writer, ",\"moribund\":true")?;
    }
    if element.locked {
        write!(writer, ",\"locked\":true")?;
    }
    if let Some(elevation) = element.elevation_feet {
        write!(writer, ",\"elevation_internal_feet\":{elevation}")?;
        if let Some(metres) = revit_catalog::internal_feet_to_metres(elevation) {
            write!(writer, ",\"elevation_meters\":{metres}")?;
        }
    }
    if let Some((name, source)) = &element.name {
        write!(
            writer,
            ",\"name\":\"{}\",\"name_source\":\"{source}\"",
            json_escape(name)
        )?;
    }
    if !element.parameters.is_empty() {
        write!(writer, ",\"parameters\":[")?;
        write_parameters_json(
            writer,
            &element.parameters,
            &normalized.properties,
            metadata.catalog,
        )?;
        write!(writer, "]")?;
    }
    if !element.type_parameters.is_empty() {
        write!(writer, ",\"type_parameters\":[")?;
        write_parameters_json(
            writer,
            &element.type_parameters,
            &normalized.type_properties,
            metadata.catalog,
        )?;
        write!(writer, "]")?;
    }
    write_material_layers_json(writer, normalized.material_layers.as_ref())?;
    write!(writer, ",\"records\":{}", element.record_count)?;
    if let Some((partition_index, member_index, offset)) = element.source {
        if let Some(partition) = metadata.partition_paths.get(partition_index) {
            write!(
                writer,
                ",\"source\":{{\"partition\":\"{}\",\"member\":{member_index},\"offset\":{offset}}}",
                json_escape(partition)
            )?;
        }
    }
    writeln!(writer, "}}")
}

/// The layered build-up, in the order the type lists it.
///
/// `source_type_id` says which record the layers were read from when the
/// element wears its type's build-up rather than declaring one, so the two
/// cases stay apart. `function` is the source's own code and is written under
/// a name that claims nothing: nothing has established what its values mean.
pub(crate) fn write_material_layers_json(
    writer: &mut impl Write,
    layers: Option<&BimMaterialLayerSet>,
) -> io::Result<()> {
    let Some(set) = layers else {
        return Ok(());
    };
    if set.layers.is_empty() {
        return Ok(());
    }
    write!(
        writer,
        ",\"material_layers\":{{\"count\":{}",
        set.layers.len()
    )?;
    if let Some(total) = set.total_thickness() {
        write!(writer, ",\"total_thickness\":{}", json_number(total.value))?;
    }
    if let Some(id) = &set.source_type_id {
        write!(writer, ",\"source_type_id\":{}", json_identifier(&id.0))?;
    }
    write!(writer, ",\"unit\":\"metre\",\"layers\":[")?;
    for (index, layer) in set.layers.iter().enumerate() {
        let separator = if index > 0 { "," } else { "" };
        write!(
            writer,
            "{separator}{{\"index\":{index},\"thickness\":{}",
            json_number(layer.thickness.value)
        )?;
        if let Some(material) = &layer.material {
            if let Some(id) = &material.id {
                write!(writer, ",\"material_id\":{}", json_identifier(&id.value))?;
            }
            if let Some(name) = &material.name {
                write!(writer, ",\"material\":\"{}\"", json_escape(name))?;
            }
        }
        if layer.is_core {
            write!(writer, ",\"core\":true")?;
        }
        if layer.is_structural {
            write!(writer, ",\"structural\":true")?;
        }
        if let Some(function) = layer.source_function {
            write!(writer, ",\"source_function\":{function}")?;
        }
        write!(writer, "}}")?;
    }
    write!(writer, "]}}")
}

pub(crate) fn write_geometry_json(
    writer: &mut impl Write,
    geometry: Option<&BimGeometry>,
    full: bool,
) -> io::Result<()> {
    match geometry {
        Some(BimGeometry::SweptDisk(swept_disk)) => {
            let start = swept_disk.directrix.start.coordinates;
            let end = swept_disk.directrix.end.coordinates;
            write!(
                writer,
                ",\"geometry\":{{\"kind\":\"swept_disk\",\"start_meters\":[{},{},{}],\"end_meters\":[{},{},{}],\"radius_meters\":{}}}",
                start[0], start[1], start[2], end[0], end[1], end[2], swept_disk.radius.value
            )
        }
        Some(BimGeometry::AxisLine(line)) => {
            let start = line.start.coordinates;
            let end = line.end.coordinates;
            write!(
                writer,
                ",\"geometry\":{{\"kind\":\"axis_line\",\"start_meters\":[{},{},{}],\"end_meters\":[{},{},{}]}}",
                start[0], start[1], start[2], end[0], end[1], end[2]
            )
        }
        Some(BimGeometry::BoundingBox(bounds)) => {
            let min = bounds.min.coordinates;
            let max = bounds.max.coordinates;
            write!(
                writer,
                ",\"geometry\":{{\"kind\":\"bounding_box\",\"min_meters\":[{},{},{}],\"max_meters\":[{},{},{}]}}",
                min[0], min[1], min[2], max[0], max[1], max[2]
            )
        }
        // Each member of an assembly is a body in its own right, so the dump
        // states them as the bodies they are rather than inventing a shape
        // that holds them.
        Some(BimGeometry::Assembly(parts)) => {
            write!(writer, ",\"geometry\":{{\"kind\":\"assembly\",\"bodies\":[")?;
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    write!(writer, ",")?;
                }
                write!(writer, "{{\"faces\":{}", part.faces.len())?;
                write!(writer, ",\"closed\":{}}}", part.complete)?;
            }
            write!(writer, "]}}")
        }
        Some(BimGeometry::Brep(brep)) => {
            let (mut lines, mut arcs, mut polylines) = (0_usize, 0_usize, 0_usize);
            for edge in brep
                .faces
                .iter()
                .flat_map(|face| face.loops.iter())
                .flat_map(|edge_loop| edge_loop.iter())
            {
                match &edge.curve {
                    BimBrepCurve::Line => lines += 1,
                    BimBrepCurve::Arc(_) => arcs += 1,
                    BimBrepCurve::Polyline(_) => polylines += 1,
                }
            }
            write!(
                writer,
                ",\"geometry\":{{\"kind\":\"brep\",\"faces\":{},\"complete\":{},\"line_edges\":{lines},\"arc_edges\":{arcs},\"polyline_edges\":{polylines}",
                brep.faces.len(),
                brep.complete
            )?;
            if full {
                write_brep_faces_json(writer, brep)?;
            }
            write!(writer, "}}")
        }
        None => Ok(()),
    }
}

pub(crate) fn write_family_instance_placement(
    writer: &mut impl Write,
    placement: Option<FamilyInstancePlacementFields>,
) -> io::Result<()> {
    let Some(placement) = placement else {
        return Ok(());
    };
    let [Some(origin_x), Some(origin_y), Some(origin_z)] = placement
        .origin
        .coordinates_feet
        .map(revit_catalog::internal_feet_to_metres)
    else {
        return Ok(());
    };
    write!(
        writer,
        ",\"family_instance_frame\":{{\"origin_meters\":[{origin_x},{origin_y},{origin_z}],\"reference_direction\":[{},{},{}],\"axis\":[{},{},{}]}}",
        placement.reference_direction[0],
        placement.reference_direction[1],
        placement.reference_direction[2],
        placement.axis[0],
        placement.axis[1],
        placement.axis[2]
    )
}

pub(crate) fn write_ginstance_transform(
    writer: &mut impl Write,
    transform: Option<GInstanceTransformFields>,
) -> io::Result<()> {
    let Some(transform) = transform else {
        return Ok(());
    };
    let [Some(origin_x), Some(origin_y), Some(origin_z)] = transform
        .origin
        .coordinates_feet
        .map(revit_catalog::internal_feet_to_metres)
    else {
        return Ok(());
    };
    let [x, y, z] = transform.basis;
    let symbol = transform
        .symbol_element_id
        .map_or_else(String::new, |id| format!(",\"symbol_element_id\":{id}"));
    write!(
        writer,
        ",\"ginstance_transform\":{{\"origin_meters\":[{origin_x},{origin_y},{origin_z}],\"basis\":[[{},{},{}],[{},{},{}],[{},{},{}]]{symbol}}}",
        x[0], x[1], x[2], y[0], y[1], y[2], z[0], z[1], z[2]
    )
}

pub(crate) fn write_parameters_json(
    writer: &mut impl Write,
    parameters: &[rvt_model::Parameter],
    properties: &[BimProperty],
    catalog: Option<Catalog>,
) -> io::Result<()> {
    for (index, (parameter, property)) in parameters.iter().zip(properties).enumerate() {
        if index > 0 {
            write!(writer, ",")?;
        }
        write!(
            writer,
            "{{\"id\":{},\"name\":\"{}\"",
            parameter.id,
            json_escape(&property.name)
        )?;
        if let Some(parameter) =
            catalog.and_then(|catalog| catalog.built_in_parameter(parameter.id))
        {
            write!(
                writer,
                ",\"built_in\":\"{}\"",
                json_escape(parameter.enum_name)
            )?;
        }
        if let Some(spec) = &property.specification {
            write!(writer, ",\"spec\":\"{}\"", json_escape(spec))?;
        }
        write_parameter_value_json(writer, &parameter.value, &property.value)?;
        write!(writer, "}}")?;
    }
    Ok(())
}

pub(crate) fn write_parameter_value_json(
    writer: &mut impl Write,
    source: &ParameterValue,
    normalized: &BimPropertyValue,
) -> io::Result<()> {
    match source {
        ParameterValue::Double(value) => {
            write!(writer, ",\"double\":{value}")?;
            if let BimPropertyValue::Number(number) = normalized {
                if let Some(unit) = &number.unit {
                    write!(
                        writer,
                        ",\"storage_value\":{},\"unit\":\"{}\",\"unit_name\":\"{}\"",
                        number.value,
                        json_escape(&unit.id),
                        json_escape(&unit.name)
                    )?;
                }
            }
        }
        ParameterValue::Integer(value) => write!(writer, ",\"int\":{value}")?,
        ParameterValue::Text(value) => {
            write!(writer, ",\"text\":\"{}\"", json_escape(value))?;
        }
        ParameterValue::Reference(value) => write!(writer, ",\"ref\":{value}")?,
    }
    Ok(())
}

pub(crate) fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(escaped, "\\u{:04x}", control as u32);
            }
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_references_that_name_something_to_that_name() {
        let named = |name: &str| ExportedElement {
            name: Some((name.to_owned(), "declared")),
            ..ExportedElement::default()
        };
        let mut elements = BTreeMap::new();
        elements.insert(10, named("-01 Подвал"));
        elements.insert(20, named("Basic Wall: 200mm"));
        elements.insert(30, named("Отверстие (ниша)"));
        // The referenced element that carries no name resolves to nothing
        // rather than to a placeholder.
        elements.insert(40, ExportedElement::default());
        elements.insert(
            1,
            ExportedElement {
                level_id: Some(10),
                type_element_id: Some(20),
                family_id: Some(30),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            2,
            ExportedElement {
                level_id: Some(40),
                ..ExportedElement::default()
            },
        );

        let metadata = ExportMetadata {
            schema: None,
            parameter_names: &BTreeMap::new(),
            parameter_specs: &BTreeMap::new(),
            catalog: None,
            partition_paths: &[],
            full: false,
        };
        let render = |id: u32| {
            let mut bytes = Vec::new();
            write_element_json(&mut bytes, id, &elements[&id], &elements, &metadata).unwrap();
            String::from_utf8(bytes).unwrap()
        };

        let line = render(1);
        assert!(line.contains(r#""level_id":10"#), "{line}");
        assert!(line.contains(r#""level_name":"-01 Подвал""#), "{line}");
        assert!(
            line.contains(r#""type_name":"Basic Wall: 200mm""#),
            "{line}"
        );
        assert!(
            line.contains(r#""family_name":"Отверстие (ниша)""#),
            "{line}"
        );

        // A reference whose target has no name keeps the identifier alone.
        let line = render(2);
        assert!(line.contains(r#""level_id":40"#), "{line}");
        assert!(!line.contains("level_name"), "{line}");
    }
}
