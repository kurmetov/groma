//! The drivers behind `export-json`, `export-ifc` and `export-scene`.
//!
//! Each one reads its sources through [`crate::source`], hands the model to a
//! writer crate, and reports what happened. None of them parses anything.

use std::{
    error::Error,
    fs::File,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bim_mesh::MeshOptions;
use ifc_export::{LengthUnit, MetadataOptions, metadata_ifc, uuid_v5};
// The whole semantic reconstruction moved into `rvt-import`. Glob-imported
// because the probe commands below read the same intermediate the pipeline
// builds, and naming each item here would be a second list to keep in step.
#[allow(clippy::wildcard_imports)] // The binary's own constants.
use crate::*;
#[allow(clippy::wildcard_imports)]
// Sibling modules of one binary; naming each item would be a second list to keep in step.
use crate::{json::*, source::*};
use rvt_import::recover_elements;
use scene_pack::{PackOptions, write_scene};

pub(crate) fn export_json(
    path: &Path,
    output: Option<&Path>,
    limit: Option<usize>,
    full: bool,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let recovered = recover_elements(path, max_member_bytes)?;
    let writer: Box<dyn Write> = match output {
        Some(output) => Box::new(BufWriter::new(File::create(output)?)),
        None => Box::new(io::stdout().lock()),
    };
    let metadata = ExportMetadata {
        schema: recovered.schema.as_ref(),
        partition_paths: &recovered.partition_paths,
        parameter_names: &recovered.parameter_names,
        parameter_specs: &recovered.parameter_specs,
        catalog: recovered.catalog,
        full,
    };
    let written = write_exported_elements(writer, path, &recovered, &metadata, limit)?;
    if output.is_some() {
        println!(
            "Elements written: {written} of {}",
            recovered.elements.len()
        );
    }
    Ok(())
}

/// The export setup a run asks for: a saved file, and whatever the flags say
/// on top of it. Kept together so the dispatch and the exporter do not each
/// grow five arguments that only travel together.
// One field per flag, on purpose: this is the list of what `export-ifc`
// accepts, and keeping it flat is what makes it readable beside `--help`.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct IfcSettingsArguments<'a> {
    pub(crate) settings: Option<&'a Path>,
    pub(crate) length_unit: Option<LengthUnitArgument>,
    pub(crate) no_revit_property_sets: bool,
    pub(crate) no_revit_type_property_sets: bool,
    pub(crate) no_ifc_common_property_sets: bool,
    pub(crate) base_quantities: bool,
    pub(crate) no_types: bool,
    pub(crate) class_mapping: Option<&'a Path>,
    pub(crate) write_settings: Option<&'a Path>,
}

/// The length units the exporter writes, as a flag. A mirror of
/// [`LengthUnit`], because the exporter does not depend on the argument
/// parser.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum LengthUnitArgument {
    Metre,
    Millimetre,
}

impl From<LengthUnitArgument> for LengthUnit {
    fn from(unit: LengthUnitArgument) -> Self {
        match unit {
            LengthUnitArgument::Metre => Self::Metre,
            LengthUnitArgument::Millimetre => Self::Millimetre,
        }
    }
}

/// The knobs `export-scene` exposes, kept together so that neither the
/// dispatch nor the exporter grows an argument list nobody can read.
pub(crate) struct SceneArguments {
    pub(crate) chord_tolerance_mm: f64,
    pub(crate) refinement_depth: u8,
    pub(crate) chunk_triangles: usize,
    pub(crate) property_block: usize,
    pub(crate) compression: u32,
}

/// Build the binary scene a viewer loads, from whatever formats the files are
/// in.
///
/// One function for every source: the format is settled by [`read_source`] and
/// nothing below it needs to know the answer. This used to be two nearly
/// identical functions - one per format - which is how the two came to report
/// the same facts in different words.
pub(crate) fn export_scene(
    paths: &[PathBuf],
    output: Option<&Path>,
    include_unplaced: bool,
    limit: Option<usize>,
    arguments: &SceneArguments,
    progress: bool,
    max_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let output = default_output(paths, output, "rvs")?;
    let options = pack_options(arguments)?;
    // Three stages, timed separately because they fail and scale for
    // different reasons: reading the sources and walking their records, then
    // selecting and typing the model, then tessellating and packing it. A
    // caller driving a progress display needs them apart, and so does anyone
    // asking where a slow conversion went.
    let mut stage = Stage::new(progress);
    let conversion = read_sources(
        paths,
        &ReadOptions {
            include_unplaced,
            limit,
            max_bytes,
            chord_tolerance: arguments.chord_tolerance_mm / 1000.0,
        },
        &mut stage,
    )?;

    stage.begins("tessellate");
    let mut writer = BufWriter::new(File::create(&output)?);
    let stats = write_scene(&conversion.model, &conversion.info(), &options, &mut writer)?;
    writer.flush()?;
    stage.finished("tessellate");

    println!("Scene written: {}", output.display());
    conversion.report_read();
    conversion.report_federation();
    println!("Building storeys: {}", conversion.model.levels.len());
    println!(
        "Elements: {} ({} carry geometry)",
        stats.elements, stats.elements_with_geometry
    );
    conversion.report_model();
    println!(
        "Triangles: {} across {} vertices in {} chunks",
        stats.triangles, stats.vertices, stats.chunks
    );
    println!("Declared edges: {}", stats.edges);
    if stats.skipped_faces > 0 {
        // A face the tessellator declined is reported rather than replaced by
        // a box, so that the count here and the geometry in the file always
        // describe the same thing.
        println!(
            "Faces the tessellator could not read: {}",
            stats.skipped_faces
        );
    }
    println!("Bytes: {} ({})", stats.bytes, describe_bytes(stats.bytes));
    if stats.triangles > 0 {
        #[allow(clippy::cast_precision_loss)]
        // Both counts are bounded by the file that was just written.
        let per_triangle = stats.bytes as f64 / stats.triangles as f64;
        println!("Bytes per triangle: {per_triangle:.2}");
    }
    conversion.report_properties();
    stage.total();
    Ok(())
}

pub(crate) fn export_ifc(
    paths: &[PathBuf],
    output: Option<&Path>,
    model_namespace: Option<&str>,
    include_unplaced: bool,
    limit: Option<usize>,
    settings_arguments: &IfcSettingsArguments<'_>,
    max_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let settings = settings_arguments.resolve()?;
    if let Some(path) = settings_arguments.write_settings {
        settings.to_json_file(path)?;
    }
    let output = default_output(paths, output, "ifc")?;

    let mut stage = Stage::new(false);
    let conversion = read_sources(
        paths,
        &ReadOptions {
            include_unplaced,
            limit,
            max_bytes,
            chord_tolerance: DEFAULT_CHORD_TOLERANCE_MM / 1000.0,
        },
        &mut stage,
    )?;

    let namespace = if let Some(value) = model_namespace {
        parse_uuid(value)?
    } else {
        source_path_namespace(paths)?
    };
    let (creation_time, timestamp) = current_utc_timestamp()?;
    // A federated set has no one source file to name the project after, so
    // the output's own name stands for it.
    let project_name = if paths.len() == 1 { &paths[0] } else { &output }
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Rivet Project")
        .to_owned();
    let level_count = conversion.model.levels.len();
    let element_count = conversion.model.elements.len();
    let geometry_count = conversion
        .model
        .elements
        .iter()
        .filter(|element| element.geometry.is_some())
        .count();
    let options = MetadataOptions {
        model_namespace: namespace,
        file_name: output.to_string_lossy().into_owned(),
        timestamp,
        creation_time,
        project_name,
        site_name: "Site".to_owned(),
        building_name: "Building".to_owned(),
        settings,
    };
    let file = metadata_ifc(&conversion.model, &options)?;
    let mut writer = File::create(&output)?;
    file.write_to(&mut writer)?;
    writer.flush()?;

    println!("IFC written: {}", output.display());
    println!("Model namespace: {}", format_uuid(namespace));
    println!("Length unit: {}", options.settings.length_unit);
    if let Some(mapping) = options.settings.class_mapping() {
        println!("Class mapping rows: {}", mapping.len());
    }
    conversion.report_read();
    conversion.report_federation();
    println!("Building storeys: {level_count}");
    println!("Elements: {element_count}");
    println!("Elements with verified geometry: {geometry_count}");
    conversion.report_model();
    conversion.report_geometry_funnels();
    conversion.report_properties();
    Ok(())
}

/// Where an export writes, and the refusals that have to happen before a long
/// conversion is paid for.
///
/// A single source names its own output by extension, which is what every
/// caller has always relied on. Several sources have no such name - and the
/// default for one IFC source would be the source itself - so `--output` is
/// required rather than guessed at.
pub(crate) fn default_output(
    paths: &[PathBuf],
    output: Option<&Path>,
    extension: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let refuse = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    let output = match (output, paths) {
        (Some(output), _) => output.to_path_buf(),
        (None, [only]) => only.with_extension(extension),
        (None, _) => {
            return Err(refuse(format!(
                "name the {} to write with --output: {} source files have no one name to \
                 derive it from",
                extension.to_uppercase(),
                paths.len()
            ))
            .into());
        }
    };
    for path in paths {
        if same_existing_file(path, &output)? {
            // Reached without `--output` when the source's own extension is
            // the output's, which is what one IFC source does.
            return Err(refuse(format!(
                "output must not overwrite the source file {}; name another with --output",
                path.display()
            ))
            .into());
        }
    }
    Ok(output)
}

/// A model namespace derived from the sources' canonical paths.
///
/// Every path contributes, separated by a byte no path can contain, so that
/// federating the same set twice yields the same identifiers and federating a
/// different set does not.
pub(crate) fn source_path_namespace(paths: &[PathBuf]) -> Result<[u8; 16], Box<dyn Error>> {
    let mut name = Vec::new();
    for path in paths {
        let canonical = std::fs::canonicalize(path)?;
        name.extend_from_slice(canonical.as_os_str().as_encoded_bytes());
        name.push(0);
    }
    Ok(uuid_v5(SOURCE_PATH_NAMESPACE, &name))
}

pub(crate) fn pack_options(arguments: &SceneArguments) -> Result<PackOptions, io::Error> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    if !(arguments.chord_tolerance_mm.is_finite() && arguments.chord_tolerance_mm > 0.0) {
        return Err(invalid(
            "chord tolerance must be a positive number of millimetres",
        ));
    }
    if arguments.chunk_triangles == 0 {
        return Err(invalid("a chunk must hold at least one triangle"));
    }
    if arguments.property_block == 0 {
        return Err(invalid("a property block must hold at least one element"));
    }
    if arguments.compression > 9 {
        return Err(invalid("compression must be between 0 and 9"));
    }
    Ok(PackOptions {
        mesh: MeshOptions {
            chord_tolerance: arguments.chord_tolerance_mm / 1000.0,
            refinement_depth: arguments.refinement_depth,
            ..MeshOptions::default()
        },
        chunk_triangle_budget: arguments.chunk_triangles,
        property_block: arguments.property_block,
        compression: arguments.compression,
    })
}

/// A byte count in the largest unit that keeps it above one.
pub(crate) fn describe_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    // The count is a file length; the scale it is divided by is exact.
    let mut value = bytes as f64;
    for unit in ["B", "KiB", "MiB"] {
        if value < 1024.0 {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1} GiB")
}

pub(crate) fn same_existing_file(left: &Path, right: &Path) -> io::Result<bool> {
    if left == right {
        return Ok(true);
    }
    if !right.exists() {
        return Ok(false);
    }
    Ok(std::fs::canonicalize(left)? == std::fs::canonicalize(right)?)
}

pub(crate) fn parse_uuid(value: &str) -> Result<[u8; 16], io::Error> {
    let digits = value
        .bytes()
        .filter(|byte| *byte != b'-')
        .collect::<Vec<_>>();
    if digits.len() != 32 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "model namespace must be a UUID (32 hexadecimal digits, with optional hyphens)",
        ));
    }
    let mut uuid = [0_u8; 16];
    for (target, pair) in uuid.iter_mut().zip(digits.chunks_exact(2)) {
        let text = std::str::from_utf8(pair).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid UUID: {error}"),
            )
        })?;
        *target = u8::from_str_radix(text, 16).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid UUID: {error}"),
            )
        })?;
    }
    Ok(uuid)
}

pub(crate) fn format_uuid(uuid: [u8; 16]) -> String {
    let hex = uuid.map(|byte| format!("{byte:02x}")).concat();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub(crate) fn current_utc_timestamp() -> Result<(i64, String), io::Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            io::Error::other(format!("system clock is before the Unix epoch: {error}"))
        })?;
    let seconds = i64::try_from(elapsed.as_secs())
        .map_err(|_| io::Error::other("current time does not fit an IFC timestamp"))?;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    Ok((
        seconds,
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"),
    ))
}

/// Gregorian date for a day offset from 1970-01-01.
pub(crate) fn civil_date_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
