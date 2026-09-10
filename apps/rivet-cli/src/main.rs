#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::Write as _,
    fs::File,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use bim_convert::Format;
use bim_core::{
    BimBrep, BimBrepCurve, BimBrepProfile, BimBrepRuling, BimBrepSurface, BimDocument,
    BimDocumentId, BimFederationReport, BimGeometry, BimMaterialLayerSet, BimModel, BimPoint3,
    BimProperty, BimPropertyValue,
};
use bim_mesh::MeshOptions;
use clap::{Parser, Subcommand};
use ifc_export::{ExportSettings, LengthUnit, MetadataOptions, metadata_ifc, uuid_v5};
use revit_catalog::Catalog;
use rvt_container::{
    DEFAULT_DECODE_LIMIT, MarkerEnvelope, MarkerEnvelopeOptions, PartitionReadOptions,
    RvtContainer, StreamFraming,
};
use rvt_model::{
    ELEMENT_TAIL_BYTES, ElementAnchor, ElementFields, ElementHeaderFields,
    FamilyInstancePlacementFields, GElementBounds, GElementGraphFields, GInstanceTransformFields,
    MemberWalk, ParameterSets, ParameterValue, RECORD_LENGTH_TRAILER_BYTES, RecordFraming,
    RecordHeader, RecordLayout, RvtPoint3,
};
use rvt_schema::{Schema, TypeReference};
// The whole semantic reconstruction moved into `rvt-import`. Glob-imported
// because the probe commands below read the same intermediate the pipeline
// builds, and naming each item here would be a second list to keep in step.
use rvt_import::{
    BODY_BOUNDS_TOLERANCE_FEET, BOX_GAP_BUCKETS, ClassGeometry, ELEMENT_CLASS_FORMAT_TAG,
    ELEMENT_HEADER_CLASS, ExportedElement, FACE_INSIDE_THE_BOX_FLAG, GFaceMarks,
    GeometryStatistics, NAME_OFFSET_AGREEMENT, NameCalibration, RecoveredElements,
    UNRESOLVED_CLASS, body_bounds_residual_feet, body_extent_feet, body_is_placed_in,
    brep_body_classes, brep_class_indexes, calibrate_names, decode_elem_table_stream,
    decode_schema_stream, elem_table_ids, faces_extent_feet, for_each_member, geometry_statistics,
    mapped_family_instance_placement_counts, metadata_model, normalize_element,
    normalize_placed_brep, parameter_set_class_indexes, partition_paths, read_basic_file_info,
    read_name, read_schema, recover_elements, schema_class_index, schema_class_is_a,
    tally_class_geometry,
};
use scene_pack::{PackOptions, SourceInfo, write_scene};

/// The furthest a triangle may sit from the surface it approximates, in
/// millimetres, unless a caller says otherwise.
///
/// Held once because two commands need the same answer: `export-scene` takes
/// it as a flag, and `export-ifc` has no flag for it but still drives a
/// reader that tessellates a curve to build its model.
const DEFAULT_CHORD_TOLERANCE_MM: f64 = 4.0;
/// Leading bytes summarized per envelope when looking for a record header.
const LEADING_PATTERN_BYTES: usize = 8;
/// Rows printed for each record-boundary histogram.
const HISTOGRAM_ROWS: usize = 8;
/// Strides tested when checking whether a marker is one element of an
/// ascending little-endian `u32` sequence rather than a record boundary.
const SEQUENCE_STRIDES: [usize; 4] = [4, 8, 12, 16];
/// Largest object identifier accepted when scanning a `GElement` body for
/// nested node references. Top-level identifiers stay far below this.
const NESTED_NODE_IDENTIFIER_LIMIT: u32 = 4_096;
/// Largest decoded member observed in the corpus; a record that runs past a
/// member of exactly this size is the continuation candidate.
const MEMBER_PAGE_LIMIT_BYTES: u64 = 128 * 1024;
/// `UUIDv5` namespace used only to turn a canonical source path into a model
/// namespace. Users can supply a persistent namespace explicitly when a model
/// may move between paths.
const SOURCE_PATH_NAMESPACE: [u8; 16] = [
    0x76, 0x26, 0xfd, 0xf2, 0xc2, 0xad, 0x51, 0xd0, 0xb9, 0x1f, 0xc6, 0xcc, 0x07, 0x36, 0x0c, 0x62,
];

const KNOWN_STREAMS: [&str; 4] = [
    "BasicFileInfo",
    "Formats/Latest",
    "Global/ElemTable",
    "Global/Latest",
];

#[derive(Debug, Parser)]
#[command(name = "rivet", version, about = "Read-only RVT inspection")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show a concise container and release summary.
    Info { file: PathBuf },
    /// List every physical stream and its size.
    Streams { file: PathBuf },
    /// Copy one raw stream to stdout or a file.
    DumpStream {
        file: PathBuf,
        stream: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Inventory compressed members in every Partitions/* stream.
    Partitions {
        file: PathBuf,
        /// Print offsets and sizes for every validated member.
        #[arg(long)]
        members: bool,
        /// Maximum decoded bytes accepted from one gzip member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_decoded_bytes: u64,
        /// Maximum decoded bytes accepted from one partition.
        #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024)]
        max_partition_decoded_bytes: u64,
        /// Maximum gzip signatures retained while scanning one partition.
        #[arg(long, default_value_t = 1_000_000)]
        max_candidates: usize,
    },
    /// Decode and list the generic class schema.
    Schema {
        file: PathBuf,
        /// Print the property list of one exact class instead of the inventory.
        #[arg(long)]
        class: Option<String>,
        /// List every declared property whose name contains this text, with the
        /// class that declares it, instead of the inventory.
        #[arg(long)]
        property: Option<String>,
    },
    /// Decode the Global/ElemTable element-id index.
    ElemTable {
        file: PathBuf,
        /// Print candidate element identifiers from every parsed record.
        #[arg(long)]
        records: bool,
    },
    /// Probe whether partition-member first words correlate with element IDs.
    PartitionIdProbe { file: PathBuf },
    /// Count schema-index/zero prefixes inside decoded partition members.
    SchemaPrefixProbe {
        file: PathBuf,
        /// Exact schema class name whose index should be probed.
        #[arg(long, default_value = "GElement")]
        class: String,
    },
    /// Capture bounded byte context around schema-marker candidates.
    MarkerEnvelopes {
        file: PathBuf,
        /// Exact schema class name whose marker is captured.
        #[arg(long, default_value = "GElement")]
        class: String,
        /// Bytes retained before each marker.
        #[arg(long, default_value_t = 32)]
        leading: usize,
        /// Bytes retained after each marker.
        #[arg(long, default_value_t = 64)]
        trailing: usize,
        /// Envelopes retained per partition stream.
        #[arg(long, default_value_t = 256)]
        max_envelopes: usize,
        /// Hex-dump this many captured envelopes.
        #[arg(long, default_value_t = 0)]
        dump: usize,
    },
    /// Validate member descriptors and the record array inside each member.
    MemberFraming {
        file: PathBuf,
        /// Restrict the walk to one partition stream.
        #[arg(long)]
        partition: Option<String>,
        /// Maximum decoded bytes accepted from one RVT member, or source bytes
        /// accepted from an IFC file.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
        /// Print per-record offsets for this many members.
        #[arg(long, default_value_t = 0)]
        dump: usize,
        /// Print this many members whose body total disagrees with `+28`.
        #[arg(long, default_value_t = 0)]
        mismatches: usize,
    },
    /// Cross-check record headers with the Global/ElemTable candidate IDs.
    Records {
        file: PathBuf,
        /// Restrict the walk to one partition stream.
        #[arg(long)]
        partition: Option<String>,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
        /// Print this many record headers as hex.
        #[arg(long, default_value_t = 0)]
        dump: usize,
    },
    /// List every record belonging to one element identifier.
    Element {
        file: PathBuf,
        /// Element identifier, as reported by `records` or `elem-table`.
        id: u32,
        /// Print this many leading body bytes of each record as hex.
        #[arg(long, default_value_t = 0)]
        bytes: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Show the parameters stored on elements.
    Parameters {
        file: PathBuf,
        /// Only elements of this exact schema class.
        #[arg(long)]
        class: Option<String>,
        /// Stop after this many elements.
        #[arg(long, default_value_t = 12)]
        count: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Tally what the boundary-representation assembly resolves and excludes.
    Brep {
        file: PathBuf,
        /// Print this many excluded-face reasons, most frequent first.
        #[arg(long, default_value_t = 12)]
        reasons: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Score every mark a `GFace` declares against the one answer that is
    /// independent of it: whether the face lies inside the box its own record
    /// carries.
    FaceMarkProbe {
        file: PathBuf,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Tally the decoded bodies by the class of the element that owns them,
    /// and by whether anything carries them out to the export.
    BodyOwners {
        file: PathBuf,
        /// Print this many classes, most bodies first.
        #[arg(long, default_value_t = 24)]
        classes: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Report the layer table compound host object types carry, and score each
    /// candidate type-reference property against the class of what it names.
    Layers {
        file: PathBuf,
        /// Print this many decoded layer tables in full, most layers first.
        #[arg(long, default_value_t = 20)]
        types: usize,
        /// Also print one line per element carrying a type reference, so the
        /// links can be joined against an independent answer.
        #[arg(long)]
        links: bool,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Calibrate where each class keeps its first readable string.
    Names {
        file: PathBuf,
        /// Print this many classes, most frequent first.
        #[arg(long, default_value_t = 24)]
        classes: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Export the recovered element records as JSON lines.
    ExportJson {
        file: PathBuf,
        /// Write to this path instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Stop after this many elements.
        #[arg(long)]
        limit: Option<usize>,
        /// Write every decoded section rather than the export's selection: a
        /// leading model line indexing the file, and per element the whole
        /// decoded body - every face, loop and edge - the boxes it was checked
        /// against, and the faces and edges the decode could not read.
        #[arg(long)]
        full: bool,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Walk record bodies of one class against the schema declarations and
    /// report how much of each body the declared properties explain.
    SerialProbe {
        file: PathBuf,
        /// Schema class whose records are walked.
        #[arg(long, default_value = "GElement")]
        class: String,
        /// Keep walking the objects the record's references name, in order,
        /// instead of stopping after the declared properties.
        #[arg(long)]
        stream: bool,
        /// Read whole records: declared properties, the node stream, and the
        /// trailing length word.
        #[arg(long)]
        record: bool,
        /// Print this many rows of each histogram.
        #[arg(long, default_value_t = 12)]
        rows: usize,
        /// Dump the bytes the declarations did not explain, for records whose
        /// leftover is exactly this many bytes.
        #[arg(long)]
        dump_remaining: Option<usize>,
        /// Print every property read for the record with this identifier.
        #[arg(long)]
        trace_id: Option<u32>,
        /// Print the whole body of the record with this identifier as hex.
        #[arg(long)]
        dump_body: Option<u32>,
        /// Report each record of this element separately instead of totals.
        #[arg(long)]
        element: Option<u32>,
        /// Dump bytes around the stop of records whose stop description
        /// contains this text.
        #[arg(long)]
        dump_stop: Option<String>,
        /// Bytes shown either side of a dumped stop.
        #[arg(long, default_value_t = 24)]
        dump_window: usize,
        /// Stop dumping after this many records.
        #[arg(long, default_value_t = 8)]
        dump_count: usize,
        /// Restrict every tally to records that carry boundary faces.
        #[arg(long)]
        faces_only: bool,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Label the width of every checkable `GInfo.m_flags` from the bytes that
    /// follow it, and correlate the label against the header's candidate
    /// discriminators.
    FlagsProbe {
        file: PathBuf,
        /// Schema class whose records are walked.
        #[arg(long, default_value = "GElement")]
        class: String,
        /// Print this many rows of each histogram.
        #[arg(long, default_value_t = 12)]
        rows: usize,
        /// Restrict every tally to records that carry boundary faces.
        #[arg(long)]
        faces_only: bool,
        /// Keep samples from every record, not only from records the
        /// oracle-driven walk explained exactly.
        #[arg(long)]
        every_record: bool,
        /// Report only the objects of this node class, and add a histogram of
        /// the raw words from `m_flags` onwards.
        #[arg(long)]
        node_class: Option<String>,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Probe the identifiers a record names and writes no object for.
    IdentifierProbe {
        file: PathBuf,
        /// Print this many rows of each histogram.
        #[arg(long, default_value_t = 20)]
        rows: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Probe how a `Face` reaches its `EdgeLoop`: by the reference the face
    /// carries, or by the `pFace` every loop declares.
    LoopOwnerProbe {
        file: PathBuf,
        /// Print this many rows of each histogram.
        #[arg(long, default_value_t = 20)]
        rows: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Probe the `GElement` node graph: which node classes hang under it and
    /// where their object identifiers resolve.
    GeometryGraphProbe {
        file: PathBuf,
        /// Print this many rows of each histogram.
        #[arg(long, default_value_t = 20)]
        rows: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Census every `RuledSurf`: which curve classes its two profiles name,
    /// when it falls back to a declared point, and how its faces pair to it.
    RuledSurfProbe {
        file: PathBuf,
        /// Print this many rows of each histogram.
        #[arg(long, default_value_t = 20)]
        rows: usize,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Export an IFC4 spatial tree, typed elements, and verified geometry.
    ///
    /// Several source files are read as one federated model: every identifier
    /// is qualified by the file it came from, and coordinates are left exactly
    /// as each file stated them.
    ExportIfc {
        #[arg(required = true, num_args = 1..)]
        files: Vec<PathBuf>,
        /// Write to this path instead of replacing the source's extension
        /// with `.ifc`. Required where several sources are given.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Stable model namespace UUID. By default it is derived from the canonical RVT path.
        #[arg(long)]
        model_namespace: Option<String>,
        /// Include categorized records without a recovered level association.
        #[arg(long)]
        include_unplaced: bool,
        /// Stop after this many exported elements.
        #[arg(long)]
        limit: Option<usize>,
        /// Read the export setup from this JSON file. Every flag below
        /// overrides what the file says, so a saved setup can be reused with
        /// one thing changed.
        #[arg(long)]
        settings: Option<PathBuf>,
        /// The unit every length in the file is written in.
        #[arg(long, value_enum)]
        length_unit: Option<LengthUnitArgument>,
        /// Leave out the element's own Revit parameters.
        #[arg(long)]
        no_revit_property_sets: bool,
        /// Leave out the parameters the element's type carries.
        #[arg(long)]
        no_revit_type_property_sets: bool,
        /// Write IFC's own `Qto_..BaseQuantities`, measured from the solid
        /// this file carries: `NetVolume`, and `NetSurfaceArea` where the
        /// entity's quantity set has a name for it. Only a closed shell of
        /// planar faces is measured; nothing else is estimated.
        #[arg(long)]
        base_quantities: bool,
        /// A class mapping table in Revit's tab-separated form: a category,
        /// an empty subcategory column, the IFC class to write it as, and the
        /// predefined type. `Not Exported` keeps a category out of the file.
        /// The category is named by its `BuiltInCategory` - `OST_Walls` or
        /// `-2000011` - which is what a decoded model carries.
        #[arg(long)]
        class_mapping: Option<PathBuf>,
        /// Leave out IFC's own `Pset_..Common` for each element.
        #[arg(long)]
        no_ifc_common_property_sets: bool,
        /// Leave out the `IfcTypeProduct` behind each element, and put the
        /// type's parameters back on every element of it.
        #[arg(long)]
        no_types: bool,
        /// Write the setup this run used to this JSON file, so it can be
        /// repeated exactly.
        #[arg(long)]
        write_settings: Option<PathBuf>,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Export the binary scene a viewer loads: the elements, the triangles
    /// their geometry tessellates to, and their properties.
    ExportScene {
        #[arg(required = true, num_args = 1..)]
        files: Vec<PathBuf>,
        /// Write to this path instead of replacing the source's extension
        /// with `.rvs`. Required where several sources are given.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Include categorized records without a recovered level association.
        #[arg(long)]
        include_unplaced: bool,
        /// Stop after this many exported elements.
        #[arg(long)]
        limit: Option<usize>,
        /// The furthest a triangle may sit from the surface it approximates,
        /// in millimetres.
        #[arg(long, default_value_t = DEFAULT_CHORD_TOLERANCE_MM)]
        chord_tolerance_mm: f64,
        /// How many times a triangle may be split to reach that tolerance.
        #[arg(long, default_value_t = 2)]
        refinement_depth: u8,
        /// Close a chunk once it holds this many triangles.
        #[arg(long, default_value_t = 250_000)]
        chunk_triangles: usize,
        /// How many elements share one lazily fetched block of properties.
        #[arg(long, default_value_t = 128)]
        property_block: usize,
        /// Deflate effort, 0 to 9.
        #[arg(long, default_value_t = 6)]
        compression: u32,
        /// Announce each stage on stdout as one JSON object the moment it
        /// ends, for a caller driving a progress display.
        #[arg(long)]
        progress: bool,
        /// Maximum decoded bytes accepted from one RVT member, or source
        /// bytes accepted from an IFC file.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Print record bodies of one class as hex, for field analysis.
    Bodies {
        file: PathBuf,
        /// Exact schema class name whose records are printed.
        #[arg(long)]
        class: String,
        /// Stop after this many records.
        #[arg(long, default_value_t = 64)]
        count: usize,
        /// Leading body bytes printed per record.
        #[arg(long, default_value_t = 64)]
        bytes: usize,
        /// Restrict to one descriptor format tag.
        #[arg(long)]
        tag: Option<u32>,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
    /// Copy one inflated partition member to stdout or a file.
    DumpMember {
        file: PathBuf,
        /// Partition stream, for example `Partitions/81`.
        partition: String,
        /// Member offset in the checksum-clean stream, from `partitions --members`.
        logical_offset: u64,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Maximum decoded bytes accepted from the member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_bytes: u64,
    },
    /// Inventory streams and the recovered object records.
    Inspect {
        file: PathBuf,
        /// Skip the partition walk and report streams only.
        #[arg(long)]
        streams_only: bool,
        /// Maximum decoded bytes accepted from one member.
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        max_member_bytes: u64,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            let mut source = error.source();
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Info { file } => info(&file),
        Command::Streams { file } => streams(&file),
        Command::DumpStream {
            file,
            stream,
            output,
        } => dump_stream(&file, &stream, output.as_deref()),
        Command::Partitions {
            file,
            members,
            max_member_decoded_bytes,
            max_partition_decoded_bytes,
            max_candidates,
        } => partitions(
            &file,
            members,
            PartitionReadOptions {
                max_candidates,
                max_member_decoded_bytes,
                max_total_decoded_bytes: max_partition_decoded_bytes,
                class_prefix_range: None,
                marker_envelope: None,
            },
        ),
        other => run_model_command(other),
    }
}

/// Commands that go past the container into schema, records, and export.
#[allow(clippy::too_many_lines)]
fn run_model_command(command: Command) -> Result<(), Box<dyn Error>> {
    match command {
        Command::Schema {
            file,
            class,
            property,
        } => schema(&file, class.as_deref(), property.as_deref()),
        Command::ElemTable { file, records } => elem_table(&file, records),
        Command::PartitionIdProbe { file } => partition_id_probe(&file),
        Command::SchemaPrefixProbe { file, class } => schema_prefix_probe(&file, &class),
        Command::MarkerEnvelopes {
            file,
            class,
            leading,
            trailing,
            max_envelopes,
            dump,
        } => marker_envelopes(&file, &class, leading, trailing, max_envelopes, dump),
        Command::MemberFraming {
            file,
            partition,
            max_member_bytes,
            dump,
            mismatches,
        } => member_framing(
            &file,
            partition.as_deref(),
            max_member_bytes,
            dump,
            mismatches,
        ),
        Command::Records {
            file,
            partition,
            max_member_bytes,
            dump,
        } => records(&file, partition.as_deref(), max_member_bytes, dump),
        Command::Element {
            file,
            id,
            bytes,
            max_member_bytes,
        } => element(&file, id, bytes, max_member_bytes),
        Command::Parameters {
            file,
            class,
            count,
            max_member_bytes,
        } => parameters(&file, class.as_deref(), count, max_member_bytes),
        Command::Brep {
            file,
            reasons,
            max_member_bytes,
        } => brep(&file, reasons, max_member_bytes),
        Command::FaceMarkProbe {
            file,
            max_member_bytes,
        } => face_mark_probe(&file, max_member_bytes),
        Command::BodyOwners {
            file,
            classes,
            max_member_bytes,
        } => body_owners(&file, classes, max_member_bytes),
        Command::Layers {
            file,
            types,
            links,
            max_member_bytes,
        } => layers(&file, types, links, max_member_bytes),
        Command::Names {
            file,
            classes,
            max_member_bytes,
        } => names(&file, classes, max_member_bytes),
        Command::ExportJson {
            file,
            output,
            limit,
            full,
            max_member_bytes,
        } => export_json(&file, output.as_deref(), limit, full, max_member_bytes),
        Command::SerialProbe {
            file,
            class,
            stream,
            record,
            rows,
            dump_remaining,
            dump_stop,
            trace_id,
            dump_body,
            element,
            dump_window,
            dump_count,
            faces_only,
            max_member_bytes,
        } => serial_probe(
            &file,
            &class,
            stream,
            record,
            rows,
            dump_remaining,
            dump_stop.as_deref(),
            trace_id,
            dump_body,
            element,
            dump_window,
            dump_count,
            faces_only,
            max_member_bytes,
        ),
        Command::FlagsProbe {
            file,
            class,
            rows,
            faces_only,
            every_record,
            node_class,
            max_member_bytes,
        } => flags_probe(
            &file,
            &class,
            rows,
            faces_only,
            every_record,
            node_class.as_deref(),
            max_member_bytes,
        ),
        Command::IdentifierProbe {
            file,
            rows,
            max_member_bytes,
        } => identifier_probe(&file, rows, max_member_bytes),
        Command::LoopOwnerProbe {
            file,
            rows,
            max_member_bytes,
        } => loop_owner_probe(&file, rows, max_member_bytes),
        Command::GeometryGraphProbe {
            file,
            rows,
            max_member_bytes,
        } => geometry_graph_probe(&file, rows, max_member_bytes),
        Command::RuledSurfProbe {
            file,
            rows,
            max_member_bytes,
        } => ruled_surf_probe(&file, rows, max_member_bytes),
        Command::ExportIfc {
            files,
            output,
            model_namespace,
            include_unplaced,
            limit,
            settings,
            length_unit,
            no_revit_property_sets,
            no_revit_type_property_sets,
            no_ifc_common_property_sets,
            base_quantities,
            no_types,
            class_mapping,
            write_settings,
            max_member_bytes,
        } => export_ifc(
            &files,
            output.as_deref(),
            model_namespace.as_deref(),
            include_unplaced,
            limit,
            &IfcSettingsArguments {
                settings: settings.as_deref(),
                length_unit,
                no_revit_property_sets,
                no_revit_type_property_sets,
                no_ifc_common_property_sets,
                base_quantities,
                no_types,
                class_mapping: class_mapping.as_deref(),
                write_settings: write_settings.as_deref(),
            },
            max_member_bytes,
        ),
        Command::ExportScene {
            files,
            output,
            include_unplaced,
            limit,
            chord_tolerance_mm,
            refinement_depth,
            chunk_triangles,
            property_block,
            compression,
            progress,
            max_member_bytes,
        } => export_scene(
            &files,
            output.as_deref(),
            include_unplaced,
            limit,
            &SceneArguments {
                chord_tolerance_mm,
                refinement_depth,
                chunk_triangles,
                property_block,
                compression,
            },
            progress,
            max_member_bytes,
        ),
        Command::Bodies {
            file,
            class,
            count,
            bytes,
            tag,
            max_member_bytes,
        } => bodies(&file, &class, count, bytes, tag, max_member_bytes),
        Command::DumpMember {
            file,
            partition,
            logical_offset,
            output,
            max_bytes,
        } => dump_member(
            &file,
            &partition,
            logical_offset,
            output.as_deref(),
            max_bytes,
        ),
        Command::Inspect {
            file,
            streams_only,
            max_member_bytes,
        } => inspect(&file, streams_only, max_member_bytes),
        Command::Info { .. }
        | Command::Streams { .. }
        | Command::DumpStream { .. }
        | Command::Partitions { .. } => unreachable!("handled by run"),
    }
}

fn info(path: &Path) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let basic_info = read_basic_file_info(&container)?;

    println!("File: {}", path.display());
    println!("Container: CFB/OLE");
    println!(
        "Revit version: {}",
        basic_info
            .as_ref()
            .and_then(|info| info.revit_version)
            .map_or_else(|| "unknown".to_owned(), |year| year.to_string())
    );
    println!("Streams: {}", container.streams().len());
    println!("Partitions: {}", container.partition_count());
    println!();
    println!("Known streams:");
    for name in KNOWN_STREAMS {
        if container.stream(name).is_some() {
            println!("- {name}");
        }
    }
    Ok(())
}

fn streams(path: &Path) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    for stream in container.streams() {
        println!("{}\t{}", stream.len(), stream.path());
    }
    Ok(())
}

fn dump_stream(path: &Path, name: &str, output: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    if let Some(output) = output {
        let mut writer = File::create(output)?;
        container.copy_stream(name, &mut writer)?;
        writer.flush()?;
    } else {
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        container.copy_stream(name, &mut writer)?;
        writer.flush()?;
    }
    Ok(())
}

fn partitions(
    path: &Path,
    show_members: bool,
    options: PartitionReadOptions,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let partition_paths = partition_paths(&container);

    let mut total_stored_bytes = 0_u64;
    let mut total_logical_bytes = 0_u64;
    let mut total_decoded_bytes = 0_u64;
    let mut total_members = 0_usize;
    let mut total_embedded_candidates = 0_usize;
    let mut total_failures = 0_usize;
    let mut incomplete_partitions = 0_usize;

    for partition_path in &partition_paths {
        let report = container.inspect_partition(partition_path, options)?;
        println!(
            "{}\tstored={}\tlogical={}\tpages={}\tcandidates={}\tembedded_candidates={}\tmembers={}\tdecoded={}\tfailures={}\tcomplete={}",
            escape_terminal_text(&report.path),
            report.stored_bytes,
            report.logical_bytes,
            report.full_checksum_pages,
            report.gzip_candidates,
            report.skipped_embedded_candidates,
            report.members.len(),
            report.total_decoded_bytes,
            report.failures.len(),
            !report.truncated_by_limit
        );

        if show_members {
            for member in &report.members {
                println!(
                    "  member={}\tstored_offset={}\tlogical_offset={}\tcompressed={}\tdecoded={}",
                    member.index,
                    member.stored_offset,
                    member.logical_offset,
                    member.compressed_bytes,
                    member.decoded_bytes
                );
            }
        }
        for failure in &report.failures {
            println!(
                "  failure\tstored_offset={}\tlogical_offset={}\treason={}",
                failure.stored_offset,
                failure.logical_offset,
                escape_terminal_text(&failure.message)
            );
        }

        total_stored_bytes = total_stored_bytes.saturating_add(report.stored_bytes);
        total_logical_bytes = total_logical_bytes.saturating_add(report.logical_bytes);
        total_decoded_bytes = total_decoded_bytes.saturating_add(report.total_decoded_bytes);
        total_members = total_members.saturating_add(report.members.len());
        total_embedded_candidates =
            total_embedded_candidates.saturating_add(report.skipped_embedded_candidates);
        total_failures = total_failures.saturating_add(report.failures.len());
        incomplete_partitions += usize::from(report.truncated_by_limit);
    }

    println!();
    println!("Partition summary:");
    println!("Partitions: {}", partition_paths.len());
    println!("Stored bytes: {total_stored_bytes}");
    println!("Checksum-clean bytes: {total_logical_bytes}");
    println!("Validated gzip members: {total_members}");
    println!("Embedded gzip signatures skipped: {total_embedded_candidates}");
    println!("Decoded bytes: {total_decoded_bytes}");
    println!("Candidate failures: {total_failures}");
    println!("Incomplete partitions: {incomplete_partitions}");
    Ok(())
}

fn schema(
    path: &Path,
    class_name: Option<&str>,
    property_text: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let Some(stream) = container.stream("Formats/Latest") else {
        println!("Schema stream: not present");
        return Ok(());
    };

    let raw = container.read_stream_with_limit(stream.path(), DEFAULT_DECODE_LIMIT as u64)?;
    let (decoded, schema, stripped_page_checksums) = decode_schema_stream(&raw)?;

    println!("Schema stream: {} ({} bytes)", stream.path(), stream.len());
    match decoded.framing {
        StreamFraming::Raw => println!("Framing: raw"),
        StreamFraming::TruncatedGzip { gzip_offset } => {
            println!("Framing: truncated gzip at byte {gzip_offset}");
        }
    }
    println!("Checksum-page trailers stripped: {stripped_page_checksums}");
    println!("Decoded bytes: {}", decoded.payload.len());
    println!("Classes: {}", schema.classes.len());
    println!("Top-level classes: {}", schema.top_level_class_count);
    println!("Properties: {}", schema.property_count);
    println!("Parsed property records: {}", schema.parsed_property_count);
    println!(
        "Unresolved references: {}",
        schema.unresolved_references.len()
    );
    println!(
        "Inline index mismatches: {}",
        schema.inline_index_mismatches.len()
    );
    println!("Trailing bytes: {}", schema.trailing_bytes.len());
    if let Some(class_name) = class_name {
        return print_class_properties(&schema, class_name);
    }
    if let Some(text) = property_text {
        print_matching_properties(&schema, text);
        return Ok(());
    }

    println!();
    println!("Class inventory:");
    for class in &schema.classes {
        let parent = match &class.parent {
            TypeReference::None => "-".to_owned(),
            TypeReference::Inline { index, name, .. }
            | TypeReference::Reference { index, name } => {
                format!("{index}:{}", escape_terminal_text(name))
            }
            TypeReference::Unresolved { index } => format!("{index}:?"),
        };
        println!(
            "{}\t{}\tparent={}\tversion={}\tproperties={}",
            class.index,
            escape_terminal_text(&class.name),
            parent,
            class.version,
            class.properties.len()
        );
    }
    Ok(())
}

/// Print one class's declared properties, with the width each fixed-size type
/// occupies. Variable-width types are shown as such rather than guessed.
fn print_class_properties(schema: &Schema, class_name: &str) -> Result<(), Box<dyn Error>> {
    let class = schema.class_by_name(class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "schema class not found: {}",
                escape_terminal_text(class_name)
            ),
        )
    })?;

    println!();
    println!(
        "Class {}: {}",
        class.index,
        escape_terminal_text(&class.name)
    );
    let parent = match &class.parent {
        TypeReference::None => "-".to_owned(),
        TypeReference::Inline { index, name, .. } | TypeReference::Reference { index, name } => {
            format!("{index}:{}", escape_terminal_text(name))
        }
        TypeReference::Unresolved { index } => format!("{index}:?"),
    };
    println!("Parent: {parent}");
    println!("Version: {}", class.version);
    println!("Properties: {}", class.properties.len());
    for (index, property) in class.properties.iter().enumerate() {
        let width = property
            .field_type
            .fixed_width()
            .map_or_else(|| "variable".to_owned(), |bytes| bytes.to_string());
        let element = property
            .element
            .as_ref()
            .map_or_else(String::new, |element| {
                format!(
                    "\telement={:?}[modes={:#04x} item_mode={} size={}{}]",
                    element.field_type,
                    element.raw_modes,
                    element.item_mode,
                    element
                        .size
                        .map_or_else(|| "-".to_owned(), |size| size.to_string()),
                    element
                        .element
                        .as_ref()
                        .map_or_else(String::new, |inner| format!(
                            " inner={:?}x{}",
                            inner.field_type,
                            inner.size.unwrap_or(1)
                        ))
                )
            });
        let static_type = property.static_type.as_ref().and_then(TypeReference::name);
        println!(
            "{index}\t{}\ttype={:?}\twidth={width}\tmodes={:#04x}\titem_mode={}\tsize={}{}{}",
            escape_terminal_text(&property.name),
            property.field_type,
            property.raw_modes,
            property.item_mode,
            property
                .size
                .map_or_else(|| "-".to_owned(), |size| size.to_string()),
            element,
            static_type.map_or_else(String::new, |name| format!(
                "\tstatic={}",
                escape_terminal_text(name)
            ))
        );
    }
    Ok(())
}

/// List every declared property whose name contains `text`, with the class that
/// declares it and that class's own version. Written for questions that span the
/// whole schema rather than one class - which classes carry a `_v<N>` property,
/// for instance, and at which versions.
fn print_matching_properties(schema: &Schema, text: &str) {
    println!();
    println!("Properties whose name contains {text:?}:");
    let mut matches = 0_usize;
    for class in &schema.classes {
        for (index, property) in class.properties.iter().enumerate() {
            if !property.name.contains(text) {
                continue;
            }
            matches += 1;
            println!(
                "{}\t{}\tclass_version={}\t{index}\t{}\ttype={:?}\tmodes={:#04x}\titem_mode={}",
                class.index,
                escape_terminal_text(&class.name),
                class.version,
                escape_terminal_text(&property.name),
                property.field_type,
                property.raw_modes,
                property.item_mode,
            );
        }
    }
    println!("Matching properties: {matches}");
}

fn escape_terminal_text(value: &str) -> String {
    value.escape_default().collect()
}

fn elem_table(path: &Path, show_records: bool) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let Some(stream) = container.stream("Global/ElemTable") else {
        println!("Element table stream: not present");
        return Ok(());
    };
    let raw = container.read_stream_with_limit(stream.path(), DEFAULT_DECODE_LIMIT as u64)?;
    let (decoded, table, stripped_page_checksums) = decode_elem_table_stream(&raw)?;

    println!(
        "Element table stream: {} ({} bytes)",
        stream.path(),
        stream.len()
    );
    match decoded.framing {
        StreamFraming::Raw => println!("Framing: raw"),
        StreamFraming::TruncatedGzip { gzip_offset } => {
            println!("Framing: truncated gzip at byte {gzip_offset}");
        }
    }
    println!("Checksum-page trailers stripped: {stripped_page_checksums}");
    println!("Framing prefix bytes: {}", decoded.prefix.len());
    println!("Decoded bytes: {}", decoded.payload.len());
    println!("Declared elements: {}", table.header.element_count);
    println!("Declared records: {}", table.header.record_count);
    let framing = match table.layout.framing {
        RecordFraming::Implicit => "implicit".to_owned(),
        RecordFraming::Explicit { marker_bytes } => {
            format!("explicit-{marker_bytes}-byte-marker")
        }
    };
    println!("Record framing: {framing}");
    println!("Record start: {}", table.layout.start);
    println!("Record stride: {}", table.layout.stride);
    println!("Marker offset: {}", table.layout.marker_offset);
    println!("Records matching marker: {}", table.marker_match_count());
    println!("Parsed records: {}", table.records.len());
    println!("Unique primary IDs: {}", table.unique_primary_id_count());
    println!(
        "Primary/secondary mismatches: {}",
        table.primary_secondary_mismatch_count()
    );
    println!("Preserved header bytes: {}", table.leading_bytes().len());
    println!("Preserved trailing bytes: {}", table.trailing_bytes().len());

    if show_records {
        println!();
        println!("Record inventory:");
        for (index, record) in table.records.iter().enumerate() {
            println!(
                "{index}\toffset={}\tprimary={}\tsecondary={}",
                record.offset, record.id_primary, record.id_secondary
            );
        }
    }
    Ok(())
}

fn partition_id_probe(path: &Path) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let element_ids = elem_table_ids(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Global/ElemTable is required for this probe",
        )
    })?;
    let partition_paths = partition_paths(&container);

    let mut total_members = 0_usize;
    let mut prefix_members = 0_usize;
    let mut member_id_hits = 0_usize;
    let mut prefix_values = BTreeSet::new();
    let mut overlapping_ids = BTreeSet::new();

    println!("Partition first-word / element-ID probe:");
    println!("Candidate element IDs: {}", element_ids.len());
    for partition_path in &partition_paths {
        let report =
            container.inspect_partition(partition_path, PartitionReadOptions::default())?;
        let mut partition_prefixes = BTreeSet::new();
        let mut partition_hits = BTreeSet::new();
        let mut partition_member_hits = 0_usize;
        for member in &report.members {
            let Some(prefix) = member.decoded_prefix().get(..4) else {
                continue;
            };
            let value = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
            prefix_members += 1;
            partition_prefixes.insert(value);
            prefix_values.insert(value);
            if element_ids.contains(&value) {
                partition_member_hits += 1;
                member_id_hits += 1;
                partition_hits.insert(value);
                overlapping_ids.insert(value);
            }
        }
        total_members += report.members.len();
        println!(
            "{}	members={}	prefixes={}	member_hits={}	distinct_words={}	distinct_id_hits={}",
            escape_terminal_text(partition_path),
            report.members.len(),
            report
                .members
                .iter()
                .filter(|member| member.decoded_prefix().len() >= 4)
                .count(),
            partition_member_hits,
            partition_prefixes.len(),
            partition_hits.len()
        );
    }

    println!();
    println!("Probe summary:");
    println!("Partitions: {}", partition_paths.len());
    println!("Validated members: {total_members}");
    println!("Members with a first u32: {prefix_members}");
    println!("Members whose first u32 is a candidate ID: {member_id_hits}");
    println!("Distinct first-u32 values: {}", prefix_values.len());
    println!("Distinct overlapping IDs: {}", overlapping_ids.len());
    println!(
        "Element-ID coverage: {}",
        percentage(overlapping_ids.len(), element_ids.len())
    );
    println!(
        "Distinct-word precision: {}",
        percentage(overlapping_ids.len(), prefix_values.len())
    );
    println!(
        "Member hit rate: {}",
        percentage(member_id_hits, prefix_members)
    );
    Ok(())
}

fn percentage(numerator: usize, denominator: usize) -> String {
    if denominator == 0 {
        return "0.000%".to_owned();
    }
    let thousandths = (numerator as u128 * 100_000) / denominator as u128;
    format!("{}.{:03}%", thousandths / 1000, thousandths % 1000)
}

fn schema_prefix_probe(path: &Path, class_name: &str) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let raw = container.read_stream_with_limit("Formats/Latest", DEFAULT_DECODE_LIMIT as u64)?;
    let (_, schema, _) = decode_schema_stream(&raw)?;
    let class = schema.class_by_name(class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "schema class not found: {}",
                escape_terminal_text(class_name)
            ),
        )
    })?;
    let class_index = class.index;
    let options = PartitionReadOptions {
        class_prefix_range: Some((class_index, class_index)),
        ..PartitionReadOptions::default()
    };
    let partition_paths = partition_paths(&container);

    let mut total_members = 0_usize;
    let mut members_with_candidates = 0_usize;
    let mut total_candidates = 0_u64;
    for partition_path in &partition_paths {
        let report = container.inspect_partition(partition_path, options)?;
        total_members += report.members.len();
        members_with_candidates += report
            .members
            .iter()
            .filter(|member| member.class_prefix_candidates > 0)
            .count();
        total_candidates = total_candidates.saturating_add(
            report
                .class_prefix_counts
                .get(&class_index)
                .copied()
                .unwrap_or(0),
        );
    }

    println!("Schema-backed partition prefix probe:");
    println!("Class: {}", escape_terminal_text(&class.name));
    println!("Schema index: {class_index}");
    println!("Partitions: {}", partition_paths.len());
    println!("Validated members: {total_members}");
    println!("Members with candidates: {members_with_candidates}");
    println!("Candidate prefixes: {total_candidates}");
    Ok(())
}

/// Aggregate record-boundary evidence collected from marker envelopes.
#[derive(Debug, Default)]
struct EnvelopeStatistics {
    envelopes: usize,
    /// Last `LEADING_PATTERN_BYTES` bytes before a marker.
    leading_patterns: BTreeMap<Vec<u8>, usize>,
    /// Distance between consecutive candidates inside one decoded member.
    candidate_gaps: BTreeMap<u64, usize>,
    /// Little-endian `u32` immediately after a marker.
    following_words: BTreeMap<u32, usize>,
    following_word_samples: usize,
    /// Stride of the ascending `u32` run a marker sits inside, when one exists.
    sequence_strides: BTreeMap<usize, usize>,
}

impl EnvelopeStatistics {
    fn observe(&mut self, envelopes: &[MarkerEnvelope], marker_value: u32) {
        let mut previous: Option<(usize, u64)> = None;
        for envelope in envelopes {
            self.envelopes += 1;
            if let Some(stride) = sequence_stride(envelope, marker_value) {
                *self.sequence_strides.entry(stride).or_default() += 1;
            }

            let leading = envelope.leading();
            if leading.len() >= LEADING_PATTERN_BYTES {
                let pattern = leading[leading.len() - LEADING_PATTERN_BYTES..].to_vec();
                *self.leading_patterns.entry(pattern).or_default() += 1;
            }
            if let Some(word) = envelope.trailing().get(..4) {
                let value = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
                *self.following_words.entry(value).or_default() += 1;
                self.following_word_samples += 1;
            }
            // Captured envelopes are a prefix of the candidates in a member,
            // so consecutive captures are consecutive candidates.
            if let Some((member, offset)) = previous {
                if member == envelope.member_index {
                    let gap = envelope.decoded_offset.saturating_sub(offset);
                    *self.candidate_gaps.entry(gap).or_default() += 1;
                }
            }
            previous = Some((envelope.member_index, envelope.decoded_offset));
        }
    }
}

/// Smallest stride at which the marker is one element of a strictly ascending
/// `u32` run. Such a marker is ordinary numeric data, not a record boundary.
fn sequence_stride(envelope: &MarkerEnvelope, marker_value: u32) -> Option<usize> {
    let leading = envelope.leading();
    let trailing = envelope.trailing();
    SEQUENCE_STRIDES.into_iter().find(|stride| {
        let Some(before) = leading
            .len()
            .checked_sub(*stride)
            .and_then(|start| leading.get(start..start + 4))
        else {
            return false;
        };
        let Some(after) = trailing.get(stride - 4..*stride) else {
            return false;
        };
        read_u32(before) < marker_value && read_u32(after) > marker_value
    })
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn top_counts<K: Clone + Ord, V: Copy + Ord>(counts: &BTreeMap<K, V>, limit: usize) -> Vec<(K, V)> {
    let mut entries = counts
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    entries.truncate(limit);
    entries
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn marker_envelopes(
    path: &Path,
    class_name: &str,
    leading_bytes: usize,
    trailing_bytes: usize,
    max_envelopes: usize,
    dump: usize,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let raw = container.read_stream_with_limit("Formats/Latest", DEFAULT_DECODE_LIMIT as u64)?;
    let (_, schema, _) = decode_schema_stream(&raw)?;
    let class = schema.class_by_name(class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "schema class not found: {}",
                escape_terminal_text(class_name)
            ),
        )
    })?;
    let options = PartitionReadOptions {
        marker_envelope: Some(MarkerEnvelopeOptions {
            class_index: class.index,
            leading_bytes,
            trailing_bytes,
            max_envelopes,
        }),
        ..PartitionReadOptions::default()
    };
    let element_ids = elem_table_ids(&container)?;
    let partition_paths = partition_paths(&container);

    println!("Marker envelope capture:");
    println!("Class: {}", escape_terminal_text(&class.name));
    println!("Schema index: {}", class.index);
    println!("Leading/trailing context bytes: {leading_bytes}/{trailing_bytes}");
    println!("Envelope budget per partition: {max_envelopes}");
    match &element_ids {
        Some(ids) => println!("Element-ID reference: Global/ElemTable ({} IDs)", ids.len()),
        None => println!("Element-ID reference: Global/ElemTable not present"),
    }
    println!();

    let mut statistics = EnvelopeStatistics::default();
    let mut total_members = 0_usize;
    let mut members_with_candidates = 0_usize;
    let mut total_candidates = 0_u64;
    let mut budgeted_partitions = 0_usize;
    let mut dumped = Vec::new();

    for partition_path in &partition_paths {
        let report = container.inspect_partition(partition_path, options)?;
        println!(
            "{}\tmembers={}\tmembers_with_candidates={}\tcandidates={}\tenvelopes={}\tbudget_reached={}",
            escape_terminal_text(partition_path),
            report.members.len(),
            report
                .members
                .iter()
                .filter(|member| member.marker_candidates > 0)
                .count(),
            report.marker_candidates,
            report.marker_envelopes.len(),
            report.marker_envelopes_truncated
        );

        statistics.observe(&report.marker_envelopes, u32::from(class.index));
        total_members += report.members.len();
        members_with_candidates += report
            .members
            .iter()
            .filter(|member| member.marker_candidates > 0)
            .count();
        total_candidates = total_candidates.saturating_add(report.marker_candidates);
        budgeted_partitions += usize::from(report.marker_envelopes_truncated);
        for envelope in report.marker_envelopes {
            if dumped.len() >= dump {
                break;
            }
            dumped.push((partition_path.clone(), envelope));
        }
    }

    println!();
    println!("Envelope summary:");
    println!("Partitions: {}", partition_paths.len());
    println!("Validated members: {total_members}");
    println!("Members with candidates: {members_with_candidates}");
    println!("Marker candidates: {total_candidates}");
    println!("Captured envelopes: {}", statistics.envelopes);
    println!("Partitions stopped by envelope budget: {budgeted_partitions}");

    println!();
    println!("Record-boundary evidence:");
    print_leading_patterns(&statistics);
    print_candidate_gaps(&statistics);
    print_following_words(&statistics, element_ids.as_ref());
    print_sequence_strides(&statistics);

    if !dumped.is_empty() {
        println!();
        println!("Envelope dump:");
        for (partition_path, envelope) in &dumped {
            println!(
                "{}\tmember={}\tdecoded_offset={}\tleading={}\tmarker={}\ttrailing={}",
                escape_terminal_text(partition_path),
                envelope.member_index,
                envelope.decoded_offset,
                hex(envelope.leading()),
                hex(envelope.marker()),
                hex(envelope.trailing())
            );
        }
    }
    Ok(())
}

fn print_leading_patterns(statistics: &EnvelopeStatistics) {
    let sampled = statistics.leading_patterns.values().sum::<usize>();
    println!(
        "Envelopes with {LEADING_PATTERN_BYTES} leading bytes: {sampled} (distinct patterns: {})",
        statistics.leading_patterns.len()
    );
    for (pattern, count) in top_counts(&statistics.leading_patterns, HISTOGRAM_ROWS) {
        println!(
            "  leading={}\tcount={count}\tshare={}",
            hex(&pattern),
            percentage(count, sampled)
        );
    }
}

fn print_candidate_gaps(statistics: &EnvelopeStatistics) {
    let sampled = statistics.candidate_gaps.values().sum::<usize>();
    println!(
        "Consecutive same-member candidate gaps: {sampled} (distinct gaps: {})",
        statistics.candidate_gaps.len()
    );
    for (gap, count) in top_counts(&statistics.candidate_gaps, HISTOGRAM_ROWS) {
        println!(
            "  gap={gap}\tcount={count}\tshare={}",
            percentage(count, sampled)
        );
    }
}

fn print_following_words(statistics: &EnvelopeStatistics, element_ids: Option<&BTreeSet<u32>>) {
    println!(
        "Post-marker u32 samples: {} (distinct values: {})",
        statistics.following_word_samples,
        statistics.following_words.len()
    );
    let Some(element_ids) = element_ids else {
        println!("  ElemTable overlap: not evaluated");
        return;
    };
    let matching_values = statistics
        .following_words
        .keys()
        .filter(|value| element_ids.contains(value))
        .count();
    let matching_samples = statistics
        .following_words
        .iter()
        .filter(|(value, _)| element_ids.contains(value))
        .map(|(_, count)| *count)
        .sum::<usize>();
    println!(
        "  Samples whose u32 is a candidate ID: {matching_samples} ({})",
        percentage(matching_samples, statistics.following_word_samples)
    );
    println!(
        "  Distinct values that are candidate IDs: {matching_values} ({})",
        percentage(matching_values, statistics.following_words.len())
    );
}

fn print_sequence_strides(statistics: &EnvelopeStatistics) {
    let sampled = statistics.sequence_strides.values().sum::<usize>();
    println!(
        "Markers inside an ascending u32 run: {sampled} ({})",
        percentage(sampled, statistics.envelopes)
    );
    for (stride, count) in top_counts(&statistics.sequence_strides, HISTOGRAM_ROWS) {
        println!(
            "  run_stride={stride}\tcount={count}\tshare={}",
            percentage(count, sampled)
        );
    }
}

/// Counters for the two member-level structures under test.
#[derive(Debug, Default)]
struct MemberFramingStatistics {
    members: usize,
    descriptors: usize,
    stored_span_matches: usize,
    chain_links: usize,
    chain_links_checked: usize,
    /// Distance between the end of the previous member and this descriptor.
    descriptor_gaps: BTreeMap<u64, usize>,
    walk_failures_at_page_limit: usize,
    known_format_tags: usize,
    format_tags: BTreeMap<u32, usize>,
    walked: usize,
    walk_failures: BTreeMap<String, usize>,
    count_matches: usize,
    body_matches: usize,
    records: u64,
    /// Members whose payload ends inside a record continuing into the next one.
    continued_members: usize,
    /// Members that resumed after a carried tail.
    resumed_members: usize,
    /// Carried tails consumed by a member that then ended on a record boundary.
    continuations_closed: usize,
    /// Carries abandoned because the next member could not be walked.
    continuations_dropped: usize,
}

fn member_framing(
    path: &Path,
    partition: Option<&str>,
    max_member_bytes: u64,
    dump: usize,
    mismatches: usize,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let partition_paths = match partition {
        Some(name) => vec![name.to_owned()],
        None => partition_paths(&container),
    };
    let mut statistics = MemberFramingStatistics::default();
    let mut dumped = 0_usize;
    let mut mismatches_printed = 0_usize;

    println!("Member framing:");
    for partition_path in &partition_paths {
        let report =
            container.inspect_partition(partition_path, PartitionReadOptions::default())?;
        let mut previous: Option<&rvt_container::PartitionMember> = None;
        // Bytes the previous member's last record still expects.
        let mut carry = 0_u64;
        for member in &report.members {
            statistics.members += 1;
            let Some(descriptor) = member.descriptor else {
                previous = Some(member);
                statistics.continuations_dropped += usize::from(carry > 0);
                carry = 0;
                continue;
            };
            statistics.descriptors += 1;
            statistics.stored_span_matches +=
                usize::from(descriptor.matches_stored_span(member.compressed_bytes));
            if let Some(previous) = previous {
                let gap = member
                    .logical_offset
                    .saturating_sub(previous.logical_offset + previous.compressed_bytes);
                *statistics.descriptor_gaps.entry(gap).or_default() += 1;
                statistics.chain_links_checked += 1;
                let decoded_matches =
                    u64::from(descriptor.previous_decoded_bytes) == previous.decoded_bytes;
                let span_matches = previous
                    .descriptor
                    .is_some_and(|earlier| descriptor.previous_stored_span == earlier.stored_span);
                statistics.chain_links += usize::from(decoded_matches && span_matches);
            }
            *statistics
                .format_tags
                .entry(descriptor.format_tag)
                .or_default() += 1;

            if let Some(layout) = RecordLayout::from_format_tag(descriptor.format_tag) {
                statistics.known_format_tags += 1;
                carry = walk_member(
                    &container,
                    partition_path,
                    member,
                    descriptor,
                    layout,
                    max_member_bytes,
                    carry,
                    dump,
                    &mut dumped,
                    (mismatches, &mut mismatches_printed),
                    &mut statistics,
                )?;
            } else {
                statistics.continuations_dropped += usize::from(carry > 0);
                carry = 0;
            }
            previous = Some(member);
        }
    }
    print_member_framing(&statistics, partition_paths.len());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_member(
    container: &RvtContainer,
    partition_path: &str,
    member: &rvt_container::PartitionMember,
    descriptor: rvt_container::MemberDescriptor,
    layout: RecordLayout,
    max_member_bytes: u64,
    carry: u64,
    dump: usize,
    dumped: &mut usize,
    (mismatch_limit, mismatches_printed): (usize, &mut usize),
    statistics: &mut MemberFramingStatistics,
) -> Result<u64, Box<dyn Error>> {
    let payload = container.decode_partition_member(
        partition_path,
        member.logical_offset,
        max_member_bytes,
    )?;
    let leading_carry = usize::try_from(carry).unwrap_or(usize::MAX);
    match MemberWalk::parse(&payload, layout, leading_carry) {
        Ok(walk) => {
            statistics.walked += 1;
            statistics.records = statistics.records.saturating_add(walk.records.len() as u64);
            statistics.count_matches +=
                usize::from(u64::from(descriptor.record_count) == walk.records.len() as u64);
            let matches =
                walk.matches_descriptor(descriptor.record_count, descriptor.record_body_bytes);
            statistics.body_matches += usize::from(matches);
            if !matches && *mismatches_printed < mismatch_limit {
                *mismatches_printed += 1;
                println!(
                    "  mismatch\t{}\tmember={}\tpayload={}\tcarry={}\trecords={}\tdeficit={}\tbody_declared={}\tbody_in_member={}\tbody_started={}",
                    escape_terminal_text(partition_path),
                    member.index,
                    payload.len(),
                    walk.leading_carry,
                    walk.records.len(),
                    walk.trailing_deficit,
                    descriptor.record_body_bytes,
                    walk.body_bytes_in_member,
                    walk.body_bytes
                );
            }
            statistics.continued_members += usize::from(walk.trailing_deficit > 0);
            if carry > 0 {
                statistics.resumed_members += 1;
                statistics.continuations_closed += usize::from(walk.trailing_deficit == 0);
            }
            if *dumped < dump {
                *dumped += 1;
                println!(
                    "  {}\tmember={}\tformat={}\tcarry={}\trecords={}\tdeclared={}\tbody={}\tdeclared_body={}\tdeficit={}",
                    escape_terminal_text(partition_path),
                    member.index,
                    descriptor.format_tag,
                    walk.leading_carry,
                    walk.records.len(),
                    descriptor.record_count,
                    walk.body_bytes_in_member,
                    descriptor.record_body_bytes,
                    walk.trailing_deficit
                );
                for record in walk.records.iter().take(4) {
                    println!(
                        "    record\toffset={}\theader={}\tbody={}",
                        record.offset, record.header_bytes, record.body_bytes
                    );
                }
            }
            Ok(walk.trailing_deficit)
        }
        Err(error) => {
            *statistics
                .walk_failures
                .entry(error.to_string())
                .or_default() += 1;
            statistics.walk_failures_at_page_limit +=
                usize::from(member.decoded_bytes >= MEMBER_PAGE_LIMIT_BYTES);
            statistics.continuations_dropped += usize::from(carry > 0);
            Ok(0)
        }
    }
}

fn print_member_framing(statistics: &MemberFramingStatistics, partitions: usize) {
    println!();
    println!("Descriptor summary:");
    println!("Partitions: {partitions}");
    println!("Validated members: {}", statistics.members);
    println!("Members with a descriptor: {}", statistics.descriptors);
    println!(
        "Stored span == compressed + 16: {} ({})",
        statistics.stored_span_matches,
        percentage(statistics.stored_span_matches, statistics.descriptors)
    );
    println!(
        "Back-pointers matching the previous member: {} ({})",
        statistics.chain_links,
        percentage(statistics.chain_links, statistics.chain_links_checked)
    );
    print!("Descriptor gaps:");
    for (gap, count) in &statistics.descriptor_gaps {
        print!(" {gap}={count}");
    }
    println!();
    print!("Format tags:");
    for (tag, count) in &statistics.format_tags {
        print!(" {tag}={count}");
    }
    println!();

    println!();
    println!("Record-array summary:");
    println!(
        "Members with a known format tag: {}",
        statistics.known_format_tags
    );
    println!(
        "Members walked without an error: {} ({})",
        statistics.walked,
        percentage(statistics.walked, statistics.known_format_tags)
    );
    println!(
        "Members ending inside a record: {}",
        statistics.continued_members
    );
    println!(
        "Members resuming after a carried tail: {} (ending on a record boundary: {})",
        statistics.resumed_members, statistics.continuations_closed
    );
    println!(
        "Carries dropped at a walk failure: {}",
        statistics.continuations_dropped
    );
    println!(
        "Record count == descriptor count: {} ({})",
        statistics.count_matches,
        percentage(statistics.count_matches, statistics.walked)
    );
    println!(
        "Body bytes == descriptor body bytes: {} ({})",
        statistics.body_matches,
        percentage(statistics.body_matches, statistics.walked)
    );
    println!("Records recovered: {}", statistics.records);
    let failures = statistics.known_format_tags - statistics.walked;
    println!(
        "Walk failures on members at the {} KiB limit: {} of {failures}",
        MEMBER_PAGE_LIMIT_BYTES / 1024,
        statistics.walk_failures_at_page_limit
    );
    for (reason, count) in top_counts(&statistics.walk_failures, HISTOGRAM_ROWS) {
        println!(
            "  failure\tcount={count}\treason={}",
            escape_terminal_text(&reason)
        );
    }
}

/// Counters for the record header under test.
#[derive(Debug, Default)]
struct RecordStatistics {
    records: u64,
    narrow_records: u64,
    wide_records: u64,
    /// First `u32` of every record header.
    lead_values: BTreeSet<u32>,
    lead_in_elem_table: u64,
    lead_hits: BTreeSet<u32>,
    /// Members whose record lead values are strictly ascending.
    ascending_members: usize,
    checked_members: usize,
    tag_records: BTreeMap<u32, u64>,
    tag_class_hits: BTreeMap<u32, u64>,
    tag_class_counts: BTreeMap<(u32, u16), u64>,
    class_counts: BTreeMap<u16, u64>,
    /// The `u16` sharing the trailing word with the class index.
    companion_words: BTreeMap<u16, u64>,
    /// Second word of the wide header, which the narrow layout does not have.
    wide_second_words: BTreeMap<u32, u64>,
    body_min: u64,
    body_max: u64,
    empty_bodies: u64,
}

fn element(
    path: &Path,
    id: u32,
    body_bytes: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?;
    let partition_paths = partition_paths(&container);
    let header_class_index = schema
        .as_ref()
        .and_then(|schema| schema.class_by_name(ELEMENT_HEADER_CLASS))
        .map(|class| class.index);
    let mut found = 0_usize;

    println!("Element {id}:");
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |partition_path, member, format_tag, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.id != id {
                    continue;
                }
                found += 1;
                let class = schema
                    .as_ref()
                    .and_then(|schema| schema.class_by_index(header.class_index))
                    .map_or_else(|| "?".to_owned(), |class| escape_terminal_text(&class.name));
                println!(
                    "  {}\tmember={}\toffset={}\tformat={format_tag}\tclass={} {class}\tbody={}\tcompanion={}",
                    escape_terminal_text(partition_path),
                    member.index,
                    record.offset,
                    header.class_index,
                    header.body_bytes,
                    header.companion
                );
                if header.class_index == header_class_index.unwrap_or(u16::MAX) {
                    if let Some(fields) = ElementHeaderFields::parse(record.body_in(payload)) {
                        println!(
                            "    category={}\tfamily={}",
                            fields
                                .category
                                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
                            fields
                                .family_id
                                .map_or_else(|| "-".to_owned(), |value| value.to_string())
                        );
                    }
                }
                if body_bytes > 0 {
                    let start = record.body_offset();
                    let end = record.end().min(start + body_bytes).min(payload.len());
                    if let Some(slice) = payload.get(start..end) {
                        println!("    body={}", hex(slice));
                    }
                }
            }
        },
    )?;

    println!();
    println!("Records: {found}");
    if found == 0 {
        println!("No record carries this identifier.");
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn parameters(
    path: &Path,
    class_name: Option<&str>,
    count: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let catalog = read_basic_file_info(&container)?
        .and_then(|info| info.revit_version)
        .and_then(Catalog::for_release);
    let schema = read_schema(&container)?;
    let parameter_set_classes = parameter_set_class_indexes(schema.as_ref());
    let wanted = class_name
        .map(|name| {
            schema
                .as_ref()
                .and_then(|schema| schema.class_by_name(name))
                .map(|class| class.index)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("schema class not found: {}", escape_terminal_text(name)),
                    )
                })
        })
        .transpose()?;
    let partition_paths = partition_paths(&container);
    let (_, parameter_ids) = calibrate_names(
        &container,
        schema.as_ref(),
        &partition_paths,
        max_member_bytes,
    )?;
    let accept = |id: i32| parameter_ids.contains(&id);
    let accept_verified = |id: i32| {
        if id < 0 {
            catalog.is_some_and(|catalog| catalog.built_in_parameter(id).is_some())
        } else {
            parameter_ids.contains(&id)
        }
    };

    println!("Parameter sets:");
    println!("Known parameter elements: {}", parameter_ids.len());
    let mut shown = 0_usize;
    let mut with_parameters = 0_u64;
    let mut bodies = 0_u64;
    let mut values = 0_u64;
    let mut scanned_bodies = 0_u64;
    let mut scanned_values = 0_u64;

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, format_tag, layout, walk, payload| {
            if format_tag != ELEMENT_CLASS_FORMAT_TAG {
                return;
            }
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if wanted.is_some_and(|index| index != header.class_index) {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                bodies += 1;
                let Some(fields) = ElementFields::parse(body, header.id) else {
                    continue;
                };
                let scanned = if let (Some(classes), Some(_)) = (parameter_set_classes, catalog) {
                    ParameterSets::scan_schema_bound(
                        body,
                        fields.id_offset + 4 + ELEMENT_TAIL_BYTES,
                        fields.id_offset,
                        classes,
                        &accept_verified,
                    )
                } else {
                    ParameterSets::scan(body, &accept)
                };
                if let Some(found) = &scanned {
                    scanned_bodies += 1;
                    scanned_values += found.parameters.len() as u64;
                }
                let declared = (|| {
                    let classes = parameter_set_classes?;
                    ParameterSets::from_record(schema.as_ref()?, header.class_index, body, classes)
                })();
                let Some(found) = declared.or(scanned) else {
                    continue;
                };
                with_parameters += 1;
                values += found.parameters.len() as u64;
                if shown >= count {
                    continue;
                }
                shown += 1;
                let class = schema
                    .as_ref()
                    .and_then(|schema| schema.class_by_index(header.class_index))
                    .map_or("?", |class| class.name.as_str());
                println!(
                    "  id={}\tclass={}\tsets={}\tparameters={}",
                    header.id,
                    escape_terminal_text(class),
                    found.sets,
                    found.parameters.len()
                );
                for parameter in found.parameters.iter().take(12) {
                    println!(
                        "    param={}\t{}",
                        parameter.id,
                        escape_terminal_text(&describe_parameter_value(&parameter.value))
                    );
                }
            }
        },
    )?;

    println!();
    println!("Element bodies examined: {bodies}");
    println!(
        "Bodies with a parameter run: {with_parameters} ({})",
        percentage(
            usize::try_from(with_parameters).unwrap_or(usize::MAX),
            usize::try_from(bodies).unwrap_or(usize::MAX)
        )
    );
    println!("Parameter values recovered: {values}");
    println!("  bodies the scan alone explained: {scanned_bodies} ({scanned_values} values)");
    Ok(())
}

fn describe_parameter_value(value: &ParameterValue) -> String {
    match value {
        ParameterValue::Double(number) => format!("double={number}"),
        ParameterValue::Integer(number) => format!("int={number}"),
        ParameterValue::Text(text) => format!("text={text:?}"),
        ParameterValue::Reference(id) => format!("ref={id}"),
    }
}

/// Which identifiers a record names and never writes an object for.
///
/// A node reaches the stream by being *fully* referenced - identifier plus
/// class - because that is what the walk queues; a bare identifier
/// (`GEdge.m_next`, `GEdgeLoop.m_pFace`) names an object without queueing one.
/// So an object nothing full-references is never written, and the boundary of
/// a face whose `m_pFirstLoop` is null is missing for exactly that reason: the
/// only full reference to its loop was that null.
///
/// This asks what those unwritten identifiers are. Per record: which slots
/// name them, whether the written identifiers form a dense range with the
/// unwritten ones as its gaps, and whether the record's neighbours in the same
/// member write the objects it is missing.
#[allow(clippy::too_many_lines)] // One streaming pass and the tallies it fills.
fn identifier_probe(path: &Path, rows: usize, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let face_class_index = schema_class_index(Some(&schema), "Face");
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement");
    let (Some(face_class_index), Some(geometry_element_class_index)) =
        (face_class_index, geometry_element_class_index)
    else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare Face and GElement",
        )));
    };
    let partition_paths = partition_paths(&container);
    let class_name = |class_index: u16| {
        schema.class_by_index(class_index).map_or_else(
            || format!("class {class_index}"),
            |class| class.name.clone(),
        )
    };

    let mut records = 0_u64;
    let mut written = 0_u64;
    let mut distinct_written = 0_u64;
    let mut named = 0_u64;
    let mut unwritten = 0_u64;
    // Which slot names an identifier no object is written for, and which slot
    // names one that is written - the same tally twice, so a slot that only
    // ever names the missing can be told from one that usually resolves.
    let mut slots = [BTreeMap::<String, u64>::new(), BTreeMap::new()];
    // Identifiers are not a dense counter - the highest one a record writes is
    // far above the number it writes - so "an identifier below the highest" is
    // no evidence of anything, and this records only that.
    let mut highest = 0_u64;
    // Whether another record of the same member writes what this one misses.
    let mut unwritten_in_member = 0_u64;
    let mut written_by_a_neighbour = 0_u64;
    let mut neighbour_classes = BTreeMap::<String, u64>::new();
    // An element writes more than one `GElement` record, and the export keeps
    // one of them. Per element: how many of its records carry a face with no
    // loop, and how many faces each record holds - so a record missing
    // boundaries can be checked against its own siblings.
    let mut element_records: BTreeMap<u32, Vec<(usize, usize)>> = BTreeMap::new();
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            // Every identifier this member writes an object for, and every one
            // its records name and do not write.
            let mut member_written: BTreeMap<u32, u16> = BTreeMap::new();
            let mut member_missing: BTreeSet<u32> = BTreeSet::new();
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (_, objects) =
                    rvt_model::walk_record_collecting(&schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == face_class_index)
                {
                    continue;
                }
                records += 1;
                written += objects.len() as u64;
                let record_written: BTreeMap<u32, u16> = objects
                    .iter()
                    .map(|object| (object.object_id, object.class_index))
                    .collect();
                distinct_written += record_written.len() as u64;
                member_written.extend(&record_written);

                element_records.entry(header.id).or_default().push((
                    objects
                        .iter()
                        .filter(|object| object.class_index == face_class_index)
                        .count(),
                    objects
                        .iter()
                        .filter(|object| {
                            object.class_index == face_class_index
                                && object
                                    .references
                                    .first()
                                    .is_none_or(|reference| reference.object_id == 0)
                        })
                        .count(),
                ));

                let mut record_named: BTreeSet<u32> = BTreeSet::new();
                for object in &objects {
                    for (slot, identifier) in object.identifiers.iter().enumerate() {
                        if *identifier == 0 {
                            continue;
                        }
                        record_named.insert(*identifier);
                        let resolved = record_written.contains_key(identifier);
                        *slots[usize::from(resolved)]
                            .entry(format!("{} slot {slot}", class_name(object.class_index)))
                            .or_default() += 1;
                    }
                }
                named += record_named.len() as u64;
                let missing = record_named
                    .iter()
                    .filter(|identifier| !record_written.contains_key(identifier))
                    .copied()
                    .collect::<Vec<_>>();
                unwritten += missing.len() as u64;
                member_missing.extend(&missing);

                if let Some(top) = record_written.keys().next_back() {
                    highest = highest.max(u64::from(*top));
                }
            }
            for identifier in &member_missing {
                unwritten_in_member += 1;
                if let Some(class_index) = member_written.get(identifier) {
                    written_by_a_neighbour += 1;
                    *neighbour_classes
                        .entry(class_name(*class_index))
                        .or_default() += 1;
                }
            }
        },
    )?;

    println!("Face-bearing `GElement` records: {records}");
    println!("  objects written: {written} ({distinct_written} distinct identifiers)");
    println!("  identifiers named by a bare reference: {named}, of them unwritten: {unwritten}");
    println!("  highest identifier written by any record: {highest}");
    println!(
        "  distinct unwritten identifiers per member: {unwritten_in_member}, \
         written by another record of the same member: {written_by_a_neighbour}"
    );
    let mut neighbours = neighbour_classes.into_iter().collect::<Vec<_>>();
    neighbours.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (class, count) in neighbours.into_iter().take(rows) {
        println!("  {count}\t{}", escape_terminal_text(&class));
    }
    // An element whose every record is short of boundaries has nowhere else to
    // look; one with a whole sibling has the geometry somewhere in the file.
    let mut elements = [0_u64; 4];
    let mut faces_lost = [0_u64; 2];
    for records in element_records.values() {
        let short = records.iter().filter(|(_, missing)| *missing > 0).count();
        let whole = records.len() - short;
        let index = match (short, whole) {
            (0, _) => 0,
            (_, 0) => 1,
            _ => 2,
        };
        elements[index] += 1;
        elements[3] += u64::from(records.len() > 1);
        if index != 0 {
            faces_lost[usize::from(whole > 0)] += records
                .iter()
                .map(|(_, missing)| *missing as u64)
                .sum::<u64>();
        }
    }
    println!(
        "\nElements by what their `GElement` records carry: {} whole, {} short in every record, \
         {} short in some and whole in another ({} elements write more than one record)",
        elements[0], elements[1], elements[2], elements[3]
    );
    println!(
        "  loopless faces in elements with no whole record: {}, and in elements that have one: {}",
        faces_lost[0], faces_lost[1]
    );
    println!("\nSlots naming an identifier no object is written for:");
    print_scalar_rows(&slots[0], rows);
    println!("\nSlots naming an identifier that is written:");
    print_scalar_rows(&slots[1], rows);
    Ok(())
}

/// How a `Face` reaches the `EdgeLoop` that bounds it.
///
/// `assemble_face` reads the loop from the face's own first reference, and
/// where that reference is missing the face is dropped: on AR S1 that is
/// 76 510 of the 80 205 excluded faces, the largest exclusion by an order of
/// magnitude. Every `EdgeLoop` also declares the face it bounds in
/// `identifiers[0]` - `walk_loop` already refuses a loop whose `pFace` is not
/// its face - so the same link exists on the loop's side, and this measures
/// whether reading it backwards is the same link.
///
/// Two questions, and the first has to be answered before the second means
/// anything. On faces that *do* carry a reference the inverse map must
/// reproduce it: that is the control, and a disagreement there refutes the
/// map. Only then does the count of loops claiming a face that carries no
/// reference say what could be recovered. Both are split on whether the
/// record's declarations tiled its body exactly, because a drifted walk hands
/// this arbitrary bytes rather than the fields it names.
#[allow(clippy::too_many_lines)] // One streaming pass and the tallies it fills.
fn loop_owner_probe(path: &Path, rows: usize, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let classes = brep_class_indexes(Some(&schema));
    let body_classes = brep_body_classes(Some(&schema));
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement");
    let (Some(classes), Some(geometry_element_class_index)) =
        (classes, geometry_element_class_index)
    else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare the boundary-representation classes",
        )));
    };
    let partition_paths = partition_paths(&container);

    // Every count is (all records, exactly-tiled records only).
    let mut faces = [0_u64; 2];
    let mut referenced = [0_u64; 2];
    let mut control = BTreeMap::<&'static str, [u64; 2]>::new();
    let mut unreferenced = BTreeMap::<&'static str, [u64; 2]>::new();
    // What the first reference of a face that carries no usable one actually
    // holds, so the sentinel can be recognised rather than guessed at.
    let mut first_reference = BTreeMap::<String, u64>::new();
    let mut excluded_without_a_loop = [0_u64; 2];
    let mut recoverable = [0_u64; 2];
    // How many further references a face with a null first one carries, and
    // whether such faces come a whole record at a time or mixed in among
    // faces that do reach a loop.
    let mut reference_counts = BTreeMap::<usize, u64>::new();
    let mut record_split = BTreeMap::<&'static str, u64>::new();
    // What a face with no loop still names: its surface, and whether that
    // surface or its own identifier is one a loop-bearing face in the same
    // record already used.
    let mut null_surface_class = BTreeMap::<String, u64>::new();
    let mut shares_a_surface = 0_u64;
    let mut shares_an_identifier = 0_u64;
    // The scalar fields a face declares - `m_cutType`, `m_faceFlags_v9` - for
    // each kind of face. If the two kinds differ in a declared field, the file
    // is saying what the loopless ones are.
    let mut null_scalars = BTreeMap::<String, u64>::new();
    let mut loop_bearing_scalars = BTreeMap::<String, u64>::new();
    let mut loop_bearing_reference_counts = BTreeMap::<usize, u64>::new();
    // Who names each kind of face, by the class of the naming object and the
    // slot the name sits in. A face is reached by the walk because something
    // referenced it, and if the two kinds are reached from different places
    // that is what they are.
    // Whether a `GBRep` node holds a shell or a set of free surfaces. Every
    // `Edge` names the two faces it separates, so the solid's faces are the
    // ones some edge names; a node no edge reaches declares no shell. What
    // such a node would be is already written down in `declared_bodies`: one
    // node per free surface beside the solid, which is how a wall stores the
    // planes of its compound structure. If the loopless faces fall that way -
    // a whole node at a time - they are not a boundary the walk failed to
    // read, they are a different declaration being counted as one.
    // The mark that already separates a wall's own faces from the cut
    // geometry joined into its shell, asked of both kinds of face - see
    // `FACE_INSIDE_THE_BOX_FLAG`. If the loopless faces are the cut side, the
    // file has already said so and this is not a boundary to go looking for.
    // Whether a face that declares no loop is nonetheless ordered by the file.
    // `GEdge` names, for each of the two faces it separates, the next edge
    // around that face - `identifiers[2 + side]`. Where those are live the ring
    // is declared and the endpoint reconstruction is not needed; where they are
    // null there is nothing to read and an ambiguous corner has to be refused.
    // This is the question to settle before anything is invented for the 4 353
    // faces that corner refuses.
    let mut loopless_next_links = BTreeMap::<&'static str, u64>::new();
    let mut loopless_flags = BTreeMap::<String, u64>::new();
    let mut loop_bearing_flags = BTreeMap::<String, u64>::new();
    let mut node_kind = BTreeMap::<&'static str, u64>::new();
    let mut node_faces_by_kind = BTreeMap::<&'static str, u64>::new();
    // Faces per node, for the nodes no edge reaches: a free surface should be
    // one face, and a count that is not says the reading is something else.
    let mut free_node_faces = BTreeMap::<usize, u64>::new();
    let mut free_nodes_per_record = BTreeMap::<usize, u64>::new();
    // The loopless faces, split by whether their node holds a shell. Only the
    // ones in a shell node are a boundary this walk owes an answer for.
    let mut loopless_by_node = BTreeMap::<&'static str, u64>::new();
    let mut null_parents = BTreeMap::<String, u64>::new();
    let mut loop_bearing_parents = BTreeMap::<String, u64>::new();
    // Whether the shell the loop-bearing faces make is self-contained. Every
    // `Edge` names the two faces it separates, so an edge naming a loopless
    // face says that face is part of the solid and its boundary is missing;
    // no such edge says the loopless faces are not in the shell at all.
    let mut edges_naming = BTreeMap::<&'static str, u64>::new();
    // Whether a face's boundary can be rebuilt from the edges alone. Every
    // `Edge` names the two faces it separates and, for each of them, the next
    // edge around that face, so the ring is in the edges and the `EdgeLoop`
    // object is only an entry point into it. Asked of both kinds of face: on
    // the ones that have a loop the rebuild must reproduce it, and on the ones
    // that have none it is the only route there is.
    let mut rings = [BTreeMap::<String, u64>::new(), BTreeMap::new()];
    // Identifiers are record-local, so two objects of one class sharing one
    // identifier inside a record would make every map here cross two objects.
    let mut duplicate_identifiers = BTreeMap::<&'static str, u64>::new();
    // What a ring's last edge steps onto when it is not one of the loops that
    // claim the face: an object of some other class, or nothing this record
    // holds at all.
    let mut terminators = BTreeMap::<String, u64>::new();
    let mut terminator_range = BTreeMap::<&'static str, u64>::new();
    // How many edges each ring has, split on whether an `EdgeLoop` of the
    // record ends it. A ring of one or two edges is a broken chain rather than
    // a hole, so this is the check on the rings no loop knows about.
    let mut ring_lengths = [BTreeMap::<usize, u64>::new(), BTreeMap::new()];
    // The same rebuild in three numbers per kind of face: rings recovered,
    // no edge to recover them from, and a rebuild that failed outright.
    let mut rebuilt_tally = [[0_u64; 3]; 2];
    // Rings recovered beyond the loops the record actually holds - a face
    // whose holes are in the edges and in no `EdgeLoop` object.
    let mut rings_beyond_the_loops = [0_u64; 2];
    // What ordering the edges of a loopless face geometrically could reach.
    // A body is emitted whole or not at all, so the population that matters is
    // records, not faces: a record where one loopless face has no edge to
    // order cannot be completed however well the others go.
    let mut short_records = 0_u64;
    let mut every_loopless_face_has_edges = 0_u64;
    let mut short_only_for_want_of_a_loop = 0_u64;
    let mut recoverable_records = 0_u64;
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (record_walk, objects) =
                    rvt_model::walk_record_collecting(&schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == classes.face)
                {
                    continue;
                }
                let exact = record_walk.is_exact();
                let bump = |counter: &mut [u64; 2]| {
                    counter[0] += 1;
                    counter[1] += u64::from(exact);
                };

                // Which loops claim each face, read from `EdgeLoop.pFace`. A
                // loop that does not declare all three of pFace/next/prev is
                // one `walk_loop` would refuse anyway, so it claims nothing.
                let mut claimed: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
                for object in &objects {
                    if object.class_index != classes.edge_loop || object.identifiers.len() != 3 {
                        continue;
                    }
                    claimed
                        .entry(object.identifiers[0])
                        .or_default()
                        .push(object.object_id);
                }

                for face in objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                {
                    bump(&mut faces);
                    let claiming = claimed.get(&face.object_id).map_or(&[][..], Vec::as_slice);
                    if let Some(reference) = face
                        .references
                        .first()
                        .filter(|reference| reference.object_id != 0)
                    {
                        bump(&mut referenced);
                        let verdict = match claiming {
                            [] => "no loop claims the face",
                            [only] if *only == reference.object_id => {
                                "one loop, and it is the referenced one"
                            }
                            [_] => "one loop, and it is not the referenced one",
                            many if many.contains(&reference.object_id) => {
                                "several loops, the referenced one among them"
                            }
                            _ => "several loops, none of them the referenced one",
                        };
                        bump(control.entry(verdict).or_default());
                    } else {
                        let verdict = match claiming {
                            [] => "no loop claims the face",
                            [_] => "exactly one loop claims the face",
                            _ => "several loops claim the face",
                        };
                        bump(unreferenced.entry(verdict).or_default());
                        *reference_counts.entry(face.references.len()).or_default() += 1;
                        *first_reference
                            .entry(face.references.first().map_or_else(
                                || "no references at all".to_owned(),
                                |reference| {
                                    format!(
                                        "first reference (id {}, class {})",
                                        reference.object_id, reference.class_index
                                    )
                                },
                            ))
                            .or_default() += 1;
                    }
                }

                // Every object naming a face of this record, by the class
                // that names it and the slot the name sits in.
                let mut named_by: BTreeMap<u32, Vec<String>> = BTreeMap::new();
                for object in &objects {
                    for (slot, reference) in object.references.iter().enumerate() {
                        if reference.object_id == 0 || reference.class_index != classes.face {
                            continue;
                        }
                        named_by
                            .entry(reference.object_id)
                            .or_default()
                            .push(format!(
                                "{} slot {slot}",
                                schema.class_by_index(object.class_index).map_or_else(
                                    || format!("class {}", object.class_index),
                                    |class| class.name.clone()
                                )
                            ));
                    }
                }

                // Which faces an edge names, and which faces each node holds.
                let mut edge_named: BTreeSet<u32> = BTreeSet::new();
                for edge in objects.iter().filter(|object| {
                    object.class_index == classes.edge && object.identifiers.len() == 6
                }) {
                    for face in &edge.identifiers[..2] {
                        if *face != 0 {
                            edge_named.insert(*face);
                        }
                    }
                }
                let mut node_of_face: BTreeMap<u32, u32> = BTreeMap::new();
                let mut faces_of_node: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
                for node in objects
                    .iter()
                    .filter(|object| body_classes.contains(&object.class_index))
                {
                    for reference in &node.references {
                        if reference.class_index != classes.face || reference.object_id == 0 {
                            continue;
                        }
                        node_of_face
                            .entry(reference.object_id)
                            .or_insert(node.object_id);
                        faces_of_node
                            .entry(node.object_id)
                            .or_default()
                            .push(reference.object_id);
                    }
                }
                let mut edges_of_face: BTreeMap<u32, Vec<(u32, usize)>> = BTreeMap::new();
                for edge in objects.iter().filter(|object| {
                    object.class_index == classes.edge && object.identifiers.len() == 6
                }) {
                    for (side, face) in edge.identifiers[..2].iter().enumerate() {
                        if *face != 0 {
                            edges_of_face
                                .entry(*face)
                                .or_default()
                                .push((edge.object_id, side));
                        }
                    }
                }
                let by_edge_id: BTreeMap<u32, &rvt_model::SerialObject> = objects
                    .iter()
                    .filter(|object| object.class_index == classes.edge)
                    .map(|object| (object.object_id, object))
                    .collect();
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_none_or(|reference| reference.object_id == 0)
                }) {
                    let Some(incidences) = edges_of_face.get(&face.object_id) else {
                        continue;
                    };
                    let live = incidences
                        .iter()
                        .filter(|(edge, side)| {
                            by_edge_id.get(edge).is_some_and(|edge| {
                                edge.identifiers
                                    .get(2 + side)
                                    .is_some_and(|next| *next != 0)
                            })
                        })
                        .count();
                    let verdict = if live == incidences.len() {
                        "every edge names the next one around this face"
                    } else if live == 0 {
                        "no edge names a next one around this face"
                    } else {
                        "some do and some do not"
                    };
                    *loopless_next_links.entry(verdict).or_default() += 1;
                }
                let mut free_nodes = 0_usize;
                let mut free_node_ids: BTreeSet<u32> = BTreeSet::new();
                for (node, node_face_ids) in &faces_of_node {
                    let bounded = node_face_ids
                        .iter()
                        .filter(|face| edge_named.contains(face))
                        .count();
                    let kind = if bounded == node_face_ids.len() {
                        "every face of the node is named by an edge"
                    } else if bounded == 0 {
                        "no face of the node is named by an edge"
                    } else {
                        "some faces of the node are named by an edge and some are not"
                    };
                    *node_kind.entry(kind).or_default() += 1;
                    *node_faces_by_kind.entry(kind).or_default() += node_face_ids.len() as u64;
                    if bounded == 0 {
                        free_nodes += 1;
                        free_node_ids.insert(*node);
                        *free_node_faces.entry(node_face_ids.len()).or_default() += 1;
                    }
                }
                *free_nodes_per_record.entry(free_nodes).or_default() += 1;
                for face in objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                {
                    let loopless = face
                        .references
                        .first()
                        .is_none_or(|reference| reference.object_id == 0)
                        && !edge_named.contains(&face.object_id);
                    // Read through `GFaceMarks` rather than off an index, so
                    // this asks the same question `place_body_less_its_cut_faces`
                    // asks and a face whose walk was truncated is not read as
                    // one declaring zeroes.
                    let marks = GFaceMarks::read(face);
                    let key = format!(
                        "{} / {:?}",
                        match marks {
                            Some(marks) if marks.info_flags & FACE_INSIDE_THE_BOX_FLAG != 0 =>
                                "inside the box",
                            Some(_) => "not inside the box",
                            None => "the face's marks did not all read",
                        },
                        face.alternate_integers
                    );
                    *if loopless {
                        loopless_flags.entry(key).or_default()
                    } else {
                        loop_bearing_flags.entry(key).or_default()
                    } += 1;
                }
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_none_or(|reference| reference.object_id == 0)
                        && !edge_named.contains(&object.object_id)
                }) {
                    let verdict = match node_of_face.get(&face.object_id) {
                        None => "no node names the face",
                        Some(node) if free_node_ids.contains(node) => "in a node no edge reaches",
                        Some(_) => "in a node that holds a shell",
                    };
                    *loopless_by_node.entry(verdict).or_default() += 1;
                }

                // Every surface and identifier a loop-bearing face in this
                // record claims, to test the null-loop faces against.
                let mut loop_bearing_surfaces = BTreeSet::new();
                let mut loop_bearing_faces = BTreeSet::new();
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_some_and(|reference| reference.object_id != 0)
                }) {
                    loop_bearing_faces.insert(face.object_id);
                    *loop_bearing_reference_counts
                        .entry(face.references.len())
                        .or_default() += 1;
                    *loop_bearing_scalars.entry(face_scalars(face)).or_default() += 1;
                    *loop_bearing_parents
                        .entry(named_by.get(&face.object_id).map_or_else(
                            || "nothing names it".to_owned(),
                            |names| names.join(", "),
                        ))
                        .or_default() += 1;
                    if let Some(surface) = face.references.last() {
                        loop_bearing_surfaces.insert((surface.object_id, surface.class_index));
                    }
                }
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_none_or(|reference| reference.object_id == 0)
                }) {
                    let surface = face.references.last();
                    *null_surface_class
                        .entry(surface.map_or_else(
                            || "no surface reference".to_owned(),
                            |surface| {
                                if surface.object_id == 0 {
                                    "null surface reference".to_owned()
                                } else if surface.class_index == classes.plane {
                                    "Plane".to_owned()
                                } else if surface.class_index == classes.cyl_surf {
                                    "CylSurf".to_owned()
                                } else {
                                    format!("class {}", surface.class_index)
                                }
                            },
                        ))
                        .or_default() += 1;
                    if surface.is_some_and(|surface| {
                        loop_bearing_surfaces.contains(&(surface.object_id, surface.class_index))
                    }) {
                        shares_a_surface += 1;
                    }
                    if loop_bearing_faces.contains(&face.object_id) {
                        shares_an_identifier += 1;
                    }
                    *null_scalars.entry(face_scalars(face)).or_default() += 1;
                    *null_parents
                        .entry(named_by.get(&face.object_id).map_or_else(
                            || "nothing names it".to_owned(),
                            |names| names.join(", "),
                        ))
                        .or_default() += 1;
                }

                {
                    let mut seen = BTreeMap::<(u16, u32), u64>::new();
                    for object in &objects {
                        *seen
                            .entry((object.class_index, object.object_id))
                            .or_default() += 1;
                    }
                    for ((class_index, _), count) in seen {
                        if count > 1 {
                            *duplicate_identifiers
                                .entry(if class_index == classes.face {
                                    "Face"
                                } else if class_index == classes.edge {
                                    "Edge"
                                } else if class_index == classes.edge_loop {
                                    "EdgeLoop"
                                } else {
                                    "another class"
                                })
                                .or_default() += count - 1;
                        }
                    }
                }

                let objects_by_id: BTreeMap<u32, u16> = objects
                    .iter()
                    .map(|object| (object.object_id, object.class_index))
                    .collect();

                let loop_first_edge: BTreeMap<u32, u32> = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.edge_loop && object.identifiers.len() == 3
                    })
                    .map(|object| (object.object_id, object.identifiers[1]))
                    .collect();

                // `Edge.identifiers` is `[pFace0, pFace1, next0, next1,
                // prev0, prev1]`, so the ring around face `pFace[side]`
                // continues at `next[side]`.
                let edge_links: BTreeMap<u32, [u32; 4]> = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.edge && object.identifiers.len() == 6
                    })
                    .map(|edge| {
                        (
                            edge.object_id,
                            [
                                edge.identifiers[0],
                                edge.identifiers[1],
                                edge.identifiers[2],
                                edge.identifiers[3],
                            ],
                        )
                    })
                    .collect();
                let mut edges_of_face: BTreeMap<u32, Vec<(u32, usize)>> = BTreeMap::new();
                for (id, links) in &edge_links {
                    for (side, face) in links[..2].iter().enumerate() {
                        if *face != 0 {
                            edges_of_face.entry(*face).or_default().push((*id, side));
                        }
                    }
                }

                for face in objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                {
                    let has_a_loop = face
                        .references
                        .first()
                        .is_some_and(|reference| reference.object_id != 0);
                    let claiming = claimed.get(&face.object_id).map_or(&[][..], Vec::as_slice);
                    let verdict = match rebuild_rings(
                        face.object_id,
                        edges_of_face
                            .get(&face.object_id)
                            .map_or(&[][..], Vec::as_slice),
                        &edge_links,
                    ) {
                        Err(failure) => {
                            rebuilt_tally[usize::from(has_a_loop)]
                                [usize::from(failure.starts_with("no edge"))] += 1;
                            failure
                        }
                        Ok(rebuilt) => {
                            // Each ring ends by stepping onto the `EdgeLoop`
                            // that owns it, and each loop declares the edge
                            // its ring starts at, so agreement is checkable in
                            // both directions rather than by counting.
                            for (_, terminator, length) in &rebuilt {
                                *ring_lengths[usize::from(claiming.contains(terminator))]
                                    .entry(*length)
                                    .or_default() += 1;
                                if claiming.contains(terminator) {
                                    continue;
                                }
                                *terminator_range
                                    .entry(if *terminator == 0 {
                                        "a null link"
                                    } else if objects_by_id.contains_key(terminator) {
                                        "an object of this record"
                                    } else if *terminator == u32::MAX {
                                        "0xffffffff"
                                    } else if objects_by_id
                                        .keys()
                                        .next_back()
                                        .is_some_and(|highest| terminator <= highest)
                                    {
                                        "an unused identifier below the record's highest"
                                    } else {
                                        "above every identifier the record uses"
                                    })
                                    .or_default() += 1;
                                *terminators
                                    .entry(objects_by_id.get(terminator).map_or_else(
                                        || "no object of this record".to_owned(),
                                        |class_index| {
                                            schema.class_by_index(*class_index).map_or_else(
                                                || format!("class {class_index}"),
                                                |class| class.name.clone(),
                                            )
                                        },
                                    ))
                                    .or_default() += 1;
                            }
                            rebuilt_tally[usize::from(has_a_loop)][2] += 1;
                            rings_beyond_the_loops[usize::from(has_a_loop)] +=
                                (rebuilt.len() as u64).saturating_sub(claiming.len() as u64);
                            let ends_on_its_loop = rebuilt
                                .iter()
                                .all(|(_, terminator, _)| claiming.contains(terminator));
                            let starts_where_the_loops_say = claiming.iter().all(|loop_id| {
                                loop_first_edge.get(loop_id).is_some_and(|first| {
                                    rebuilt.iter().any(|(head, _, _)| head == first)
                                })
                            });
                            format!(
                                "{} ring(s) against {} loop(s); ends on its loop: {}; starts \
                                 where the loops say: {}",
                                rebuilt.len(),
                                claiming.len(),
                                ends_on_its_loop,
                                starts_where_the_loops_say,
                            )
                        }
                    };
                    *rings[usize::from(has_a_loop)].entry(verdict).or_default() += 1;
                }

                let loopless: BTreeSet<u32> = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.face
                            && object
                                .references
                                .first()
                                .is_none_or(|reference| reference.object_id == 0)
                    })
                    .map(|object| object.object_id)
                    .collect();
                for edge in objects
                    .iter()
                    .filter(|object| object.class_index == classes.edge)
                {
                    let named = &edge.identifiers.get(..2).unwrap_or_default();
                    let verdict = match named.iter().filter(|face| loopless.contains(face)).count()
                    {
                        0 if named.len() == 2 => "both faces bounded",
                        1 => "one face has no loop",
                        2 => "both faces have no loop",
                        _ => "edge names fewer than two faces",
                    };
                    *edges_naming.entry(verdict).or_default() += 1;
                }

                let record_faces = objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                    .count();
                let record_null = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.face
                            && object
                                .references
                                .first()
                                .is_none_or(|reference| reference.object_id == 0)
                    })
                    .count();
                *record_split
                    .entry(if record_null == 0 {
                        "every face reaches a loop"
                    } else if record_null == record_faces {
                        "no face in the record reaches a loop"
                    } else {
                        "some faces reach a loop and some do not"
                    })
                    .or_default() += 1;

                if record_null > 0 {
                    short_records += 1;
                    let all_have_edges = objects
                        .iter()
                        .filter(|object| {
                            object.class_index == classes.face
                                && object
                                    .references
                                    .first()
                                    .is_none_or(|reference| reference.object_id == 0)
                        })
                        .all(|face| edges_of_face.contains_key(&face.object_id));
                    every_loopless_face_has_edges += u64::from(all_have_edges);
                    // Whether the loop is the record's *only* complaint. A
                    // record excluded for something else as well is not
                    // completed by ordering edges.
                    let only_the_loop =
                        rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes)
                            .excluded_faces
                            .iter()
                            .all(|exclusion| exclusion.reason == "face has no first loop");
                    short_only_for_want_of_a_loop += u64::from(only_the_loop);
                    recoverable_records += u64::from(all_have_edges && only_the_loop);
                }

                // The same question asked of the exclusions themselves, which
                // is what a fix would actually buy: a face excluded for
                // carrying no first loop has already resolved its surface,
                // because `assemble_face` reads that first.
                let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes);
                for exclusion in &assembled.excluded_faces {
                    if exclusion.reason != "face has no first loop" {
                        continue;
                    }
                    bump(&mut excluded_without_a_loop);
                    if claimed
                        .get(&exclusion.face_id)
                        .is_some_and(|loops| loops.len() == 1)
                    {
                        bump(&mut recoverable);
                    }
                }
            }
        },
    )?;

    println!("Faces in face-bearing `GElement` records: {faces:?} (all, exactly tiled)");
    println!("  carrying a usable first-loop reference: {referenced:?}");
    println!("\nControl - what `EdgeLoop.pFace` says about a face that carries a reference:");
    print_loop_owner_rows(&control, rows);
    println!("\nWhat it says about a face that carries none:");
    print_loop_owner_rows(&unreferenced, rows);
    println!("\nWhat those faces carry in the first reference slot instead:");
    let mut shapes = first_reference.into_iter().collect::<Vec<_>>();
    shapes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (shape, count) in shapes.into_iter().take(rows) {
        println!("  {count}\t{}", escape_terminal_text(&shape));
    }
    println!("\n  further references such a face carries (count: faces):");
    for (count, faces) in &reference_counts {
        println!("  {count}\t{faces}");
    }
    println!(
        "\nRecords carrying a loopless face: {short_records}, of them every loopless face \
         named by an edge: {every_loopless_face_has_edges}, excluded for nothing else: \
         {short_only_for_want_of_a_loop}, both: {recoverable_records}"
    );
    println!(
        "\nRebuild in three numbers (rebuild failed, no edge names the face, rings recovered):\n  \
         a face with a loop: {:?}, rings beyond the loops the record holds: {}\n  \
         a face with none:   {:?}, rings beyond the loops the record holds: {}",
        rebuilt_tally[1], rings_beyond_the_loops[1], rebuilt_tally[0], rings_beyond_the_loops[0]
    );
    println!("\nEdges per ring - a ring an `EdgeLoop` of the record ends:");
    print_ring_lengths(&ring_lengths[1]);
    println!("  and a ring no loop of the record knows about:");
    print_ring_lengths(&ring_lengths[0]);
    println!("\nWhat kind of identifier such a ring ends on:");
    for (kind, count) in &terminator_range {
        println!("  {count}\t{kind}");
    }
    println!("\nWhere a ring ends when it is not on a loop that claims the face:");
    print_scalar_rows(&terminators, rows);
    println!("\nObjects sharing an identifier with another of their class in one record:");
    for (class, count) in &duplicate_identifiers {
        println!("  {count}\t{class}");
    }
    println!("\nRebuilding a face's boundary from the edges alone - a face with a loop:");
    print_scalar_rows(&rings[1], rows);
    println!("  and a face with none:");
    print_scalar_rows(&rings[0], rows);
    println!("\nWhat the edges say about the two kinds of face:");
    let mut naming = edges_naming.into_iter().collect::<Vec<_>>();
    naming.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in naming {
        println!("  {count}\t{verdict}");
    }
    println!("\n  what names such a face:");
    print_scalar_rows(&null_parents, rows);
    println!("\n  and what names a loop-bearing face:");
    print_scalar_rows(&loop_bearing_parents, rows);
    println!("\n  the scalars such a face declares:");
    print_scalar_rows(&null_scalars, rows);
    println!("\n  and those a loop-bearing face declares:");
    print_scalar_rows(&loop_bearing_scalars, rows);
    println!("  references a loop-bearing face carries (count: faces):");
    for (count, faces) in &loop_bearing_reference_counts {
        println!("  {count}\t{faces}");
    }
    println!("\n  the surface such a face names:");
    let mut surfaces = null_surface_class.into_iter().collect::<Vec<_>>();
    surfaces.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (class, count) in surfaces {
        println!("  {count}\t{}", escape_terminal_text(&class));
    }
    println!(
        "  sharing that surface with a loop-bearing face of the same record: {shares_a_surface}"
    );
    println!("  whose own identifier a loop-bearing face also carries: {shares_an_identifier}");
    println!("\nHow the two kinds of face fall across records:");
    let mut split = record_split.into_iter().collect::<Vec<_>>();
    split.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in split {
        println!("  {count}\t{verdict}");
    }
    println!("\nWhat each `GBRep` node holds, by whether an edge reaches its faces:");
    let mut kinds = node_kind.into_iter().collect::<Vec<_>>();
    kinds.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (kind, count) in kinds {
        let faces = node_faces_by_kind.get(kind).copied().unwrap_or_default();
        println!("  {count}\tnodes, {faces} faces\t{kind}");
    }
    println!("  faces per node, for the nodes no edge reaches (faces: nodes):");
    for (faces, nodes) in &free_node_faces {
        println!("  {faces}\t{nodes}");
    }
    println!("  such nodes per record (nodes: records):");
    for (nodes, records) in &free_nodes_per_record {
        println!("  {nodes}\t{records}");
    }
    println!("\n  what a face with no loop, but with edges, is ordered by:");
    let mut links = loopless_next_links.into_iter().collect::<Vec<_>>();
    links.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in links {
        println!("  {count}\t{verdict}");
    }
    println!("\n  the marks a face with no loop and no edge naming it carries:");
    print_scalar_rows(&loopless_flags, rows);
    println!("  and those every other face carries:");
    print_scalar_rows(&loop_bearing_flags, rows);
    println!("\n  where a face with no loop and no edge naming it sits:");
    let mut placed = loopless_by_node.into_iter().collect::<Vec<_>>();
    placed.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in placed {
        println!("  {count}\t{verdict}");
    }
    println!(
        "\nFaces excluded as \"face has no first loop\": {excluded_without_a_loop:?}, \
         of which exactly one loop claims: {recoverable:?}"
    );
    Ok(())
}

/// Ring sizes, smallest first, with everything past ten collapsed.
fn print_ring_lengths(rows: &BTreeMap<usize, u64>) {
    let mut long = 0_u64;
    for (length, count) in rows {
        if *length > 10 {
            long += count;
        } else {
            println!("  {length} edge(s)\t{count}");
        }
    }
    println!("  11+ edges\t{long}");
}

/// Walk the edges that name one face into rings, and say what came of it.
///
/// From any edge, the next edge around this face is `next[side]`, and the ring
/// closes by naming something that is not an edge of this record - the
/// `EdgeLoop` object, which is the one thing a loopless face does not have.
/// So the ring is walked until it leaves the edges, and the verdict is
/// whether the rings partition exactly the edges that name the face.
fn rebuild_rings(
    face_id: u32,
    edges: &[(u32, usize)],
    links: &BTreeMap<u32, [u32; 4]>,
) -> Result<Vec<(u32, u32, usize)>, String> {
    if edges.is_empty() {
        return Err("no edge names the face".to_owned());
    }
    // The chain a ring makes is a path, not a cycle: its last edge's next
    // names the `EdgeLoop` object rather than the first edge. So a ring has to
    // be walked from its head - the edge no other edge of this face steps onto
    // - and starting anywhere else would walk only a suffix.
    let successors: BTreeSet<u32> = edges
        .iter()
        .filter_map(|&(edge, side)| {
            let next = links[&edge][2 + side];
            links.contains_key(&next).then_some(next)
        })
        .collect();
    let heads = edges
        .iter()
        .filter(|(edge, _)| !successors.contains(edge))
        .copied()
        .collect::<Vec<_>>();
    if heads.is_empty() {
        return Err("no edge of the face begins a ring".to_owned());
    }
    let mut visited = BTreeSet::new();
    // Each ring as the edge it starts at and the identifier its last edge
    // steps onto - which should be the `EdgeLoop` that names this face.
    let mut rings = Vec::new();
    for &(start, start_side) in &heads {
        if visited.contains(&start) {
            continue;
        }
        let before = visited.len();
        let (mut edge, mut side) = (start, start_side);
        let terminator = loop {
            if !visited.insert(edge) {
                return Err("the ring came back to an edge it had already used".to_owned());
            }
            let next = links[&edge][2 + side];
            let Some(next_links) = links.get(&next) else {
                break next;
            };
            side = if next_links[0] == face_id {
                0
            } else if next_links[1] == face_id {
                1
            } else {
                return Err("the ring stepped onto an edge of another face".to_owned());
            };
            edge = next;
        };
        rings.push((start, terminator, visited.len() - before));
    }
    if visited.len() != edges.len() {
        return Err("the rings did not use every edge that names the face".to_owned());
    }
    Ok(rings)
}

/// A face's declared scalars, as one comparable key: the `Integer32` fields
/// `m_cutType` and `m_faceFlags_v9`, then whatever narrower fields the walk
/// read.
fn face_scalars(face: &rvt_model::SerialObject) -> String {
    format!("{:?} / {:?}", face.integers, face.small_integers)
}

/// One histogram of face scalars, most frequent first.
fn print_scalar_rows(rows: &BTreeMap<String, u64>, limit: usize) {
    let mut ordered = rows.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
    for (scalars, count) in ordered.into_iter().take(limit) {
        println!("  {count}\t{}", escape_terminal_text(scalars));
    }
}

/// One tally of the loop-owner probe, most frequent first, as `count (exact:
/// count)`.
fn print_loop_owner_rows(rows: &BTreeMap<&'static str, [u64; 2]>, limit: usize) {
    let mut ordered = rows.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1[0].cmp(&left.1[0]).then_with(|| left.0.cmp(right.0)));
    for (verdict, counts) in ordered.into_iter().take(limit) {
        println!("  {}\t(exact: {})\t{verdict}", counts[0], counts[1]);
    }
}

/// Face-count buckets for the openness breakdown, as `(name, largest count in
/// it)`. The single digits are called out on their own because they are the
/// shapes that say what a record is: one face is a plane, and six is the face
/// count of a box.
const FACE_BUCKETS: [(&str, usize); 7] = [
    ("1", 1),
    ("2", 2),
    ("3-5", 5),
    ("6", 6),
    ("7-12", 12),
    ("13-50", 50),
    ("51+", usize::MAX),
];

/// Which [`FACE_BUCKETS`] entry a face count falls in.
fn face_bucket(faces: usize) -> usize {
    FACE_BUCKETS
        .iter()
        .position(|(_, largest)| faces <= *largest)
        .unwrap_or(FACE_BUCKETS.len() - 1)
}

/// Tally what the boundary-representation assembly gets and what it drops,
/// over every record of a file.
///
/// The bar for a geometry rule is not that a hand-picked record improves: it is
/// that the corpus-wide face and body counts rise with no new exclusion reason
/// appearing. This reports exactly those, so a change can be diffed rather than
/// argued, and it is the cheap half of the check - the other half is
/// `ifcopenshell.geom.create_shape` on every emitted body.
#[allow(clippy::too_many_lines)] // One streaming pass plus its report.
fn brep(path: &Path, reasons: usize, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?;
    let classes = brep_class_indexes(schema.as_ref());
    let body_classes = brep_body_classes(schema.as_ref());
    // The solid lives in a `GElement` record, and only there. Reading every
    // record that merely contains a `Face` object instead would tally tens of
    // thousands of truncated node streams as exclusions and measure nothing.
    let geometry_element_class_index = schema_class_index(schema.as_ref(), "GElement");
    let (Some(schema), Some(classes), Some(geometry_element_class_index)) =
        (schema.as_ref(), classes, geometry_element_class_index)
    else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare the boundary-representation classes",
        )));
    };
    let partition_paths = partition_paths(&container);

    let mut records = 0_u64;
    let mut complete = 0_u64;
    let mut faces = 0_u64;
    let mut excluded = 0_u64;
    let mut loops = 0_u64;
    let mut edges = 0_u64;
    let mut arcs = 0_u64;
    let mut polylines = 0_u64;
    let mut why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut exact_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut edge_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut exact_edge_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut ordering_control = rvt_model::OrderingControl::default();
    let mut holes = rvt_model::HoleTally::default();
    let mut hole_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut gaps: Vec<f64> = Vec::new();
    let mut gaps_with_a_cylinder = 0_u64;
    let mut gaps_in_exact_records = 0_u64;
    // Faces the record declares that are on no boundary of it - see
    // `SymbolBrep::unbounded_faces`. Reported, because "not a failure" is not
    // the same as "not there", and a change that started counting them as
    // faces again should be visible here rather than in a shortfall elsewhere.
    let mut unbounded = 0_u64;
    let mut records_with_unbounded = 0_u64;
    // Of the records that excluded no face, how many hold faces that actually
    // close - the geometric test an exporter applies, against the topological
    // one this tally is otherwise about. The two are far apart and the gap is
    // worth having in front of whoever reads this next.
    let mut complete_and_a_volume = 0_u64;
    // And of the ones that do not, what their bodies say about why - see
    // `rvt_model::BrepOpenness`. A record is not one body, so most of this gap
    // is expected to be records carrying a solid *and* something beside it;
    // the point of the breakdown is to say how much of it is that and how much
    // is a shell with a face missing from it.
    let mut openness: BTreeMap<rvt_model::BrepOpenness, u64> = BTreeMap::new();
    let mut openness_faces: BTreeMap<rvt_model::BrepOpenness, [u64; FACE_BUCKETS.len()]> =
        BTreeMap::new();
    // Of each class, the records holding a face some of whose edges no loop of
    // theirs uses. That is a hole nobody read, and it is the one reading that
    // would leave a body closed on its edges and open on its loops without
    // anything being wrong with the edges themselves - so it separates the two
    // halves of `BrepOpenness::ClosedOnItsEdgesOnly` rather than leaving them
    // named together.
    let mut openness_short: BTreeMap<rvt_model::BrepOpenness, u64> = BTreeMap::new();
    // And of each class, the records that close topologically all the same.
    // That is the other half of the exporter's own test
    // (`is_closed() || bounds_a_volume()`), so it says which of these classes
    // is already reaching the export and which is refused outright.
    let mut openness_closed: BTreeMap<rvt_model::BrepOpenness, u64> = BTreeMap::new();
    let mut one_sided_edges = 0_u64;
    let mut edges_out_of_their_body = 0_u64;
    let mut exact_records = 0_u64;
    let mut exact_complete = 0_u64;
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (record_walk, objects) =
                    rvt_model::walk_record_collecting(schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == classes.face)
                {
                    continue;
                }
                // Whether the record's declarations tiled its body exactly. A
                // walk that drifted hands this module bytes that are not the
                // fields it thinks they are, so every number after the drift is
                // arbitrary - which is a different failure from a geometry rule
                // being wrong, and has to be counted apart from one.
                let exact = record_walk.is_exact();
                let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes);
                unbounded += assembled.unbounded_faces.len() as u64;
                records_with_unbounded += u64::from(!assembled.unbounded_faces.is_empty());
                if assembled.is_empty() && assembled.excluded_faces.is_empty() {
                    continue;
                }
                records += 1;
                exact_records += u64::from(exact);
                complete += u64::from(assembled.excluded_faces.is_empty());
                if assembled.excluded_faces.is_empty() {
                    if assembled.bounds_a_volume() {
                        complete_and_a_volume += 1;
                    } else {
                        let reading = assembled.openness();
                        *openness.entry(reading).or_default() += 1;
                        openness_faces.entry(reading).or_default()
                            [face_bucket(assembled.faces.len())] += 1;
                        *openness_short.entry(reading).or_default() +=
                            u64::from(assembled.holes.edges_short > 0);
                        *openness_closed.entry(reading).or_default() +=
                            u64::from(assembled.is_closed());
                        for body in &assembled.bodies {
                            one_sided_edges += body.one_sided_edges as u64;
                            edges_out_of_their_body += body.open_edges as u64;
                        }
                    }
                }
                exact_complete += u64::from(exact && assembled.excluded_faces.is_empty());
                faces += assembled.faces.len() as u64;
                excluded += assembled.excluded_faces.len() as u64;
                for face in &assembled.faces {
                    loops += face.loops.len() as u64;
                    for face_loop in &face.loops {
                        edges += face_loop.len() as u64;
                        arcs += face_loop
                            .iter()
                            .filter(|edge| matches!(edge.curve, rvt_model::BrepCurve::Arc(_)))
                            .count() as u64;
                        polylines += face_loop
                            .iter()
                            .filter(|edge| matches!(edge.curve, rvt_model::BrepCurve::Polyline(_)))
                            .count() as u64;
                    }
                }
                for exclusion in &assembled.excluded_faces {
                    *why.entry(exclusion.reason).or_default() += 1;
                    if exact {
                        *exact_why.entry(exclusion.reason).or_default() += 1;
                    }
                }
                ordering_control.agreed += assembled.ordering_control.agreed;
                ordering_control.refused += assembled.ordering_control.refused;
                ordering_control.contradicted += assembled.ordering_control.contradicted;
                ordering_control.not_comparable += assembled.ordering_control.not_comparable;
                holes.faces += assembled.holes.faces;
                holes.loops += assembled.holes.loops;
                holes.edges_accounted += assembled.holes.edges_accounted;
                holes.first_loop_accounted += assembled.holes.first_loop_accounted;
                holes.edges_short += assembled.holes.edges_short;
                holes.edges_over += assembled.holes.edges_over;
                for unread in &assembled.holes.unread {
                    *hole_why.entry(unread.reason).or_default() += 1;
                }
                // Whether a record holds any cylinder at all is the control for
                // the one place a face's surface is *inferred* rather than
                // read: `CylSurf` objects share one identifier, so they are
                // paired to faces by encounter order. If that pairing slips, a
                // face is evaluated against a surface that is not its own -
                // which is exactly what a cross-face disagreement looks like.
                // A record with no cylinder cannot suffer from it.
                let cylindrical =
                    assembled.faces.iter().any(|face| {
                        matches!(face.surface, rvt_model::BrepSurface::Cylinder { .. })
                    }) || objects
                        .iter()
                        .any(|object| object.class_index == classes.cyl_surf);
                for failure in &assembled.failed_edges {
                    *edge_why.entry(failure.reason).or_default() += 1;
                    if exact {
                        *exact_edge_why.entry(failure.reason).or_default() += 1;
                    }
                    if let Some(gap) = failure.gap_feet {
                        gaps.push(gap);
                        if cylindrical {
                            gaps_with_a_cylinder += 1;
                        }
                        if exact {
                            gaps_in_exact_records += 1;
                        }
                    }
                }
            }
        },
    )?;

    println!("Boundary representations assembled from face-bearing records:");
    println!("Records producing a body: {records}");
    println!("  whose declarations tiled the body exactly: {exact_records}");
    println!("  every face resolved: {complete}");
    println!("    of those, in an exactly-tiled record: {exact_complete}");
    println!("  complete records bounding a volume: {complete_and_a_volume}");
    let not_a_volume = complete - complete_and_a_volume;
    // The backlog proper: a record whose best body neither bounds a volume nor
    // pairs its curves up is one nothing here can hand an exporter, and it is
    // the only one of the classes below that a further reading could move.
    let without_a_closed_body = openness
        .iter()
        .filter(|(reading, _)| {
            !matches!(
                reading,
                rvt_model::BrepOpenness::BoundsAVolume
                    | rvt_model::BrepOpenness::ClosesCurveForCurve
            )
        })
        .map(|(_, count)| *count)
        .sum::<u64>();
    println!("  complete records that do not bound a volume: {not_a_volume}");
    println!("    of those, none of their bodies closes at all: {without_a_closed_body}");
    for (reading, count) in &openness {
        println!("    {count}\t{}", reading.label());
        let mut row = String::new();
        for (index, (name, _)) in FACE_BUCKETS.iter().enumerate() {
            let faces = openness_faces
                .get(reading)
                .map_or(0, |buckets| buckets[index]);
            let _ = write!(row, " {name}:{faces}");
        }
        println!("      faces per record:{row}");
        println!(
            "      of them, holding a face whose loops leave an edge over: {}, \
             closing on their edges: {}",
            openness_short.get(reading).copied().unwrap_or(0),
            openness_closed.get(reading).copied().unwrap_or(0)
        );
    }
    println!(
        "    edges with nothing on the far side: {one_sided_edges}, \
         edges naming a face outside their body: {edges_out_of_their_body}"
    );
    println!(
        "  faces on no boundary of their record: {unbounded}, in {records_with_unbounded} records"
    );
    println!("Faces resolved: {faces}");
    println!("  loops: {loops}, edges: {edges}, of which arcs: {arcs}, polylines: {polylines}");
    println!(
        "Faces excluded: {excluded} (in an exactly-tiled record: {})",
        exact_why.values().sum::<u64>()
    );
    let mut ordered = why.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (reason, count) in ordered.into_iter().take(reasons) {
        println!(
            "  {count}\t(exact: {})\t{}",
            exact_why.get(reason).copied().unwrap_or(0),
            escape_terminal_text(reason)
        );
    }

    // The licence for the reconstruction a loopless face is assembled by:
    // where the file declares the ring, ordering the edges by their endpoints
    // has to find the same one.
    println!(
        "Ordering a declared loop's edges by their endpoints: {} agreed, {} refused, \
         {} contradicted, {} not comparable",
        ordering_control.agreed,
        ordering_control.refused,
        ordering_control.contradicted,
        ordering_control.not_comparable
    );

    // A face's holes, and the check on them that does not come from the loop
    // chain at all: the edges name their face from their own side, so a face
    // whose loops use exactly those edges has had its whole boundary read.
    println!(
        "Faces carrying a further loop: {}, further loops read: {}",
        holes.faces, holes.loops
    );
    println!(
        "  resolved faces whose loops use exactly the edges naming them: {} (by the first loop alone: {}), {} leave edges over, {} use more",
        holes.edges_accounted, holes.first_loop_accounted, holes.edges_short, holes.edges_over
    );
    let mut ordered = hole_why.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (reason, count) in ordered.into_iter().take(reasons) {
        println!(
            "  {count}\tloop chain stopped: {}",
            escape_terminal_text(reason)
        );
    }

    // Edges, not faces. A face is excluded by the first failing edge its loop
    // reaches, so face counts say how much was lost and these say how much is
    // actually wrong.
    println!(
        "Edges that did not resolve: {} (in an exactly-tiled record: {})",
        edge_why.values().sum::<u64>(),
        exact_edge_why.values().sum::<u64>()
    );
    let mut ordered = edge_why.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (reason, count) in ordered.into_iter().take(reasons) {
        println!(
            "  {count}\t(exact: {})\t{}",
            exact_edge_why.get(reason).copied().unwrap_or(0),
            escape_terminal_text(reason)
        );
    }

    if !gaps.is_empty() {
        // How far apart the two faces put a shared EdgePnt separates a
        // tolerance that is too tight from a face evaluated against the wrong
        // surface. Reported in millimetres, which is the scale the answer
        // matters at - except that a non-finite gap is neither: it means an
        // evaluated point is infinite or NaN, so some number feeding it is not
        // a coordinate at all, and the two are counted apart.
        let total = gaps.len();
        let non_finite = gaps.iter().filter(|gap| !gap.is_finite()).count();
        let share = |part: u64| {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / total as f64
            }
        };
        gaps.retain(|gap| gap.is_finite());
        gaps.sort_by(f64::total_cmp);
        let millimetres = |feet: f64| feet * 304.8;
        println!("Cross-face EdgePnt disagreements: {total}");
        println!(
            "  in a record that holds a cylinder: {gaps_with_a_cylinder} ({:.1}%)",
            share(gaps_with_a_cylinder)
        );
        println!(
            "  in a record whose declarations tiled it exactly: {gaps_in_exact_records} ({:.1}%)",
            share(gaps_in_exact_records)
        );
        println!(
            "  the evaluated point is infinite or NaN: {non_finite} ({:.1}%)",
            share(non_finite as u64)
        );
        if !gaps.is_empty() {
            for (label, bound_mm) in [
                ("under 0.01 mm", 0.01),
                ("under 0.1 mm", 0.1),
                ("under 1 mm", 1.0),
                ("under 10 mm", 10.0),
                ("under 1 m", 1000.0),
            ] {
                let count = gaps
                    .iter()
                    .filter(|gap| millimetres(**gap) < bound_mm)
                    .count();
                println!("  finite and {label}: {count}");
            }
            let quantile = |q: f64| {
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let index = ((gaps.len() - 1) as f64 * q).round() as usize;
                millimetres(gaps[index])
            };
            println!(
                "  finite: median {:.4} mm, 90th {:.4} mm, max {:.4} mm",
                quantile(0.5),
                quantile(0.9),
                millimetres(*gaps.last().unwrap_or(&0.0))
            );
        }
    }
    Ok(())
}

/// Millimetres in one Revit internal foot.
const MILLIMETRES_PER_FOOT: f64 = 304.8;

/// What one candidate type-reference property named, for one class that
/// declares it: how many values it carried, what classes they named, and how
/// many distinct elements they named between them.
#[derive(Default)]
struct TypeLinkTally<'a> {
    values: u64,
    named_classes: BTreeMap<&'a str, u64>,
    named_elements: BTreeSet<i32>,
}

/// Report the layer table compound host object types carry, and score every
/// candidate type-reference property by the class of the element it names.
///
/// Both halves are measurements rather than output. A type reference is
/// credible when its values land on one class out of the file's thousands, and
/// a layer table is credible when its widths and materials reproduce an answer
/// this decode did not produce: Revit's own IFC export of the same model
/// carries an `IfcMaterialConstituentSet` per wall, with the layers in order,
/// the material names, and each layer's share of the total width. `--links`
/// prints the element-to-type pairs so that join can be made.
fn layers(
    path: &Path,
    types: usize,
    links_wanted: bool,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let recovered = recover_elements(path, max_member_bytes)?;
    let schema = recovered.schema.as_ref();
    let elements = &recovered.elements;
    let class_of = |id: u32| -> Option<&str> {
        elements
            .get(&id)
            .and_then(|element| element_class_name(element, schema))
    };
    let name_of = |id: u32| -> Option<&str> {
        elements
            .get(&id)
            .and_then(|element| element.name.as_ref())
            .map(|(name, _)| name.as_str())
    };

    report_type_links(elements, schema, &class_of);
    if links_wanted {
        println!();
        for (id, element) in elements {
            let (property, target) = match (element.type_element_property, element.type_element_id)
            {
                (Some(property), Some(target)) => (property, target),
                // A record with no type reference still reports its family and
                // its category, which is the other half of the same join.
                _ if element.family_element_id.is_some()
                    || element.declared_category_id.is_some() =>
                {
                    ("-", -1)
                }
                _ => continue,
            };
            let target_id = u32::try_from(target).ok();
            println!(
                "LINK {id}\t{owner}\t{property}\t{target}\t{class}\t{name}\t{family}\t{category}",
                owner = element_class_name(element, schema).unwrap_or("-"),
                class = target_id.and_then(&class_of).unwrap_or("-"),
                name = target_id.and_then(&name_of).unwrap_or("-"),
                family = element.family_element_id.unwrap_or(-1),
                category = element.declared_category_id.unwrap_or(-1),
            );
        }
    }
    report_family_categories(elements, schema, &class_of);
    report_layer_carriers(elements, schema);
    report_layer_tables(elements, schema, types, &name_of);
    Ok(())
}

/// The class of the element's own record, when the schema names one.
fn element_class_name<'a>(
    element: &ExportedElement,
    schema: Option<&'a Schema>,
) -> Option<&'a str> {
    element
        .class_index
        .and_then(|index| schema?.class_by_index(index))
        .map(|class| class.name.as_str())
}

/// What each candidate property names, per class that declares it. A real type
/// reference lands on one class out of the file's thousands; a misread field
/// scatters across them.
fn report_type_links<'a>(
    elements: &'a BTreeMap<u32, ExportedElement>,
    schema: Option<&'a Schema>,
    class_of: &impl Fn(u32) -> Option<&'a str>,
) {
    let mut links: BTreeMap<(&str, &str), TypeLinkTally<'a>> = BTreeMap::new();
    for element in elements.values() {
        let (Some(property), Some(target)) =
            (element.type_element_property, element.type_element_id)
        else {
            continue;
        };
        let Some(owner) = element_class_name(element, schema) else {
            continue;
        };
        let row = links.entry((property, owner)).or_default();
        row.values += 1;
        *row.named_classes
            .entry(
                u32::try_from(target)
                    .ok()
                    .and_then(class_of)
                    .unwrap_or("<no such element>"),
            )
            .or_default() += 1;
        row.named_elements.insert(target);
    }
    println!("Type references, by the property that carried them:");
    let mut rows = links.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, row)| std::cmp::Reverse(row.values));
    for ((property, owner), row) in rows {
        let named = row
            .named_classes
            .iter()
            .map(|(class, count)| format!("{class} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {property}\ton {owner}: {values} values naming {distinct} elements - {named}",
            values = row.values,
            distinct = row.named_elements.len(),
        );
    }
}

/// The route a loadable family's category takes: symbol -> family -> category,
/// and what each hop reaches. See `inherit_family_categories`.
fn report_family_categories<'a>(
    elements: &'a BTreeMap<u32, ExportedElement>,
    schema: Option<&'a Schema>,
    class_of: &impl Fn(u32) -> Option<&'a str>,
) {
    let mut families: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    let mut declaring: BTreeMap<&str, u64> = BTreeMap::new();
    let mut sources: BTreeMap<&str, u64> = BTreeMap::new();
    let mut without = 0_u64;
    for element in elements.values() {
        let owner = element_class_name(element, schema).unwrap_or("-");
        if let Some(family) = element.family_element_id {
            let named = u32::try_from(family)
                .ok()
                .and_then(class_of)
                .unwrap_or("<no such element>");
            *families.entry((owner, named)).or_default() += 1;
        }
        if element.declared_category_id.is_some() {
            *declaring.entry(owner).or_default() += 1;
        }
        match element.category_source {
            Some(source) => *sources.entry(source).or_default() += 1,
            None if element.category.is_some() => *sources.entry("-").or_default() += 1,
            None => without += 1,
        }
    }
    println!();
    println!("Family references, by the class that declares one and the class it names:");
    let mut rows = families.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for ((owner, named), count) in rows.into_iter().take(8) {
        println!("  m_familyId	on {owner}: {count} naming {named}");
    }
    println!("Records declaring a category identifier:");
    let mut rows = declaring.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (owner, count) in rows.into_iter().take(8) {
        println!("  m_categoryId	on {owner}: {count}");
    }
    println!("Categories, by where they came from:");
    for (source, count) in sources {
        println!("  {source}: {count}");
    }
    println!("  no category: {without}");
}

/// Every class that can carry a layer table, whether or not one was read, so a
/// class that carries none is visible rather than absent.
fn report_layer_carriers(elements: &BTreeMap<u32, ExportedElement>, schema: Option<&Schema>) {
    let mut carriers: BTreeMap<&str, (u64, u64, BTreeMap<usize, u64>)> = BTreeMap::new();
    for element in elements.values() {
        let Some(class) = element_class_name(element, schema) else {
            continue;
        };
        let host_object_type = element.class_index.is_some_and(|index| {
            schema.is_some_and(|schema| {
                rvt_model::descends_from(
                    schema,
                    index,
                    rvt_model::HOST_OBJECT_ATTRIBUTES_CLASS_NAME,
                )
            })
        });
        if !host_object_type {
            continue;
        }
        let row = carriers.entry(class).or_default();
        row.0 += 1;
        if let Some(structure) = element.compound_structures.first() {
            row.1 += 1;
            *row.2.entry(structure.layers.len()).or_default() += 1;
        }
    }
    println!();
    println!("Layer tables, by the class of the type that carries them:");
    let mut rows = carriers.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, row)| std::cmp::Reverse(row.0));
    for (class, (ids, read, histogram)) in rows {
        let counts = histogram
            .iter()
            .map(|(layers, ids)| format!("{layers}:{ids}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!("  {class}\tids={ids}\twith a layer table={read}\tlayers {counts}");
    }
}

/// The tables themselves, most layers first: this is what an independent
/// answer is compared against.
fn report_layer_tables<'a>(
    elements: &'a BTreeMap<u32, ExportedElement>,
    schema: Option<&'a Schema>,
    types: usize,
    name_of: &impl Fn(u32) -> Option<&'a str>,
) {
    let mut listed = elements
        .iter()
        .filter_map(|(id, element)| {
            element
                .compound_structures
                .first()
                .map(|structure| (*id, element, structure))
        })
        .collect::<Vec<_>>();
    listed.sort_by(|left, right| {
        right
            .2
            .layers
            .len()
            .cmp(&left.2.layers.len())
            .then(left.0.cmp(&right.0))
    });
    println!();
    println!(
        "Decoded layer tables: {} of {} shown, widths in millimetres",
        listed.len().min(types),
        listed.len()
    );
    for (id, element, structure) in listed.iter().take(types) {
        println!();
        println!(
            "TYPE {id}\t{class}\t{name}\tlayers={layers}\tcore=[{exterior},{interior}]\ttotal={total:.1}\tpattern={pattern}\tendCap={end_cap}\twrap={wrap}",
            class = element_class_name(element, schema).unwrap_or("-"),
            name = name_of(*id).unwrap_or("-"),
            layers = structure.layers.len(),
            exterior = structure.shell_layers_exterior,
            interior = structure.shell_layers_interior,
            total = structure.total_width_feet() * MILLIMETRES_PER_FOOT,
            pattern = structure.coarse_scale_fill_pattern_id.unwrap_or(-1),
            end_cap = structure.end_cap,
            wrap = structure.opening_wrapping,
        );
        for (index, layer) in structure.layers.iter().enumerate() {
            // Its own column: a marker appended to the material name would be
            // read back as part of the name by anything joining this report
            // against another answer.
            let structural = if structure.structural_layer_index == Some(index) {
                "\tstructural"
            } else {
                ""
            };
            println!(
                "  LAYER {index}\t{width:.1}\tfunction={function}\tmaterial={material_id}\t{material}{structural}",
                width = layer.width_feet * MILLIMETRES_PER_FOOT,
                function = layer.function,
                material_id = layer.material_id.unwrap_or(-1),
                material = layer
                    .material_id
                    .and_then(|id| u32::try_from(id).ok())
                    .and_then(name_of)
                    .unwrap_or("-"),
            );
        }
    }
}

fn names(path: &Path, classes: usize, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?;
    let partition_paths = partition_paths(&container);
    let (calibrations, _) = calibrate_names(
        &container,
        schema.as_ref(),
        &partition_paths,
        max_member_bytes,
    )?;

    let mut ordered = calibrations.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        right
            .1
            .bodies
            .cmp(&left.1.bodies)
            .then_with(|| left.0.cmp(right.0))
    });

    println!("String-offset calibration:");
    println!(
        "A class is accepted when at least {NAME_OFFSET_AGREEMENT}% of its records place their first readable string at the same offset after the Element tail."
    );
    println!();
    let mut settled = 0_usize;
    for (index, calibration) in ordered.iter().take(classes) {
        let name = schema
            .as_ref()
            .and_then(|schema| schema.class_by_index(**index))
            .map_or("?", |class| class.name.as_str());
        let offset = calibration
            .settled_offset()
            .map_or_else(|| "-".to_owned(), |offset| format!("+{offset}"));
        println!(
            "{index}\t{}\tbodies={}\toffset={offset}\tagreement={}%\tsamples={:?}",
            escape_terminal_text(name),
            calibration.bodies,
            calibration.agreement(),
            calibration.samples
        );
    }
    for calibration in calibrations.values() {
        settled += usize::from(calibration.settled_offset().is_some());
    }
    println!();
    println!("Classes seen: {}", calibrations.len());
    println!("Classes with a settled string offset: {settled}");

    let Some(schema) = schema.as_ref() else {
        return Ok(());
    };
    report_declared_names(
        &container,
        schema,
        &partition_paths,
        &calibrations,
        &ordered,
        classes,
        max_member_bytes,
    )
}

/// Measure the name the declarations give a record against the calibrated
/// scan it replaces. The declarations say which property a string is the value
/// of, so a name needs no offset agreement to be located.
fn report_declared_names(
    container: &RvtContainer,
    schema: &Schema,
    partition_paths: &[String],
    calibrations: &BTreeMap<u16, NameCalibration>,
    ordered: &[(&u16, &NameCalibration)],
    classes: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let mut declared: BTreeMap<u16, DeclaredNameTally> = BTreeMap::new();
    for_each_member(
        container,
        partition_paths,
        max_member_bytes,
        |_, _, format_tag, layout, walk, payload| {
            if format_tag != ELEMENT_CLASS_FORMAT_TAG {
                return;
            }
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if !calibrations.contains_key(&header.class_index) {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let tally = declared.entry(header.class_index).or_default();
                tally.bodies += 1;
                // The reader itself answers, so the report cannot drift from
                // what the export actually uses.
                let Some(first) = rvt_model::record_name_string(schema, header.class_index, body)
                else {
                    continue;
                };
                tally.with_string += 1;
                *tally
                    .properties
                    .entry(format!("{}.{}", first.class, first.property))
                    .or_default() += 1;
                let scanned = ElementFields::parse(body, header.id).and_then(|fields| {
                    read_name(
                        body,
                        fields.id_offset + 4 + ELEMENT_TAIL_BYTES,
                        calibrations
                            .get(&header.class_index)
                            .and_then(NameCalibration::settled_offset),
                    )
                });
                if let Some((scanned, _)) = scanned {
                    tally.comparable += 1;
                    if scanned == first.value {
                        tally.agreed += 1;
                    } else {
                        // A disagreement is only evidence once it says what
                        // the scan was reading instead. Name the declaration
                        // whose value the scan returned, so "the scan is one
                        // string early" can be told apart from "the two
                        // readings found unrelated bytes".
                        let (_walk, strings) =
                            rvt_model::walk_record_strings(schema, header.class_index, body);
                        let matched = strings
                            .iter()
                            .find(|string| string.value == scanned)
                            .map_or_else(
                                || "(no declared string)".to_owned(),
                                |string| format!("{}.{}", string.class, string.property),
                            );
                        *tally.scanned_properties.entry(matched).or_default() += 1;
                    }
                }
                if tally.samples.len() < 3 {
                    tally.samples.push(first.value.clone());
                }
            }
        },
    )?;

    println!();
    println!("The name a record's declarations give it, by class:");
    for (index, _) in ordered.iter().take(classes) {
        let Some(tally) = declared.get(index) else {
            continue;
        };
        let name = schema
            .class_by_index(**index)
            .map_or("?", |class| class.name.as_str());
        let property = tally
            .properties
            .iter()
            .max_by_key(|(_, count)| **count)
            .map_or_else(|| "-".to_owned(), |(property, _)| property.clone());
        println!(
            "{index}\t{}\tbodies={}\twith a string={}\tproperty={}\tagrees with the scan={}/{}\tsamples={:?}",
            escape_terminal_text(name),
            tally.bodies,
            tally.with_string,
            escape_terminal_text(&property),
            tally.agreed,
            tally.comparable,
            tally.samples
        );
        let mut scanned = tally.scanned_properties.iter().collect::<Vec<_>>();
        scanned.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        for (property, count) in scanned.iter().take(3) {
            println!(
                "\t\tthe scan read {} instead in {count}",
                escape_terminal_text(property)
            );
        }
    }
    Ok(())
}

/// Where a class's records keep the first string their declarations read.
#[derive(Default)]
struct DeclaredNameTally {
    bodies: usize,
    with_string: usize,
    /// The declaring `class.property`, counted so a class with more than one
    /// answer shows it rather than hiding behind the first record.
    properties: BTreeMap<String, usize>,
    /// Records where the scan also returned a name.
    comparable: usize,
    /// Of those, records where the two agree.
    agreed: usize,
    /// Of the rest, the declaration whose value the scan returned instead.
    scanned_properties: BTreeMap<String, usize>,
    samples: Vec<String>,
}

/// Walk every record of one class against the schema and report how far the
/// declared properties explain the body. A body is only "exact" when the
/// declarations tile it with nothing left over.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // One pass plus its report.
fn serial_probe(
    path: &Path,
    class_name: &str,
    stream: bool,
    record_mode: bool,
    rows: usize,
    dump_remaining: Option<usize>,
    dump_stop: Option<&str>,
    trace_id: Option<u32>,
    dump_body: Option<u32>,
    element: Option<u32>,
    dump_window: usize,
    dump_count: usize,
    faces_only: bool,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let class_index = schema_class_index(Some(&schema), class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("schema class not found: {class_name}"),
        )
    })?;
    let partition_paths = partition_paths(&container);

    let mut attempted = 0_usize;
    let mut exact = 0_usize;
    let mut trailing = 0_usize;
    let mut stopped = 0_usize;
    let mut stops: BTreeMap<String, usize> = BTreeMap::new();
    let mut trailing_bytes: BTreeMap<usize, usize> = BTreeMap::new();
    let mut references = 0_usize;
    let mut objects = 0_usize;
    let mut pending = 0_usize;
    let mut dumped = 0_usize;
    let mut trailer_matches = 0_usize;
    let mut boundary_records = 0_usize;
    let mut boundary_exact = 0_usize;
    let mut boundary_faces = 0_usize;
    let mut boundary_exact_faces = 0_usize;
    let mut drained = 0_usize;
    let mut drained_exact = 0_usize;

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                attempted += 1;
                let header_walk = rvt_model::walk_object(&schema, class_index, body);
                if dump_remaining == Some(header_walk.remaining)
                    && header_walk.stop.is_none()
                    && dumped < dump_count
                {
                    dumped += 1;
                    let mut named = String::new();
                    for reference in &header_walk.references {
                        if !named.is_empty() {
                            named.push(' ');
                        }
                        let class = schema
                            .class_by_index(reference.class_index)
                            .map_or("<unknown>", |class| class.name.as_str());
                        let _ = write!(named, "{}:{class}", reference.object_id);
                    }
                    let mut tail = String::new();
                    for byte in body.get(header_walk.consumed..).unwrap_or_default() {
                        let _ = write!(tail, "{byte:02x}");
                    }
                    println!(
                        "  id={} len={} header={} refs=[{named}]",
                        header.id,
                        body.len(),
                        header_walk.consumed
                    );
                    println!("    tail={tail}");
                }
                if dump_body == Some(header.id) {
                    let mut hexed = String::new();
                    for byte in body {
                        let _ = write!(hexed, "{byte:02x}");
                    }
                    println!("body id={} len={} {hexed}", header.id, body.len());
                }
                if trace_id == Some(header.id) && dumped == 0 {
                    dumped += 1;
                    let (result, trace) = rvt_model::walk_record_traced(&schema, class_index, body);
                    println!("Trace of record {} ({} bytes):", header.id, body.len());
                    for entry in &trace {
                        println!(
                            "  {:6} +{:<4} {}.{}",
                            entry.offset, entry.consumed, entry.class, entry.property
                        );
                    }
                    println!(
                        "  stop_at={} {}",
                        result.stop_offset,
                        result
                            .stop
                            .as_ref()
                            .map_or_else(|| "none".to_owned(), describe_serial_stop)
                    );
                }
                if element == Some(header.id) {
                    let (result, objects) =
                        rvt_model::walk_record_collecting(&schema, class_index, body);
                    report_boundary_topology(&schema, header.id, body.len(), &result, &objects);
                    report_declared_bodies(&schema, body, &objects);
                }
                if record_mode {
                    let result = rvt_model::walk_record(&schema, class_index, body);
                    let faces = result
                        .references
                        .iter()
                        .filter(|reference| {
                            schema
                                .class_by_index(reference.class_index)
                                .is_some_and(|class| class.name == "Face")
                        })
                        .count();
                    if faces > 0 {
                        boundary_records += 1;
                        boundary_faces += faces;
                        if result.is_exact() {
                            boundary_exact += 1;
                            boundary_exact_faces += faces;
                        }
                    }
                    // `--faces-only` narrows every tally below to the records
                    // geometry is actually read out of, which tile exactly far
                    // less often than the file average.
                    if faces_only && faces == 0 {
                        continue;
                    }
                    if result.pending_references == 0 {
                        drained += 1;
                        drained_exact += usize::from(result.is_exact());
                    }
                    objects += result.nodes;
                    pending += result.pending_references;
                    references += result.references.len();
                    trailer_matches += usize::from(result.length_trailer_matches);
                    if result.is_exact() {
                        exact += 1;
                    } else if let Some(stop) = &result.stop {
                        stopped += 1;
                        let described = describe_serial_stop(stop);
                        if dump_stop.is_some_and(|wanted| described.contains(wanted))
                            && dumped < dump_count
                        {
                            dumped += 1;
                            let from = result.stop_offset.saturating_sub(dump_window);
                            let to = (result.stop_offset + dump_window).min(body.len());
                            let mut before = String::new();
                            for byte in body.get(from..result.stop_offset).unwrap_or_default() {
                                let _ = write!(before, "{byte:02x}");
                            }
                            let mut after = String::new();
                            for byte in body.get(result.stop_offset..to).unwrap_or_default() {
                                let _ = write!(after, "{byte:02x}");
                            }
                            println!(
                                "  id={} len={} stop_at={} {described}",
                                header.id,
                                body.len(),
                                result.stop_offset
                            );
                            println!("    before={before} | after={after}");
                        }
                        *stops.entry(described).or_default() += 1;
                    } else {
                        trailing += 1;
                        if dump_remaining == Some(result.remaining) && dumped < dump_count {
                            dumped += 1;
                            let mut tail = String::new();
                            for byte in body
                                .get(result.consumed..body.len() - RECORD_LENGTH_TRAILER_BYTES)
                                .unwrap_or_default()
                            {
                                let _ = write!(tail, "{byte:02x}");
                            }
                            let (_, walked) =
                                rvt_model::walk_record_collecting(&schema, class_index, body);
                            let mut walked_classes = String::new();
                            for object in walked.iter().rev().take(6).rev() {
                                let class = schema
                                    .class_by_index(object.class_index)
                                    .map_or("<unknown>", |class| class.name.as_str());
                                let _ = write!(walked_classes, " {class}/{}", object.bytes);
                            }
                            println!(
                                "  id={} len={} consumed={} pending={} objects={} tail={tail}",
                                header.id,
                                body.len(),
                                result.consumed,
                                result.pending_references,
                                walked.len()
                            );
                            println!("    last:{walked_classes}");
                        }
                        *trailing_bytes.entry(result.remaining).or_default() += 1;
                    }
                    continue;
                }
                let (remaining, stop, found, read_objects) = if stream {
                    let result = rvt_model::walk_object_stream(&schema, class_index, body);
                    objects += result.objects;
                    pending += result.pending_references;
                    (
                        result.remaining,
                        result.stop,
                        result.references.len(),
                        result.objects,
                    )
                } else {
                    let result = rvt_model::walk_object(&schema, class_index, body);
                    (result.remaining, result.stop, result.references.len(), 0)
                };
                let _ = read_objects;
                references += found;
                match &stop {
                    None if remaining == 0 => exact += 1,
                    None => {
                        trailing += 1;
                        *trailing_bytes.entry(remaining).or_default() += 1;
                    }
                    Some(stop) => {
                        stopped += 1;
                        *stops.entry(describe_serial_stop(stop)).or_default() += 1;
                    }
                }
            }
        },
    )?;

    if faces_only {
        // The tallies below cover only face-bearing records; report their
        // count, not the file's, so the percentages have the right base.
        attempted = boundary_records;
    }

    let share = |part: usize| {
        if attempted == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / attempted as f64
            }
        }
    };
    println!("Class: {class_name} [{class_index}]");
    println!("Records walked: {attempted}");
    println!("  explained exactly: {exact} ({:.1}%)", share(exact));
    println!(
        "  declarations read, bytes left over: {trailing} ({:.1}%)",
        share(trailing)
    );
    println!("  stopped early: {stopped} ({:.1}%)", share(stopped));
    println!("  node references read: {references}");
    if stream || record_mode {
        println!("  referenced objects walked: {objects}");
        println!("  references never reached: {pending}");
    }
    if record_mode {
        println!(
            "  records carrying boundary faces: {boundary_records} ({boundary_exact} explained exactly)"
        );
        println!("  faces in them: {boundary_faces} ({boundary_exact_faces} in explained records)");
        // A record whose reference queue drains before its body does has read
        // every object the body holds; one with references left over stopped
        // because the body ended, and its last references name objects stored
        // elsewhere. The two end differently, so tally them apart.
        println!(
            "  reference queue drained before the body: {drained} ({drained_exact} explained exactly)"
        );
        println!(
            "  trailing length word matches the body length: {trailer_matches} ({:.1}%)",
            share(trailer_matches)
        );
    }
    if !stops.is_empty() {
        println!("Where the walk stopped:");
        let mut ranked = stops.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        for (reason, count) in ranked.iter().take(rows) {
            println!("  {count:8}  {reason}");
        }
    }
    if !trailing_bytes.is_empty() {
        println!("Bytes left over:");
        let mut ranked = trailing_bytes.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        for (remaining, count) in ranked.iter().take(rows) {
            println!("  {count:8} records  {remaining} bytes");
        }
    }
    Ok(())
}

/// Check the width the walk reads `GInfo.m_flags` at against the width the
/// bytes that follow prove, over every node the corpus holds.
///
/// The check assumes no rule: for a node whose first declaration after the
/// inherited `GNode.m_GInfo` is a reference the schema fixes the class of, the
/// width is read off the following bytes, and the walk's own reading is scored
/// against it. This is what refuted the reading that took the lead word's top
/// bit as a width marker - it labelled 73 354 sites four bytes and not one
/// site two - and it is what would catch a regression the record tallies
/// cannot see, since a record can tile with a compensating pair of errors.
///
/// `--node-class` narrows the report to one class and adds the raw words from
/// `m_flags` on. That is how the `EdgeLoop` question was settled: under the old
/// reading its `m_nextLoop` read as the nonsense `id=8 class=0` in all 67 691
/// loops of an exactly-explained record, harmless only because class 0 does not
/// resolve, where the declared width gives a null identifier followed directly
/// by `m_pFace`.
#[allow(clippy::too_many_lines)] // One pass over the corpus plus its tables.
fn flags_probe(
    path: &Path,
    class_name: &str,
    rows: usize,
    faces_only: bool,
    every_record: bool,
    node_class: Option<&str>,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let class_index = schema_class_index(Some(&schema), class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("schema class not found: {class_name}"),
        )
    })?;
    let partition_paths = partition_paths(&container);

    let mut records = 0_usize;
    let mut exact = 0_usize;
    let mut sampled_records = 0_usize;
    let mut samples_seen = 0_usize;
    let mut samples: Vec<rvt_model::FlagWidthSample> = Vec::new();

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (result, found) =
                    rvt_model::walk_record_flag_widths(&schema, class_index, body);
                if faces_only && !carries_faces(&schema, &result) {
                    continue;
                }
                records += 1;
                exact += usize::from(result.is_exact());
                samples_seen += found.len();
                // A site inside a record that did not tile may sit at an
                // offset the walk got wrong, and then its label is a label of
                // the wrong bytes. Exactly-explained records are the ones
                // whose offsets are known good.
                if every_record || result.is_exact() {
                    sampled_records += 1;
                    samples.extend(found);
                }
            }
        },
    )?;

    let share = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / whole as f64
            }
        }
    };
    println!("Class: {class_name} [{class_index}]");
    println!("Records walked: {records}");
    println!(
        "  explained exactly: {exact} ({:.1}%)",
        share(exact, records)
    );
    println!("Node GInfo objects met: {samples_seen}");
    println!(
        "  kept from {sampled_records} records{}: {}",
        if every_record {
            ""
        } else {
            " explained exactly"
        },
        samples.len()
    );

    let mut dataset: Vec<(rvt_model::FlagWidthSample, usize)> = Vec::new();
    let mut checkable = 0_usize;
    let mut ambiguous = 0_usize;
    for sample in &samples {
        if sample.checkable {
            checkable += 1;
        }
        match sample.proved {
            Some(proved) => dataset.push((*sample, proved)),
            None if sample.checkable => ambiguous += 1,
            None => {}
        }
    }
    let proved_short = dataset.iter().filter(|(_, width)| *width == 2).count();
    let proved_long = dataset.iter().filter(|(_, width)| *width == 4).count();
    let disagreed = dataset
        .iter()
        .filter(|(sample, width)| sample.read != *width)
        .count();
    println!("Of them checkable by the reference oracle: {checkable}");
    println!("  the bytes prove four: {proved_long}");
    println!("  the bytes prove two: {proved_short}");
    println!("  unlabelled, neither width or both land on a legal reference: {ambiguous}");
    println!(
        "  where the walk read a different width: {disagreed} ({:.2}%)",
        share(disagreed, dataset.len())
    );

    if let Some(wanted) = node_class {
        dataset.retain(|(sample, _)| {
            schema
                .class_by_index(sample.node_class)
                .is_some_and(|class| class.name == wanted)
        });
        println!(
            "Restricted to {wanted} objects: {} labelled sites",
            dataset.len()
        );
    }

    let split = |key: &dyn Fn(&rvt_model::FlagWidthSample) -> String| {
        let mut table: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for (sample, width) in &dataset {
            let entry = table.entry(key(sample)).or_default();
            if *width == 4 {
                entry.1 += 1;
            } else {
                entry.0 += 1;
            }
        }
        table
    };
    let report = |title: &str, table: &BTreeMap<String, (usize, usize)>| {
        let mixed: usize = table
            .values()
            .map(|(short, long)| short.min(long))
            .sum::<usize>();
        println!(
            "{title}: {} values, {mixed} of them holding both widths",
            table.len()
        );
        let mut ranked = table.iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            (right.1.0 + right.1.1)
                .cmp(&(left.1.0 + left.1.1))
                .then(left.0.cmp(right.0))
        });
        for (value, (short, long)) in ranked.iter().take(rows) {
            println!("  {value:>30}  two={short:<8} four={long}");
        }
    };

    report(
        "By the node's class",
        &split(&|sample| {
            schema.class_by_index(sample.node_class).map_or_else(
                || format!("{}", sample.node_class),
                |class| class.name.clone(),
            )
        }),
    );
    if node_class.is_some() {
        // The raw words, so the two readings can be compared byte for byte:
        // under two bytes the object's next declaration starts at word 1,
        // under the declared four at word 2.
        report(
            "By the words from m_flags on",
            &split(&|sample| {
                let mut shown = String::new();
                for word in sample.words {
                    let _ = write!(shown, "{word:04x} ");
                }
                shown.trim_end().to_owned()
            }),
        );
        let reference_at = |sample: &rvt_model::FlagWidthSample, word: usize| {
            let class = sample.words[word + 2];
            format!(
                "id={} {}",
                u32::from(sample.words[word]) | (u32::from(sample.words[word + 1]) << 16),
                schema
                    .class_by_index(class)
                    .map_or_else(|| format!("class {class}"), |class| class.name.clone())
            )
        };
        report(
            "By the reference a two-byte read lands on",
            &split(&|sample| reference_at(sample, 1)),
        );
        report(
            "By the reference the declared width lands on",
            &split(&|sample| reference_at(sample, 2)),
        );
    }
    Ok(())
}

/// Whether a walked record named any boundary face.
fn carries_faces(schema: &Schema, walk: &rvt_model::SerialRecordWalk) -> bool {
    walk.references.iter().any(|reference| {
        schema
            .class_by_index(reference.class_index)
            .is_some_and(|class| class.name == "Face")
    })
}

/// What bodies one record declares, and how each stands against the box the
/// same record carries.
///
/// This is the per-record form of the question [`place_declared_body`] answers
/// in bulk: a record is a set of `GBRep` nodes, and the reason a body appears
/// to disagree with its box is nearly always that two of them were read as
/// one.
fn report_declared_bodies(
    schema: &Schema,
    record_body: &[u8],
    objects: &[rvt_model::SerialObject],
) {
    let Some(classes) = brep_class_indexes(Some(schema)) else {
        return;
    };
    let assembled =
        rvt_model::assemble_symbol_brep(objects, &classes, &brep_body_classes(Some(schema)));
    if assembled.bodies.is_empty() {
        return;
    }
    let bounds = schema_class_index(Some(schema), "GNode").and_then(|gnode| {
        GElementGraphFields::parse(record_body, |class_index| {
            schema_class_is_a(schema, class_index, gnode)
        })
    });
    println!(
        "    bodies declared by GBRep nodes: {} (box {})",
        assembled.bodies.len(),
        bounds.as_ref().map_or_else(
            || "not read".to_owned(),
            |graph| format!("{:?} {:?}", graph.bounds.min, graph.bounds.max)
        )
    );
    for (index, declared) in assembled.bodies.iter().enumerate() {
        let sub = assembled.body(index);
        let extent = sub.as_ref().and_then(body_extent_feet);
        let residual = sub
            .as_ref()
            .zip(bounds.as_ref())
            .and_then(|(sub, graph)| body_bounds_residual_feet(sub, &graph.bounds));
        println!(
            "      node {} faces={} edges={} one-sided={} open={} solid={} closed={} extent={extent:?} residual={residual:?}",
            declared.node_id,
            declared.faces.len(),
            declared.edges,
            declared.one_sided_edges,
            declared.open_edges,
            declared.is_solid(),
            declared.is_closed(),
        );
        let Some(sub) = sub else { continue };
        for (face, face_id) in sub.faces.iter().zip(&sub.face_ids) {
            let Some(marks) = objects
                .iter()
                .find(|object| object.object_id == *face_id)
                .and_then(GFaceMarks::read)
            else {
                continue;
            };
            let extent = faces_extent_feet(std::slice::from_ref(face));
            let outside = extent.zip(bounds.as_ref()).map(|(extent, graph)| {
                face_reaches_outside(extent, &graph.bounds, BODY_BOUNDS_TOLERANCE_FEET)
            });
            println!("        face {face_id} {marks} outside={outside:?} extent={extent:?}");
        }
    }
}

/// One candidate mark a face might carry, and what it is read from.
///
/// Each is a field the schema declares on `GFace` or on the `GInfo` every
/// `GNode` opens with - nothing here is searched for or inferred from the
/// geometry. What the probe asks of each is the same question: does it fire on
/// exactly the faces that reach outside the box the record itself carries?
struct FaceMark {
    name: &'static str,
    fires: fn(&GFaceMarks) -> bool,
}

const FACE_MARKS: &[FaceMark] = &[
    FaceMark {
        name: "m_cutType != 0",
        fires: |marks| marks.cut_type != 0,
    },
    FaceMark {
        name: "m_renderStyleId is null",
        fires: |marks| marks.render_style_id < 0,
    },
    FaceMark {
        name: "GInfo.m_flags & 0x80000 clear",
        fires: |marks| marks.info_flags & FACE_INSIDE_THE_BOX_FLAG == 0,
    },
    FaceMark {
        name: "m_faceFlags_v9 & 0x2 set",
        fires: |marks| marks.face_flags & 0x2 != 0,
    },
];

/// What one mark scored, over every face the file declares and every record
/// whose bodies miss its box.
#[derive(Clone, Copy, Debug, Default)]
struct FaceMarkScore {
    /// Faces the mark fires on that reach outside their record's box.
    marked_outside: u64,
    /// Faces it fires on that lie inside it.
    marked_inside: u64,
    /// Faces outside the box that it does not fire on.
    unmarked_outside: u64,
    /// Records whose bodies miss the box and where dropping the faces this
    /// mark fires on leaves exactly one body reproducing it. This is what the
    /// mark is for, and the number to read.
    records_recovered: u64,
    /// ...of which the recovered body's boundary closes, and of which it is a
    /// solid. A body that only closes once its own geometry is thrown away is
    /// not a wall.
    records_recovered_closed: u64,
    records_recovered_solid: u64,
    /// ...and of which the kept faces bound a volume by their own loops. See
    /// [`rvt_model::SymbolBrep::bounds_a_volume`]: this is the closure an exporter needs, and
    /// the topological one cannot see it because the trimmed faces are still
    /// named by the edges that reach them.
    records_recovered_bounded: u64,
    /// Records where the trim leaves more than one body on the box, so the
    /// answer is ambiguous and nothing can be selected.
    records_ambiguous: u64,
    /// The control, measured on records the box already resolves: how many
    /// would stop being resolved if the same trim were applied there too.
    /// A trim is only ever a second pass, so this costs nothing - it says how
    /// much real geometry the mark would take if it were a first one.
    placed_records_broken: u64,
}

/// Score every declared face mark against the record's own box.
///
/// A `GElement` record's box bounds what the element draws. A body that
/// reaches outside it holds faces that are not the element's surface - on a
/// wall, the geometry of the openings cut into it, which the file joins into
/// the same shell. This asks each field the format declares on a face whether
/// it separates those two sets, and reports the one measurement that can
/// accept such a rule: how many records whose bodies miss the box land exactly
/// one body on it once the marked faces are dropped.
///
/// The trim is measured the way it would be applied - the marked `Face`
/// objects are removed and the record is assembled again - so the body counts
/// its own edges afterwards and can say whether what is left still closes.
#[allow(clippy::too_many_lines)] // One streaming pass plus its report.
fn face_mark_probe(path: &Path, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?;
    let classes = brep_class_indexes(schema.as_ref());
    let body_classes = brep_body_classes(schema.as_ref());
    let geometry_element_class_index = schema_class_index(schema.as_ref(), "GElement");
    let gnode_class_index = schema_class_index(schema.as_ref(), "GNode");
    let (Some(schema), Some(classes), Some(geometry_element_class_index), Some(gnode_class_index)) = (
        schema.as_ref(),
        classes,
        geometry_element_class_index,
        gnode_class_index,
    ) else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare the boundary-representation classes",
        )));
    };
    let partition_paths = partition_paths(&container);

    let mut records = 0_u64;
    let mut records_placed = 0_u64;
    let mut records_ambiguous = 0_u64;
    let mut bodies = 0_u64;
    let mut faces_read = 0_u64;
    let mut faces_declared = 0_u64;
    let mut faces_outside = 0_u64;
    let mut scores = vec![FaceMarkScore::default(); FACE_MARKS.len()];
    // Which bodies of an assembly reproduce the box, and whether that is
    // exactly one - the same test `place_declared_body` applies.
    let placed_bodies = |assembled: &rvt_model::SymbolBrep, bounds: &GElementBounds| {
        (0..assembled.bodies.len())
            .filter(|index| {
                assembled
                    .body(*index)
                    .is_some_and(|body| body_is_placed_in(&body, bounds))
            })
            .collect::<Vec<_>>()
    };
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let Some(graph) = GElementGraphFields::parse(body, |class_index| {
                    schema_class_is_a(schema, class_index, gnode_class_index)
                }) else {
                    continue;
                };
                if !graph.bounds.is_volumetric() {
                    continue;
                }
                let (_walk, objects) =
                    rvt_model::walk_record_collecting(schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == classes.face)
                {
                    continue;
                }
                let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes);
                if assembled.bodies.is_empty() {
                    continue;
                }
                records += 1;
                bodies += assembled.bodies.len() as u64;
                let placed = placed_bodies(&assembled, &graph.bounds);
                records_placed += u64::from(placed.len() == 1);
                records_ambiguous += u64::from(placed.len() > 1);

                // The face-level cross-tab, over the faces the record's bodies
                // hold: what each mark fires on, against whether the face
                // reaches outside the box.
                let marks: std::collections::HashMap<u32, GFaceMarks> = objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                    .filter_map(|object| {
                        GFaceMarks::read(object).map(|marks| (object.object_id, marks))
                    })
                    .collect();
                faces_declared += objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                    .count() as u64;
                faces_read += assembled.faces.len() as u64;
                for (face, face_id) in assembled.faces.iter().zip(&assembled.face_ids) {
                    let outside =
                        faces_extent_feet(std::slice::from_ref(face)).is_some_and(|extent| {
                            face_reaches_outside(extent, &graph.bounds, BODY_BOUNDS_TOLERANCE_FEET)
                        });
                    faces_outside += u64::from(outside);
                    let Some(face_marks) = marks.get(face_id) else {
                        continue;
                    };
                    for (mark, score) in FACE_MARKS.iter().zip(&mut scores) {
                        match ((mark.fires)(face_marks), outside) {
                            (true, true) => score.marked_outside += 1,
                            (true, false) => score.marked_inside += 1,
                            (false, true) => score.unmarked_outside += 1,
                            (false, false) => {}
                        }
                    }
                }

                // The trim, assembled the way it would be applied.
                for (mark, score) in FACE_MARKS.iter().zip(&mut scores) {
                    let trimmed_objects = objects
                        .iter()
                        .filter(|object| {
                            object.class_index != classes.face
                                || !marks.get(&object.object_id).is_some_and(mark.fires)
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if trimmed_objects.len() == objects.len() {
                        continue;
                    }
                    let trimmed =
                        rvt_model::assemble_symbol_brep(&trimmed_objects, &classes, &body_classes);
                    let trimmed_placed = placed_bodies(&trimmed, &graph.bounds);
                    if placed.len() == 1 {
                        score.placed_records_broken += u64::from(trimmed_placed.len() != 1);
                        continue;
                    }
                    match trimmed_placed[..] {
                        [index] => {
                            score.records_recovered += 1;
                            if let Some(body) = trimmed.bodies.get(index) {
                                score.records_recovered_closed += u64::from(body.is_closed());
                                score.records_recovered_solid += u64::from(body.is_solid());
                            }
                            if let Some(body) = trimmed.body(index) {
                                score.records_recovered_bounded +=
                                    u64::from(body.bounds_a_volume());
                            }
                        }
                        [_, _, ..] => score.records_ambiguous += 1,
                        [] => {}
                    }
                }
            }
        },
    )?;

    let share = |part: u64, whole: u64| {
        if whole == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / whole as f64
            }
        }
    };
    println!("File: {}", path.display());
    println!("GElement records with a volumetric box and a declared body: {records}");
    println!("  bodies declared: {bodies}");
    println!(
        "  records where exactly one body reproduces the box: {records_placed} ({:.1}%)",
        share(records_placed, records)
    );
    println!("  records where more than one does: {records_ambiguous}");
    println!("  faces declared: {faces_declared}, of them resolved and in a body: {faces_read}");
    println!(
        "  faces reaching outside their record's box: {faces_outside} ({:.1}%)",
        share(faces_outside, faces_read)
    );
    println!("Each mark against that answer:");
    for (mark, score) in FACE_MARKS.iter().zip(&scores) {
        println!("  {}", mark.name);
        println!(
            "    fires on {} faces outside the box and {} inside it; {} outside faces it misses",
            score.marked_outside, score.marked_inside, score.unmarked_outside
        );
        println!(
            "    records recovered: {} (bounding a volume {}, two-sided {}, closed {}), left ambiguous: {}",
            score.records_recovered,
            score.records_recovered_bounded,
            score.records_recovered_solid,
            score.records_recovered_closed,
            score.records_ambiguous
        );
        println!(
            "    records the box already resolves that the same trim would break: {}",
            score.placed_records_broken
        );
    }
    Ok(())
}

/// Whether a face's own extent reaches outside the record's box by more than
/// `tolerance`. A face that lies inside it is part of what the box bounds; one
/// that reaches past it cannot be.
fn face_reaches_outside(
    extent: ([f64; 3], [f64; 3]),
    bounds: &GElementBounds,
    tolerance: f64,
) -> bool {
    let (min, max) = extent;
    (0..3).any(|axis| {
        min[axis] < bounds.min[axis] - tolerance || max[axis] > bounds.max[axis] + tolerance
    })
}

/// Summarize one record's node stream and check that its boundary topology
/// closes: every edge naming two faces that exist, every loop naming a face,
/// every face naming a loop. Nothing is inferred - only what was read.
#[allow(clippy::too_many_lines)] // One pass over the objects plus its report.
fn report_boundary_topology(
    schema: &Schema,
    id: u32,
    body_bytes: usize,
    walk: &rvt_model::SerialRecordWalk,
    objects: &[rvt_model::SerialObject],
) {
    let name_of = |class_index: u16| {
        schema
            .class_by_index(class_index)
            .map_or("<unknown>", |class| class.name.as_str())
    };
    let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
    for object in objects {
        *classes.entry(name_of(object.class_index)).or_default() += 1;
    }
    let mut ranked = classes.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    let mut summary = String::new();
    for (name, count) in ranked.iter().take(7) {
        if !summary.is_empty() {
            summary.push(' ');
        }
        let _ = write!(summary, "{name}x{count}");
    }
    println!(
        "  record {id}: {body_bytes} bytes, exact={}, objects={}",
        walk.is_exact(),
        objects.len()
    );
    println!("    classes: {summary}");

    for object in objects.iter().filter(|object| {
        matches!(
            name_of(object.class_index),
            "RuledSurf"
                | "GLine"
                | "GArc"
                | "GEllipse"
                | "Face"
                | "Edge"
                | "InstanceInfo"
                | "GInstance"
                | "Geometry"
        )
    }) {
        println!(
            "    object {} {} refs={:?} ids={:?} ints={:?} alts={:?} numbers={:?}",
            object.object_id,
            name_of(object.class_index),
            object.references,
            object.identifiers,
            object.integers,
            object.alternate_integers,
            object.numbers
        );
    }

    let ids_of = |wanted: &str| {
        objects
            .iter()
            .filter(|object| name_of(object.class_index) == wanted)
            .map(|object| object.object_id)
            .collect::<BTreeSet<_>>()
    };
    let faces = ids_of("Face");
    let loops = ids_of("EdgeLoop");
    if faces.is_empty() {
        return;
    }

    let mut edges = 0_usize;
    let mut edges_with_two_faces = 0_usize;
    let mut edges_resolved = 0_usize;
    let mut loops_resolved = 0_usize;
    let mut faces_with_loop = 0_usize;
    let mut faces_with_surface = 0_usize;
    let mut surfaces: BTreeMap<&str, usize> = BTreeMap::new();
    for object in objects {
        match name_of(object.class_index) {
            "Edge" => {
                edges += 1;
                let named = object.identifiers.iter().take(2).collect::<BTreeSet<_>>();
                if named.len() == 2 {
                    edges_with_two_faces += 1;
                }
                if named.iter().all(|id| faces.contains(id)) {
                    edges_resolved += 1;
                }
            }
            "EdgeLoop" => {
                if object
                    .identifiers
                    .first()
                    .is_some_and(|id| faces.contains(id))
                {
                    loops_resolved += 1;
                }
            }
            "Face" => {
                if object
                    .references
                    .first()
                    .is_some_and(|reference| loops.contains(&reference.object_id))
                {
                    faces_with_loop += 1;
                }
                // `Face.m_pSurf` is the last reference the class declares.
                if let Some(surface) = object
                    .references
                    .last()
                    .filter(|reference| reference.object_id != 0)
                {
                    faces_with_surface += 1;
                    *surfaces.entry(name_of(surface.class_index)).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    println!(
        "    faces {} ({faces_with_loop} name a loop that exists, {faces_with_surface} name a surface)",
        faces.len()
    );
    println!(
        "    loops {} ({loops_resolved} name a face that exists)",
        loops.len()
    );
    println!(
        "    edges {edges} ({edges_with_two_faces} name two distinct faces, {edges_resolved} of those exist)"
    );
    // Surfaces carry their frame as plain numbers: an `Envelope` of four,
    // then the axes. Checking the frame is orthonormal and the radius positive
    // tests the decode itself, not just the topology.
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value).unwrap_or(f64::NAN);
    let unit = |v: &[f64]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let dot = |a: &[f64], b: &[f64]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let mut planes_checked = 0_usize;
    let mut planes_orthonormal = 0_usize;
    let mut cylinders_checked = 0_usize;
    let mut cylinders_orthonormal = 0_usize;
    let mut radii: Vec<f64> = Vec::new();
    for object in objects {
        let numbers = &object.numbers;
        match name_of(object.class_index) {
            // Envelope(4), origin(3), xVec(3), yVec(3)
            "Plane" if numbers.len() >= 13 => {
                planes_checked += 1;
                let (x, y) = (&numbers[7..10], &numbers[10..13]);
                if (unit(x) - 1.0).abs() < 1.0e-9
                    && (unit(y) - 1.0).abs() < 1.0e-9
                    && dot(x, y).abs() < 1.0e-9
                {
                    planes_orthonormal += 1;
                }
            }
            // Envelope(4), center(3), xVec(3), yVec(3), zVec(3), radius(1)
            "CylSurf" if numbers.len() >= 17 => {
                cylinders_checked += 1;
                let (x, y, z) = (&numbers[7..10], &numbers[10..13], &numbers[13..16]);
                if (unit(x) - 1.0).abs() < 1.0e-9
                    && (unit(y) - 1.0).abs() < 1.0e-9
                    && (unit(z) - 1.0).abs() < 1.0e-9
                    && dot(x, y).abs() < 1.0e-9
                    && dot(x, z).abs() < 1.0e-9
                {
                    cylinders_orthonormal += 1;
                }
                radii.push(metres(numbers[16]) * 1000.0);
            }
            _ => {}
        }
    }
    if planes_checked > 0 || cylinders_checked > 0 {
        println!("    planes with an orthonormal frame: {planes_orthonormal} of {planes_checked}");
        println!(
            "    cylinders with an orthonormal frame: {cylinders_orthonormal} of {cylinders_checked}"
        );
        radii.sort_by(f64::total_cmp);
        radii.dedup_by(|left, right| (*left - *right).abs() < 1.0e-6);
        let mut listed = String::new();
        for radius in radii.iter().take(12) {
            if !listed.is_empty() {
                listed.push(' ');
            }
            let _ = write!(listed, "{radius:.1}");
        }
        println!("    distinct cylinder radii, mm: {listed}");
    }
    let mut surface_summary = String::new();
    for (name, count) in surfaces {
        if !surface_summary.is_empty() {
            surface_summary.push(' ');
        }
        let _ = write!(surface_summary, "{name}x{count}");
    }
    if !surface_summary.is_empty() {
        println!("    surfaces: {surface_summary}");
    }
}

fn describe_serial_stop(stop: &rvt_model::SerialStop) -> String {
    match stop {
        rvt_model::SerialStop::Truncated { class, property } => {
            format!("truncated at {class}.{property}")
        }
        rvt_model::SerialStop::Unsupported {
            class,
            property,
            reason,
        } => format!("unsupported {reason} at {class}.{property}"),
        rvt_model::SerialStop::UnknownClass { class_index } => {
            format!("unknown class [{class_index}]")
        }
        rvt_model::SerialStop::TooDeep => "class chain too deep".to_owned(),
    }
}

/// Census every `RuledSurf` the corpus writes.
///
/// After the parent `Surface`'s four-number envelope the class declares two
/// object references - `m_pProfileCurve1` and `m_pProfileCurve2` - and two
/// three-number points, `m_Point1` and `m_Point2`. Which of the four a given
/// surface actually uses is not stated anywhere, so this counts it rather
/// than assuming it: the class pair the references name, whether the named
/// object is written in the same record, what the points hold on each side,
/// and whether a face reaches its `RuledSurf` by identifier or by encounter
/// order the way `CylSurf` does.
#[allow(clippy::too_many_lines)] // One streaming pass and the tallies it fills.
fn ruled_surf_probe(path: &Path, rows: usize, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let ruled_class_index = schema_class_index(Some(&schema), "RuledSurf")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no RuledSurf"))?;
    let face_class_index = schema_class_index(Some(&schema), "Face")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no Face"))?;
    let edge_class_index = schema_class_index(Some(&schema), "Edge")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no Edge"))?;
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no GElement"))?;
    let partition_paths = partition_paths(&container);

    let mut records_with_a_ruled_surface = 0_u64;
    let mut surfaces = 0_u64;
    let mut faces_naming_one = 0_u64;
    let mut numbers_widths: BTreeMap<usize, u64> = BTreeMap::new();
    let mut reference_counts: BTreeMap<usize, u64> = BTreeMap::new();
    let mut profile_pairs: BTreeMap<(String, String), u64> = BTreeMap::new();
    // Per side: is the reference live, does the named object exist in this
    // record, and is the matching point non-zero? A degenerate profile is
    // expected to show as a null reference beside a used point, but that is
    // the hypothesis, not the reading.
    let mut side_shape: BTreeMap<(usize, bool, bool, bool), u64> = BTreeMap::new();
    // Distinct identifiers among a record's `RuledSurf` objects, against how
    // many it holds. A sentinel-identified class collapses to one.
    let mut identifier_spread: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    let mut sentinel_identifiers: BTreeMap<u32, u64> = BTreeMap::new();
    // Does the count of faces naming a `RuledSurf` match the count of objects?
    // Encounter-order pairing is only sound where it does.
    let mut face_to_object_balance: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    let mut profile_end_params: BTreeMap<String, (u64, f64, f64)> = BTreeMap::new();
    // What the file's own `EdgePnt`s say about the surface's two axes. The
    // ruling axis of a ruled surface runs between the two profiles, so it
    // should span exactly [0, 1]; the other should span the profile's own
    // `m_endParams`. Measuring both ranges decides which axis is which
    // without assuming either.
    let mut u_low = f64::MAX;
    let mut u_high = f64::MIN;
    let mut v_low = f64::MAX;
    let mut v_high = f64::MIN;
    let mut edge_points_on_a_ruled_face = 0_u64;
    // Record identifiers to hand to `serial-probe --element` for a trace.
    let mut example_records: Vec<u32> = Vec::new();
    // `Surface.m_Envelope` is the surface's own parameter box. If a ruled
    // surface always states [0, 0, 1, 1] then both of its axes are normalised
    // and neither carries a profile's raw parameter.
    let mut envelopes: BTreeMap<String, u64> = BTreeMap::new();
    let mut v_outside_unit = 0_u64;
    // How often each axis holds still across an edge: a ruling is constant in
    // the along-profile axis, a profile-following edge in the ruling axis.
    let mut u_constant = 0_u64;
    let mut v_constant = 0_u64;
    let mut neither_constant = 0_u64;
    // The ruling axis should take its extreme values on the profiles
    // themselves, so tally how often an edge sits exactly at each end.
    let mut ruling_axis_endpoints: BTreeMap<String, u64> = BTreeMap::new();

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (_, objects) =
                    rvt_model::walk_record_collecting(&schema, header.class_index, body);
                let ruled: Vec<&rvt_model::SerialObject> = objects
                    .iter()
                    .filter(|object| object.class_index == ruled_class_index)
                    .collect();
                if ruled.is_empty() {
                    continue;
                }
                records_with_a_ruled_surface += 1;
                if example_records.len() < 8 {
                    example_records.push(header.id);
                }
                let present: BTreeSet<(u16, u32)> = objects
                    .iter()
                    .map(|object| (object.class_index, object.object_id))
                    .collect();
                let naming_faces = objects
                    .iter()
                    .filter(|object| object.class_index == face_class_index)
                    .filter(|object| {
                        object
                            .references
                            .last()
                            .is_some_and(|reference| reference.class_index == ruled_class_index)
                    })
                    .count();
                faces_naming_one += naming_faces as u64;
                *face_to_object_balance
                    .entry((naming_faces, ruled.len()))
                    .or_default() += 1;
                let distinct: BTreeSet<u32> = ruled.iter().map(|object| object.object_id).collect();
                *identifier_spread
                    .entry((distinct.len(), ruled.len()))
                    .or_default() += 1;
                if distinct.len() == 1 {
                    if let Some(identifier) = distinct.iter().next() {
                        *sentinel_identifiers.entry(*identifier).or_default() += 1;
                    }
                }
                // Pair faces to objects by encounter order, the way every
                // sentinel-identified surface class is paired, then read the
                // `EdgePnt`s the edges of those faces store on that side.
                let ruled_faces: BTreeSet<u32> = objects
                    .iter()
                    .filter(|object| object.class_index == face_class_index)
                    .filter(|object| {
                        object
                            .references
                            .last()
                            .is_some_and(|reference| reference.class_index == ruled_class_index)
                    })
                    .map(|object| object.object_id)
                    .collect();
                for edge in objects
                    .iter()
                    .filter(|object| object.class_index == edge_class_index)
                {
                    if edge.identifiers.len() != 6 || edge.numbers.len() < 8 {
                        continue;
                    }
                    let tail = &edge.numbers[edge.numbers.len() - 8..];
                    for side in 0..2 {
                        if !ruled_faces.contains(&edge.identifiers[side]) {
                            continue;
                        }
                        let (first_u, first_v) = (tail[side * 2], tail[side * 2 + 1]);
                        let (last_u, last_v) = (tail[4 + side * 2], tail[4 + side * 2 + 1]);
                        for value in [first_u, last_u] {
                            u_low = u_low.min(value);
                            u_high = u_high.max(value);
                        }
                        for value in [first_v, last_v] {
                            v_low = v_low.min(value);
                            v_high = v_high.max(value);
                        }
                        edge_points_on_a_ruled_face += 1;
                        for value in [first_v, last_v] {
                            if value < -1.0e-9 || value > 1.0 + 1.0e-9 {
                                v_outside_unit += 1;
                            }
                        }
                        let still_u = (first_u - last_u).abs() <= 1.0e-9;
                        let still_v = (first_v - last_v).abs() <= 1.0e-9;
                        match (still_u, still_v) {
                            (true, false) => u_constant += 1,
                            (false, true) => v_constant += 1,
                            _ => neither_constant += 1,
                        }
                        if still_u {
                            let at = if first_u.abs() <= 1.0e-9 {
                                "u held at 0"
                            } else if (first_u - 1.0).abs() <= 1.0e-9 {
                                "u held at 1"
                            } else {
                                "u held elsewhere"
                            };
                            *ruling_axis_endpoints.entry(at.to_owned()).or_default() += 1;
                        }
                        if still_v {
                            let at = if first_v.abs() <= 1.0e-9 {
                                "v held at 0"
                            } else if (first_v - 1.0).abs() <= 1.0e-9 {
                                "v held at 1"
                            } else {
                                "v held elsewhere"
                            };
                            *ruling_axis_endpoints.entry(at.to_owned()).or_default() += 1;
                        }
                    }
                }
                for object in &ruled {
                    surfaces += 1;
                    *numbers_widths.entry(object.numbers.len()).or_default() += 1;
                    if let Some(envelope) = object.numbers.get(0..4) {
                        *envelopes
                            .entry(format!(
                                "[{:.3}, {:.3}, {:.3}, {:.3}]",
                                envelope[0], envelope[1], envelope[2], envelope[3]
                            ))
                            .or_default() += 1;
                    }
                    *reference_counts.entry(object.references.len()).or_default() += 1;
                    let named = |side: usize| -> Option<&rvt_model::GElementNodeReference> {
                        object
                            .references
                            .get(side)
                            .filter(|reference| reference.object_id != 0)
                    };
                    let class_name = |side: usize| -> String {
                        named(side).map_or_else(
                            || "null".to_owned(),
                            |reference| {
                                schema.class_by_index(reference.class_index).map_or_else(
                                    || format!("class {}", reference.class_index),
                                    |class| class.name.clone(),
                                )
                            },
                        )
                    };
                    *profile_pairs
                        .entry((class_name(0), class_name(1)))
                        .or_default() += 1;
                    for side in 0..2 {
                        // `m_Point1` is numbers 4..7 and `m_Point2` 7..10, the
                        // envelope taking the first four.
                        let start = 4 + side * 3;
                        let point = object.numbers.get(start..start + 3);
                        let point_used =
                            point.is_some_and(|point| point.iter().any(|value| value.abs() > 0.0));
                        let reference = named(side);
                        let resolvable = reference.is_some_and(|reference| {
                            present.contains(&(reference.class_index, reference.object_id))
                        });
                        *side_shape
                            .entry((side, reference.is_some(), resolvable, point_used))
                            .or_default() += 1;
                        // A live profile's own parameter range says what `v`
                        // the ruling runs over; `GCurve.m_endParams` is the
                        // first two numbers of every curve.
                        if let Some(reference) = reference {
                            if let Some(curve) = objects.iter().find(|candidate| {
                                candidate.class_index == reference.class_index
                                    && candidate.object_id == reference.object_id
                            }) {
                                if let Some(params) = curve.numbers.get(0..2) {
                                    let entry = profile_end_params
                                        .entry(class_name(side))
                                        .or_insert((0, f64::MAX, f64::MIN));
                                    entry.0 += 1;
                                    entry.1 = entry.1.min(params[0]);
                                    entry.2 = entry.2.max(params[1]);
                                }
                            }
                        }
                    }
                }
            }
        },
    )?;

    println!("RuledSurf census for {}", path.display());
    println!("Records holding at least one: {records_with_a_ruled_surface}");
    println!("RuledSurf objects: {surfaces}");
    println!("Faces naming one as their surface: {faces_naming_one}");

    println!("Float64 values per object:");
    for (width, count) in numbers_widths.iter().take(rows) {
        println!("  {width}\t{count}");
    }
    println!("References per object:");
    for (count, records) in reference_counts.iter().take(rows) {
        println!("  {count}\t{records}");
    }

    println!("Profile pair (m_pProfileCurve1, m_pProfileCurve2):");
    let mut ranked: Vec<_> = profile_pairs.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((first, second), count) in ranked.iter().take(rows) {
        println!("  {count}\t{first} -> {second}");
    }

    println!("Per side (side, reference live, names an object in this record, point non-zero):");
    let mut sides: Vec<_> = side_shape.into_iter().collect();
    sides.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((side, live, resolvable, point_used), count) in sides.iter().take(rows) {
        println!(
            "  {count}\tm_Point{} live={live} resolvable={resolvable} point={point_used}",
            side + 1
        );
    }

    println!("Profile parameter range by class (from GCurve.m_endParams):");
    for (name, (count, low, high)) in profile_end_params.iter().take(rows) {
        println!("  {name}\tn={count} min={low:.6} max={high:.6}");
    }

    println!("Surface.m_Envelope values:");
    let mut boxes: Vec<_> = envelopes.into_iter().collect();
    boxes.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (envelope, count) in boxes.iter().take(rows) {
        println!("  {count}\t{envelope}");
    }
    println!("Edge v values outside [0, 1]: {v_outside_unit}");
    println!("Example records (serial-probe --element): {example_records:?}");
    println!("EdgePnt values on ruled faces: {edge_points_on_a_ruled_face} edge sides");
    if edge_points_on_a_ruled_face > 0 {
        println!("  u spans {u_low:.6} .. {u_high:.6}");
        println!("  v spans {v_low:.6} .. {v_high:.6}");
    }
    println!("  u held constant across the edge: {u_constant}");
    println!("  v held constant across the edge: {v_constant}");
    println!("  neither held constant: {neither_constant}");
    for (label, count) in &ruling_axis_endpoints {
        println!("  {count}\t{label}");
    }
    println!("Distinct identifiers vs objects per record:");
    let mut spread: Vec<_> = identifier_spread.into_iter().collect();
    spread.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((distinct, held), count) in spread.iter().take(rows) {
        println!("  {count}\t{distinct} distinct of {held}");
    }
    println!("Shared identifier when a record collapses to one:");
    let mut sentinels: Vec<_> = sentinel_identifiers.into_iter().collect();
    sentinels.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (identifier, count) in sentinels.iter().take(rows) {
        println!("  {count}\t0x{identifier:08x}");
    }
    println!("Faces naming one vs objects held, per record:");
    let mut balance: Vec<_> = face_to_object_balance.into_iter().collect();
    balance.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((naming, held), count) in balance.iter().take(rows) {
        println!("  {count}\t{naming} faces, {held} objects");
    }
    Ok(())
}

/// Walk every `GElement` record, decode its inherited `GGroup.m_subNodes`
/// reference array, and report which node classes appear and whether their
/// object identifiers resolve against the record identifiers in the file.
/// Nothing here is exported; the probe only measures what is reachable.
#[allow(clippy::too_many_lines)] // One streaming pass keeps large RVT payloads out of memory.
fn geometry_graph_probe(
    path: &Path,
    rows: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no GElement"))?;
    let gnode_class_index = schema_class_index(Some(&schema), "GNode");
    let partition_paths = partition_paths(&container);

    // Every distinct identifier/class pair and the member each identifier was
    // written in, so a node reference can be tested against both.
    let mut record_classes: BTreeSet<(u32, u16)> = BTreeSet::new();
    let mut member_records: BTreeSet<(u64, u32)> = BTreeSet::new();
    let mut graphs: Vec<(u64, Vec<rvt_model::GElementNodeReference>)> = Vec::new();
    let mut geometry_element_records = 0_usize;
    let mut strict_graphs = 0_usize;
    let mut member_key = 0_u64;
    // Bodies are large. Ranking every class by the bytes it occupies says
    // where the payload actually is, independent of any structural guess.
    let mut class_bytes: BTreeMap<u16, (usize, u64)> = BTreeMap::new();
    let mut node_identifier_maximum = 0_u32;
    // Node references are (u32 object id, u16 class index). Scanning whole
    // GElement bodies for that shape reaches nodes nested below the top level.
    // A random six bytes match only if the class index is one of the few dozen
    // GNode subclasses and the identifier stays tiny, so the scan is quiet.
    let mut nested_classes: BTreeMap<u16, usize> = BTreeMap::new();
    let mut gnode_subclasses: BTreeSet<u16> = BTreeSet::new();
    if let Some(gnode_class_index) = gnode_class_index {
        for class in &schema.classes {
            if schema_class_is_a(&schema, class.index, gnode_class_index) {
                gnode_subclasses.insert(class.index);
            }
        }
    }

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            member_key += 1;
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                record_classes.insert((header.id, header.class_index));
                member_records.insert((member_key, header.id));
                let entry = class_bytes.entry(header.class_index).or_default();
                entry.0 += 1;
                entry.1 += u64::from(header.body_bytes);
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                geometry_element_records += 1;
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                if let Some(gnode_class_index) = gnode_class_index {
                    strict_graphs += usize::from(
                        GElementGraphFields::parse(body, |class_index| {
                            schema_class_is_a(&schema, class_index, gnode_class_index)
                        })
                        .is_some(),
                    );
                }
                for window in body.windows(6) {
                    let object_id =
                        u32::from_le_bytes([window[0], window[1], window[2], window[3]]);
                    let class_index = u16::from_le_bytes([window[4], window[5]]);
                    if object_id > 0
                        && object_id <= NESTED_NODE_IDENTIFIER_LIMIT
                        && gnode_subclasses.contains(&class_index)
                    {
                        *nested_classes.entry(class_index).or_default() += 1;
                    }
                }
                // The permissive rule accepts any class the schema knows, so
                // the histogram is not narrowed by the GNode assumption.
                if let Some(graph) = GElementGraphFields::parse(body, |class_index| {
                    schema.class_by_index(class_index).is_some()
                }) {
                    graphs.push((member_key, graph.top_level_nodes));
                }
            }
        },
    )?;

    let mut node_classes: BTreeMap<u16, usize> = BTreeMap::new();
    let mut node_counts: BTreeMap<usize, usize> = BTreeMap::new();
    let mut references = 0_usize;
    let mut class_agreed = 0_usize;
    let mut same_member = 0_usize;
    for (member_key, nodes) in &graphs {
        *node_counts.entry(nodes.len()).or_default() += 1;
        for node in nodes {
            references += 1;
            node_identifier_maximum = node_identifier_maximum.max(node.object_id);
            *node_classes.entry(node.class_index).or_default() += 1;
            class_agreed +=
                usize::from(record_classes.contains(&(node.object_id, node.class_index)));
            same_member += usize::from(member_records.contains(&(*member_key, node.object_id)));
        }
    }

    let share = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / whole as f64
            }
        }
    };
    println!("GElement records: {geometry_element_records}");
    println!(
        "  with a decodable node array and bounds: {} ({:.1}%)",
        graphs.len(),
        share(graphs.len(), geometry_element_records)
    );
    println!("  accepted by the strict GNode rule: {strict_graphs}");
    println!("Top-level node references: {references}");
    println!("Nodes per GElement:");
    for (count, occurrences) in node_counts.iter().take(rows) {
        println!("  {count:6} nodes  {occurrences} GElements");
    }
    println!("Node classes:");
    let mut ranked = node_classes.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (class_index, count) in ranked.iter().take(rows) {
        let name = schema
            .class_by_index(*class_index)
            .map_or("<unknown>", |class| class.name.as_str());
        println!(
            "  {count:8} ({:5.1}%)  {name} [{class_index}]",
            share(*count, references)
        );
    }
    println!("Record classes by total body bytes:");
    let mut by_bytes = class_bytes.into_iter().collect::<Vec<_>>();
    by_bytes.sort_by(|left, right| right.1.1.cmp(&left.1.1).then(left.0.cmp(&right.0)));
    let total_bytes = by_bytes.iter().map(|(_, (_, bytes))| *bytes).sum::<u64>();
    for (class_index, (count, bytes)) in by_bytes.iter().take(rows) {
        let name = schema
            .class_by_index(*class_index)
            .map_or("<unknown>", |class| class.name.as_str());
        #[allow(clippy::cast_precision_loss)]
        let percent = if total_bytes == 0 {
            0.0
        } else {
            *bytes as f64 * 100.0 / total_bytes as f64
        };
        println!(
            "  {bytes:12} bytes ({percent:5.1}%) in {count:8} records  {name} [{class_index}]"
        );
    }
    println!("Node-shaped references anywhere in GElement bodies:");
    let mut nested = nested_classes.into_iter().collect::<Vec<_>>();
    nested.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    let nested_total = nested.iter().map(|(_, count)| *count).sum::<usize>();
    println!("  total: {nested_total}");
    for (class_index, count) in nested.iter().take(rows) {
        let name = schema
            .class_by_index(*class_index)
            .map_or("<unknown>", |class| class.name.as_str());
        println!(
            "  {count:8} ({:5.1}%)  {name} [{class_index}]",
            share(*count, nested_total)
        );
    }
    // Whether these identifiers are element identifiers is decided by their
    // range and by whether they land on a record in the member that holds the
    // GElement, not by a global lookup: every small integer exists as some
    // record identifier, so a global hit rate says nothing.
    println!("Node identifier space:");
    println!("  largest top-level node object identifier: {node_identifier_maximum}");
    println!(
        "  also a record identifier in the same member: {same_member} ({:.1}%)",
        share(same_member, references)
    );
    println!(
        "  that record carries the declared class: {class_agreed} ({:.1}%)",
        share(class_agreed, references)
    );
    Ok(())
}

fn export_json(
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
struct IfcSettingsArguments<'a> {
    settings: Option<&'a Path>,
    length_unit: Option<LengthUnitArgument>,
    no_revit_property_sets: bool,
    no_revit_type_property_sets: bool,
    no_ifc_common_property_sets: bool,
    base_quantities: bool,
    no_types: bool,
    class_mapping: Option<&'a Path>,
    write_settings: Option<&'a Path>,
}

/// The length units the exporter writes, as a flag. A mirror of
/// [`LengthUnit`], because the exporter does not depend on the argument
/// parser.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum LengthUnitArgument {
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

impl IfcSettingsArguments<'_> {
    /// The setup this run uses: the file where one is given, then every flag
    /// that was actually passed, in that order.
    fn resolve(&self) -> Result<ExportSettings, Box<dyn Error>> {
        let mut settings = match self.settings {
            Some(path) => ExportSettings::from_json_file(path)?,
            None => ExportSettings::default(),
        };
        if let Some(unit) = self.length_unit {
            settings.length_unit = unit.into();
        }
        if self.no_revit_property_sets {
            settings.property_sets.revit_parameters = false;
        }
        if self.no_revit_type_property_sets {
            settings.property_sets.revit_type_parameters = false;
        }
        if self.no_ifc_common_property_sets {
            settings.property_sets.ifc_common = false;
        }
        if self.base_quantities {
            settings.property_sets.base_quantities = true;
        }
        if self.no_types {
            settings.types = false;
        }
        if let Some(path) = self.class_mapping {
            settings.class_mapping_file = Some(path.to_path_buf());
        }
        // The table is read here rather than by the writer: a setup that names
        // a file it cannot read fails before a model is decoded, not after.
        settings.load_class_mapping()?;
        Ok(settings)
    }
}

#[allow(clippy::too_many_arguments)]
/// What the geometry recovery found, which is the same tally whichever export
/// asks for it.
fn report_geometry_recovery(geometry_statistics: &GeometryStatistics) {
    println!(
        "Recovered straight-pipe candidates: {} ({} have GElement bounds; {} match them)",
        geometry_statistics.pipe_candidates,
        geometry_statistics.pipe_candidates_with_bounds,
        geometry_statistics.verified_pipe_lines
    );
    println!(
        "Recovered single-line pipe-fitting centerlines: {} ({} link uniquely to pipe fittings)",
        geometry_statistics.fitting_center_line_candidates,
        geometry_statistics.verified_fitting_axes
    );
    println!(
        "Recovered family-instance placement candidates: {} ({} are unique inside owner bounds)",
        geometry_statistics.family_instances_with_placement_candidates,
        geometry_statistics.verified_family_instance_placements
    );
    println!(
        "Recovered bounds-verified GInstance transforms: {}",
        geometry_statistics.verified_ginstance_transforms
    );
    println!(
        "Recovered transform-verified family-symbol bounds: {}",
        geometry_statistics.verified_symbol_bounds
    );
    report_symbol_link_funnel(geometry_statistics);
    report_nested_assembly_funnel(geometry_statistics);
}

/// The knobs `export-scene` exposes, kept together so that neither the
/// dispatch nor the exporter grows an argument list nobody can read.
struct SceneArguments {
    chord_tolerance_mm: f64,
    refinement_depth: u8,
    chunk_triangles: usize,
    property_block: usize,
    compression: u32,
}

/// Build the binary scene a viewer loads, from whatever formats the files are
/// in.
///
/// One function for every source: the format is settled by [`read_source`] and
/// nothing below it needs to know the answer. This used to be two nearly
/// identical functions - one per format - which is how the two came to report
/// the same facts in different words.
fn export_scene(
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

fn export_ifc(
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
fn default_output(
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
fn source_path_namespace(paths: &[PathBuf]) -> Result<[u8; 16], Box<dyn Error>> {
    let mut name = Vec::new();
    for path in paths {
        let canonical = std::fs::canonicalize(path)?;
        name.extend_from_slice(canonical.as_os_str().as_encoded_bytes());
        name.push(0);
    }
    Ok(uuid_v5(SOURCE_PATH_NAMESPACE, &name))
}

/// A clock over the conversion's stages.
///
/// With `--progress` each stage announces its beginning and its end on stdout
/// as one JSON object, flushed immediately, so a caller can show the stage
/// that is running as well as the ones that finished. Without it the same
/// numbers are printed as prose at the end, because a person reading a
/// terminal wants the summary and not a log.
struct Stage {
    started: std::time::Instant,
    began: std::time::Instant,
    progress: bool,
    elapsed: Vec<(&'static str, f64)>,
}

impl Stage {
    fn new(progress: bool) -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            began: now,
            progress,
            elapsed: Vec::new(),
        }
    }

    fn announce(&self, value: &str) {
        if self.progress {
            println!("{value}");
            let _ = io::stdout().flush();
        }
    }

    fn begins(&mut self, name: &'static str) {
        self.began = std::time::Instant::now();
        self.announce(&format!(
            "{{\"stage\":\"{name}\",\"event\":\"begin\",\"totalSeconds\":{:.3}}}",
            self.started.elapsed().as_secs_f64()
        ));
    }

    fn finished(&mut self, name: &'static str) {
        let seconds = self.began.elapsed().as_secs_f64();
        self.elapsed.push((name, seconds));
        self.announce(&format!(
            "{{\"stage\":\"{name}\",\"event\":\"end\",\"seconds\":{seconds:.3},\"totalSeconds\":{:.3}}}",
            self.started.elapsed().as_secs_f64()
        ));
    }

    fn total(&self) {
        if self.progress {
            return;
        }
        for (name, seconds) in &self.elapsed {
            println!("{}: {seconds:.2}s", stage_label(name));
        }
        println!("Total: {:.2}s", self.started.elapsed().as_secs_f64());
    }
}

fn stage_label(name: &str) -> &'static str {
    match name {
        "decode" => "Decode",
        "model" => "Model",
        "tessellate" => "Tessellate and pack",
        _ => "Stage",
    }
}

/// Read the command's arguments as pack options, refusing the values the
/// format cannot express before a long decode has been paid for.
/// What reading a source file costs and how much of it to read, whatever the
/// format turns out to be.
struct ReadOptions {
    /// Include categorized records without a recovered level association.
    /// Meaningful only where the source leaves containment to be recovered,
    /// which an IFC does not.
    include_unplaced: bool,
    /// Stop after this many elements, per source file.
    limit: Option<usize>,
    /// Largest decoded RVT member, or largest IFC source text, accepted.
    max_bytes: u64,
    /// The furthest a chord may sit from the curve it approximates, in
    /// metres, where the reader tessellates a curve to build the model.
    chord_tolerance: f64,
}

/// One source file, read into the canonical model.
struct SourceModel {
    format: Format,
    /// The file's own name, which the scene records as its provenance.
    name: String,
    model: BimModel,
    detail: SourceDetail,
}

/// The findings that belong to one reader and have no counterpart in another.
///
/// These are deliberately not flattened into shared counters. "Products whose
/// representation held nothing this reads" and "faces the tessellator could
/// not read" are different facts about different stages, and a single number
/// covering both would say neither - which is what rules 8 and 12 of
/// `BRIEF.md` forbid. Generic code carries this and prints it through the
/// `report_*` methods below; it never interprets it.
enum SourceDetail {
    Rvt(Box<RvtDetail>),
    Ifc(IfcDetail),
}

/// What the record walk recovered and how much of each kind it established.
///
/// Only the RVT reader has any of this: containment, placement and typing are
/// all recovered rather than stated, so how much of each was recovered is
/// itself a result. Boxed into its variant, because the tallies alone are far
/// larger than everything the STEP reader reports.
struct RvtDetail {
    geometry: GeometryStatistics,
    /// Counted here rather than at report time because the join is by element
    /// identifier, and a federated model has qualified every one of them.
    mapped_family_instances: usize,
    mapped_family_instance_placements: usize,
    included_properties: usize,
    included_type_properties: usize,
    omitted_properties: usize,
}

/// What the STEP reader read, and the parts of the file it does not cover.
///
/// An IFC states its own containment, so nothing here is about recovery; it
/// is about what this reader does not yet read.
struct IfcDetail {
    entities: usize,
    skipped: usize,
    read: ifc_import::Read,
}

/// Read a source file into the canonical model, whichever format it is in.
///
/// The format is read from the file's own leading bytes, so a conversion is
/// driven by what a file *is* rather than by what it is called. This is the
/// one place that dispatches on it: adding a format means adding an arm here
/// and a reader crate behind it, not a branch in every export.
///
/// The stages are named the same way for every format, because they answer
/// the same three questions - what did reading the file cost, what did making
/// a model of it cost, what did writing the output cost - and a caller
/// driving a progress display should not have to know which format it was
/// handed.
fn read_source(
    path: &Path,
    options: &ReadOptions,
    stage: &mut Stage,
) -> Result<SourceModel, Box<dyn Error>> {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let format = match Format::sniff_file(path)? {
        Some(format) if format.is_readable() => format,
        Some(format) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is a {}, which no reader in this workspace covers yet",
                    path.display(),
                    format.label()
                ),
            )
            .into());
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} begins with the signature of no format this reads",
                    path.display()
                ),
            )
            .into());
        }
    };
    match format {
        Format::Rvt => read_rvt_source(path, name, options, stage),
        Format::Ifc => read_ifc_source(path, name, options, stage),
        // `is_readable` already refused this above; matched rather than
        // wildcarded so that adding a format is a compile error here.
        Format::Dwg => unreachable!("a format without a reader is refused above"),
    }
}

fn read_rvt_source(
    path: &Path,
    name: String,
    options: &ReadOptions,
    stage: &mut Stage,
) -> Result<SourceModel, Box<dyn Error>> {
    stage.begins("decode");
    let recovered = recover_elements(path, options.max_bytes)?;
    stage.finished("decode");

    stage.begins("model");
    let geometry = geometry_statistics(&recovered.elements, recovered.schema.as_ref());
    let (model, included_properties, included_type_properties, omitted_properties) =
        metadata_model(&recovered, options.include_unplaced, options.limit);
    let (mapped_family_instances, mapped_family_instance_placements) =
        mapped_family_instance_placement_counts(&model, &recovered);
    stage.finished("model");

    Ok(SourceModel {
        format: Format::Rvt,
        name,
        model,
        detail: SourceDetail::Rvt(Box::new(RvtDetail {
            geometry,
            mapped_family_instances,
            mapped_family_instance_placements,
            included_properties,
            included_type_properties,
            omitted_properties,
        })),
    })
}

fn read_ifc_source(
    path: &Path,
    name: String,
    options: &ReadOptions,
    stage: &mut Stage,
) -> Result<SourceModel, Box<dyn Error>> {
    // The STEP reader holds the whole file, so its cost is a multiple of the
    // source rather than of one bounded member; the ceiling is checked before
    // a long read is paid for. See `Format::memory_ratio`.
    let source_bytes = std::fs::metadata(path)?.len();
    if source_bytes > options.max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "IFC source is {source_bytes} bytes, above the configured {}-byte \
                 safe parsing limit; raise --max-member-bytes only on a host with enough memory",
                options.max_bytes
            ),
        )
        .into());
    }

    stage.begins("decode");
    let bytes = std::fs::read(path)?;
    let parsed = ifc_import::parse(&bytes).map_err(io::Error::other)?;
    let entities = parsed.entities.len();
    let skipped = parsed.skipped;
    drop(bytes);
    stage.finished("decode");

    stage.begins("model");
    let import = ifc_import::convert(
        &parsed,
        &ifc_import::Options {
            chord_tolerance: options.chord_tolerance,
        },
    );
    let mut model = import.model;
    // Every product a file states is kept: unlike a decoded RVT, an IFC
    // states its own containment, so an element without a storey is one the
    // file placed elsewhere rather than one this failed to place. That is
    // also why `include_unplaced` has no effect here.
    if let Some(limit) = options.limit {
        model.elements.truncate(limit);
    }
    drop(parsed);
    stage.finished("model");

    Ok(SourceModel {
        format: Format::Ifc,
        name,
        model,
        detail: SourceDetail::Ifc(IfcDetail {
            entities,
            skipped,
            read: import.read,
        }),
    })
}

impl SourceDetail {
    /// Whether each section below would print anything for this source.
    ///
    /// Asked so that a federated conversion can put a file's name above its
    /// own lines without leaving a bare heading over nothing.
    fn reports_read(&self) -> bool {
        matches!(self, Self::Ifc(_))
    }

    fn reports_model(&self) -> bool {
        match self {
            Self::Rvt(_) => false,
            Self::Ifc(IfcDetail { read, .. }) => {
                read.without_geometry > 0
                    || read.unread_items > 0
                    || read.approximated_items > 0
                    || read.openings > 0
                    || read.unread_placements > 0
            }
        }
    }

    fn reports_geometry_funnels(&self) -> bool {
        matches!(self, Self::Rvt(_))
    }

    fn reports_properties(&self) -> bool {
        matches!(self, Self::Rvt(_))
    }

    /// What reading the file itself found, printed before anything about the
    /// model built from it.
    fn report_read(&self) {
        match self {
            // The record walk's own findings are reported with the model,
            // because every one of them is about what was recovered from it.
            Self::Rvt(_) => {}
            Self::Ifc(IfcDetail {
                entities,
                skipped,
                read,
            }) => {
                println!("Entities read: {entities}");
                if *skipped > 0 {
                    println!("Entities this reader could not read: {skipped}");
                }
                if !read.stated_length_unit {
                    println!("The file declared no length unit; its numbers are read as metres.");
                }
            }
        }
    }

    /// What the model built from the file does and does not carry.
    fn report_model(&self) {
        match self {
            Self::Rvt(_) => {}
            Self::Ifc(IfcDetail { read, .. }) => {
                if read.without_geometry > 0 {
                    println!(
                        "Products whose representation held nothing this reads: {}",
                        read.without_geometry
                    );
                }
                if read.unread_items > 0 {
                    println!(
                        "Representation items this reader does not read: {}",
                        read.unread_items
                    );
                }
                if read.approximated_items > 0 {
                    // A boolean result is drawn as the solid it cuts from, so
                    // these are shapes shown larger than the file states them.
                    println!(
                        "Solids drawn without a cut the file states: {}",
                        read.approximated_items
                    );
                }
                if read.openings > 0 {
                    println!(
                        "Openings, which are voids rather than bodies: {}",
                        read.openings
                    );
                }
                if read.unread_placements > 0 {
                    println!(
                        "Placements this reader could not resolve: {}",
                        read.unread_placements
                    );
                }
            }
        }
    }

    /// The recovery funnels `export-ifc` reports: how much of the source's
    /// geometry was established well enough to reach the file. Only a reader
    /// that had to recover it has any.
    fn report_geometry_funnels(&self) {
        match self {
            Self::Rvt(detail) => {
                report_geometry_recovery(&detail.geometry);
                println!(
                    "Mapped family instances with verified placement: {} of {}",
                    detail.mapped_family_instance_placements, detail.mapped_family_instances
                );
            }
            Self::Ifc(_) => {}
        }
    }

    /// The properties the reader recovered, where recovering them was its
    /// job. An IFC states its properties, so it counts none.
    fn report_properties(&self) {
        match self {
            Self::Rvt(detail) => {
                println!("Recovered Revit properties: {}", detail.included_properties);
                println!(
                    "Recovered Revit properties from the element's type: {}",
                    detail.included_type_properties
                );
                if detail.omitted_properties > 0 {
                    println!(
                        "Unverified parameter candidates omitted for this Revit release: {}",
                        detail.omitted_properties
                    );
                }
            }
            Self::Ifc(_) => {}
        }
    }
}

/// The sources of one conversion, read and assembled into the model every
/// writer works from.
struct Conversion {
    /// One source's own model, or several federated into one.
    model: BimModel,
    /// One entry per source file, in the order they were given, keeping the
    /// findings only that file's own reader could state.
    sources: Vec<SourceReport>,
    federation: BimFederationReport,
}

/// What reading one source file found, kept after its model has been merged
/// into the conversion's.
struct SourceReport {
    /// The name this file's elements are qualified by in a federated model,
    /// which is what a reader of the report needs to join the two.
    document: BimDocumentId,
    name: String,
    detail: SourceDetail,
}

/// Read every source file and assemble them into one model.
///
/// With one file this is that file's model unchanged, identifiers included.
/// With several it is a federated model: see [`bim_core::federate`] for what
/// that does to identifiers and, just as importantly, what it does not do to
/// coordinates.
fn read_sources(
    paths: &[PathBuf],
    options: &ReadOptions,
    stage: &mut Stage,
) -> Result<Conversion, Box<dyn Error>> {
    if paths.is_empty() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "name at least one source file").into(),
        );
    }
    let mut sources = Vec::new();
    let mut assembled = Vec::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for path in paths {
        let source = read_source(path, options, stage)?;
        let document = BimDocument {
            id: BimDocumentId(document_id(path, &mut taken)),
            name: source.name.clone(),
            kind: source.format.source_kind().to_owned(),
            source: source.model.source.clone(),
            // `federate` fills this from the model it is handed.
            elements: 0,
        };
        sources.push(SourceReport {
            document: document.id.clone(),
            name: source.name,
            detail: source.detail,
        });
        assembled.push((document, source.model));
    }
    let (model, federation) = bim_core::federate(assembled);
    Ok(Conversion {
        model,
        sources,
        federation,
    })
}

/// A short, stable name for one source file inside a federated model.
///
/// The file's own stem, because that is what a person reading a federated
/// model recognises, with a counter appended where two files in one set share
/// it. The identifier prefixes every element id in the output, so it has to
/// be both readable and unique.
fn document_id(path: &Path, taken: &mut BTreeSet<String>) -> String {
    let stem: String = path
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let stem = stem.trim_matches('_');
    let base = if stem.is_empty() { "source" } else { stem };
    let mut candidate = base.to_owned();
    let mut next = 2;
    while !taken.insert(candidate.clone()) {
        candidate = format!("{base}-{next}");
        next += 1;
    }
    candidate
}

impl Conversion {
    /// The provenance a scene records for this conversion.
    fn info(&self) -> SourceInfo {
        if let [only] = &self.model.documents[..] {
            return SourceInfo {
                name: only.name.clone(),
                kind: only.kind.clone(),
                application: only
                    .source
                    .as_ref()
                    .map(|source| source.application.clone()),
                release: only
                    .source
                    .as_ref()
                    .and_then(|source| source.release.clone()),
            };
        }
        // A federated set has no one application behind it, and its kind is
        // the one its files agree on. Naming a mixed set `ifc` would tell a
        // reader of the scene that every class name in it came from an IFC.
        let kinds: BTreeSet<&str> = self
            .model
            .documents
            .iter()
            .map(|document| document.kind.as_str())
            .collect();
        let names: Vec<&str> = self
            .model
            .documents
            .iter()
            .map(|document| document.name.as_str())
            .collect();
        SourceInfo {
            name: names.join(", "),
            kind: if kinds.len() == 1 {
                kinds.iter().copied().next().unwrap_or_default().to_owned()
            } else {
                "mixed".to_owned()
            },
            application: None,
            release: None,
        }
    }

    /// What reading the files found, before anything about the model built
    /// from them. Each file's findings are printed by the reader that read
    /// it, under its own name where there is more than one.
    fn report_read(&self) {
        self.per_source(SourceDetail::reports_read, SourceDetail::report_read);
    }

    /// What the model does and does not carry, per source.
    fn report_model(&self) {
        self.per_source(SourceDetail::reports_model, SourceDetail::report_model);
    }

    /// The recovery funnels `export-ifc` reports.
    fn report_geometry_funnels(&self) {
        self.per_source(
            SourceDetail::reports_geometry_funnels,
            SourceDetail::report_geometry_funnels,
        );
    }

    /// The properties the readers recovered, where recovering them was their
    /// job.
    fn report_properties(&self) {
        self.per_source(
            SourceDetail::reports_properties,
            SourceDetail::report_properties,
        );
    }

    /// Print one section for every source that has something to say in it.
    ///
    /// A single source prints exactly what it always did, with no heading. A
    /// federated one puts each file's name above its own lines, because
    /// otherwise two files' counts arrive as an unattributed pair.
    fn per_source(&self, has: impl Fn(&SourceDetail) -> bool, report: impl Fn(&SourceDetail)) {
        let several = self.sources.len() > 1;
        for source in &self.sources {
            if !has(&source.detail) {
                continue;
            }
            if several {
                println!("{} ({}):", source.document.0, source.name);
            }
            report(&source.detail);
        }
    }

    /// What assembling several files did, and the one thing it could not do.
    fn report_federation(&self) {
        if self.model.documents.len() < 2 {
            return;
        }
        println!("Federated documents: {}", self.federation.documents);
        for document in &self.model.documents {
            println!(
                "  {}: {} elements from {}",
                document.id.0, document.elements, document.name
            );
        }
        if self.federation.collisions > 0 {
            println!(
                "Element identifiers claimed by more than one document, kept apart by \
                 qualification: {}",
                self.federation.collisions
            );
        }
        for (left, right) in &self.federation.disjoint {
            // Reported and not corrected: nothing here knows the transform
            // that would reconcile two origins. See `BimFederationReport`.
            println!(
                "warning: {} and {} state no overlapping geometry, so they are probably about \
                 different origins; nothing was moved",
                left.0, right.0
            );
        }
    }
}

fn pack_options(arguments: &SceneArguments) -> Result<PackOptions, io::Error> {
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
fn describe_bytes(bytes: u64) -> String {
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

/// Report which classes own the decoded bodies, and which classes of model
/// element reach one. `rivet brep` says how much geometry comes out of the
/// file; this says whose it is and what carries it out.
fn body_owners(path: &Path, classes: usize, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let recovered = recover_elements(path, max_member_bytes)?;
    let schema = recovered.schema.as_ref();
    let rows = tally_class_geometry(&recovered.elements, |element| {
        element.class_index.and_then(|index| {
            schema
                .and_then(|schema| schema.class_by_index(index))
                .map(|class| class.name.as_str())
        })
    });

    let total = rows
        .values()
        .fold(ClassGeometry::default(), |mut total, row| {
            total.body_ids += row.body_ids;
            total.body_records += row.body_records;
            total.complete_bodies += row.complete_bodies;
            total.faces += row.faces;
            total.named_by_an_instance += row.named_by_an_instance;
            total.verified_by_an_instance += row.verified_by_an_instance;
            total.bodies_with_bounds += row.bodies_with_bounds;
            total.bodies_matching_their_bounds += row.bodies_matching_their_bounds;
            total.bodies_with_graph_bounds += row.bodies_with_graph_bounds;
            total.bodies_matching_their_graph_bounds += row.bodies_matching_their_graph_bounds;
            total.bodies_placed_only_by_graph_bounds += row.bodies_placed_only_by_graph_bounds;
            total.bodies_placed_only_by_near_duplicate_bounds +=
                row.bodies_placed_only_by_near_duplicate_bounds;
            total.bodies_placed_only_by_another_box += row.bodies_placed_only_by_another_box;
            total.graph_bounds_differing_from_exact += row.graph_bounds_differing_from_exact;
            total.bodies_away_from_the_origin += row.bodies_away_from_the_origin;
            total.model_elements += row.model_elements;
            total.model_elements_with_their_own_body += row.model_elements_with_their_own_body;
            total.model_elements_with_a_placed_body += row.model_elements_with_a_placed_body;
            total.model_elements_with_a_verified_symbol_body +=
                row.model_elements_with_a_verified_symbol_body;
            total
        });
    println!(
        "Element ids owning a decoded body: {} from {} body-bearing GElement records",
        total.body_ids, total.body_records
    );
    println!(
        "  bodies held out by keeping one per id: {}",
        total.body_records.saturating_sub(total.body_ids)
    );
    println!(
        "  complete bodies: {} ({} faces)",
        total.complete_bodies, total.faces
    );
    println!(
        "  ids an instance names as its symbol: {} ({} bounds-verified)",
        total.named_by_an_instance, total.verified_by_an_instance
    );
    println!(
        "  bodies reproducing their record's own bounds: {} of {} that carry one",
        total.bodies_matching_their_bounds, total.bodies_with_bounds
    );
    println!(
        "  bodies whose centre is over a foot from the origin: {}",
        total.bodies_away_from_the_origin
    );
    println!(
        "  bodies reproducing their record's graph-header box: {} of {} that carry one \
         ({} of those boxes are not the exact block)",
        total.bodies_matching_their_graph_bounds,
        total.bodies_with_graph_bounds,
        total.graph_bounds_differing_from_exact
    );
    println!(
        "  of the bodies with no exact block, placed by the graph box: {}, by the near \
         duplicate: {}, by either: {}",
        total.bodies_placed_only_by_graph_bounds,
        total.bodies_placed_only_by_near_duplicate_bounds,
        total.bodies_placed_only_by_another_box
    );

    print_body_box_residuals(&recovered.elements);
    print_near_miss_anatomy(
        &recovered.elements,
        |element| {
            element.class_index.and_then(|index| {
                schema
                    .and_then(|schema| schema.class_by_index(index))
                    .map(|class| class.name.as_str())
            })
        },
        classes,
    );

    print_body_owner_table(&rows, classes);
    print_model_element_body_table(&rows, &total, classes);
    // The same funnel the export prints, here too: this is the command for
    // reading what the decode is worth, and the nested-family path is decoded
    // geometry that does or does not reach an element.
    report_nested_assembly_funnel(&geometry_statistics(
        &recovered.elements,
        recovered.schema.as_ref(),
    ));
    Ok(())
}

/// How closely each box on a body's own record reproduces that body.
///
/// A second placement tier is only worth adding if agreement with the graph
/// header's box is as sharp as agreement with the exact block: a box that is
/// merely near the body is a different box, not a looser reading of the same
/// one. The rows split on whether the record also carried an exact block,
/// because the bodies that carry none are the ones a second tier would add.
fn print_body_box_residuals(elements: &BTreeMap<u32, ExportedElement>) {
    /// Upper bound of each bucket in Revit internal feet, and its label.
    const BUCKETS: [(f64, &str); 6] = [
        (BODY_BOUNDS_TOLERANCE_FEET, "<=1e-6ft"),
        (1e-4, "<=1e-4ft"),
        (1e-2, "<=1e-2ft"),
        (1.0, "<=1ft"),
        (100.0, "<=100ft"),
        (f64::INFINITY, ">100ft"),
    ];
    // Rows: graph box with an exact block present, graph box without one,
    // near duplicate without one. Columns: the buckets, then "no such box".
    let mut rows = [[0_usize; BUCKETS.len() + 1]; 3];
    for element in elements.values() {
        if element.brep.is_none() {
            continue;
        }
        let residuals = element.brep_box_residuals;
        let measured: &[(usize, Option<f64>)] = if residuals.exact.is_some() {
            &[(0, residuals.graph)]
        } else {
            &[(1, residuals.graph), (2, residuals.near_duplicate)]
        };
        for &(row, residual) in measured {
            let column = residual.map_or(BUCKETS.len(), |residual| {
                BUCKETS
                    .iter()
                    .position(|(bound, _)| residual <= *bound)
                    .unwrap_or(BUCKETS.len() - 1)
            });
            rows[row][column] += 1;
        }
    }
    println!("\nHow far each box sits from the body on its own record:");
    print!("box");
    for (_, label) in BUCKETS {
        print!("\t{label}");
    }
    println!("\tno box");
    for (row, name) in [
        ("graph box, exact block present", 0),
        ("graph box, no exact block", 1),
        ("near duplicate, no exact block", 2),
    ]
    .map(|(name, row)| (row, name))
    {
        print!("{name}");
        for count in rows[row] {
            print!("\t{count}");
        }
        println!();
    }
}

/// What a body that misses its own record's box actually differs by.
///
/// A residual alone cannot say why: the same tenth of a foot is a body sitting
/// somewhere else, a body that is the wrong size, or a body missing a face.
/// Each is a different repair, so each is counted separately. The three
/// readings are independent of one another and of the residual buckets above.
fn print_near_miss_anatomy<'a>(
    elements: &BTreeMap<u32, ExportedElement>,
    class_name: impl Fn(&ExportedElement) -> Option<&'a str>,
    classes: usize,
) {
    #[derive(Default)]
    struct Anatomy {
        misses: usize,
        /// The body has the box's extent on every axis and sits elsewhere:
        /// a transform the decode does not apply.
        same_size_shifted: usize,
        /// The body lies inside the box on every axis: geometry not decoded,
        /// or a box drawn around more than this body.
        inside_the_box: usize,
        /// ...and of those, how many are bodies with an excluded face, which
        /// is the reading that would explain it.
        inside_and_incomplete: usize,
        /// The body reaches outside its own box on some axis.
        outside_the_box: usize,
        /// Bodies whose id carries more than one body-bearing record, so the
        /// box may belong to a body this one displaced.
        from_a_multi_record_id: usize,
        /// What the record declares - see [`place_declared_body`]: how many
        /// bodies, how many of those reproduce the box, and how many are
        /// solids rather than free surfaces.
        declared_bodies: usize,
        bodies_matching_the_box: usize,
        solid_bodies: usize,
        /// Records with no declared body at all: nothing to select from.
        with_no_declared_body: usize,
    }
    let mut rows: BTreeMap<&str, Anatomy> = BTreeMap::new();
    let mut total = Anatomy::default();
    for element in elements.values() {
        let (Some(brep), Some(bounds)) = (&element.brep, element.brep_placement_box) else {
            continue;
        };
        let Some(residual) = body_bounds_residual_feet(brep, &bounds) else {
            continue;
        };
        if residual <= BODY_BOUNDS_TOLERANCE_FEET {
            continue;
        }
        let Some((min, max)) = body_extent_feet(brep) else {
            continue;
        };
        let size_residual = (0..3)
            .map(|axis| ((max[axis] - min[axis]) - (bounds.max[axis] - bounds.min[axis])).abs())
            .fold(0.0_f64, f64::max);
        let inside = (0..3).all(|axis| {
            min[axis] >= bounds.min[axis] - BODY_BOUNDS_TOLERANCE_FEET
                && max[axis] <= bounds.max[axis] + BODY_BOUNDS_TOLERANCE_FEET
        });
        let matching = (0..brep.bodies.len())
            .filter(|index| {
                brep.body(*index)
                    .is_some_and(|body| body_is_placed_in(&body, &bounds))
            })
            .count();
        let solids = brep.bodies.iter().filter(|body| body.is_solid()).count();
        let row = rows
            .entry(class_name(element).unwrap_or(UNRESOLVED_CLASS))
            .or_default();
        for row in [row, &mut total] {
            row.misses += 1;
            row.same_size_shifted += usize::from(size_residual <= BODY_BOUNDS_TOLERANCE_FEET);
            row.inside_the_box += usize::from(inside);
            row.inside_and_incomplete += usize::from(inside && !brep.excluded_faces.is_empty());
            row.outside_the_box += usize::from(!inside);
            row.from_a_multi_record_id += usize::from(element.brep_records > 1);
            row.declared_bodies += brep.bodies.len();
            row.bodies_matching_the_box += matching;
            row.solid_bodies += solids;
            row.with_no_declared_body += usize::from(brep.bodies.is_empty());
        }
    }
    let mut by_misses = rows.iter().collect::<Vec<_>>();
    by_misses.sort_by(|left, right| {
        right
            .1
            .misses
            .cmp(&left.1.misses)
            .then_with(|| left.0.cmp(right.0))
    });
    println!("\nWhat a body missing its own box differs by:");
    println!(
        "class\tmisses\tsame size, moved\tinside it\toutside it\tmulti-record id\t\
         declared bodies\t=box\tsolid\tno body"
    );
    let listed = by_misses
        .into_iter()
        .take(classes)
        .map(|(name, row)| (*name, row))
        .chain(std::iter::once(("total", &total)));
    for (name, row) in listed {
        println!(
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.misses,
            row.same_size_shifted,
            row.inside_the_box,
            row.outside_the_box,
            row.from_a_multi_record_id,
            row.declared_bodies,
            row.bodies_matching_the_box,
            row.solid_bodies,
            row.with_no_declared_body
        );
    }
}

/// Which classes own the decoded bodies, most bodies first.
fn print_body_owner_table(rows: &BTreeMap<&str, ClassGeometry>, classes: usize) {
    let mut by_bodies = rows.iter().collect::<Vec<_>>();
    by_bodies.sort_by(|left, right| {
        right
            .1
            .body_ids
            .cmp(&left.1.body_ids)
            .then_with(|| left.0.cmp(right.0))
    });
    println!("\nWho owns the bodies:");
    println!(
        "class\tids\trecords\tcomplete\tfaces\tnamed\tverified\t=bounds\t+graph\t+dup\t+either\toff-origin"
    );
    for (name, row) in by_bodies
        .iter()
        .filter(|(_, row)| row.body_ids > 0)
        .take(classes)
    {
        println!(
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}/{}\t{}\t{}\t{}\t{}",
            row.body_ids,
            row.body_records,
            row.complete_bodies,
            row.faces,
            row.named_by_an_instance,
            row.verified_by_an_instance,
            row.bodies_matching_their_bounds,
            row.bodies_with_bounds,
            row.bodies_placed_only_by_graph_bounds,
            row.bodies_placed_only_by_near_duplicate_bounds,
            row.bodies_placed_only_by_another_box,
            row.bodies_away_from_the_origin
        );
    }
}

/// Which classes of model element reach a body, most elements first.
fn print_model_element_body_table(
    rows: &BTreeMap<&str, ClassGeometry>,
    total: &ClassGeometry,
    classes: usize,
) {
    let mut by_model_elements = rows.iter().collect::<Vec<_>>();
    by_model_elements.sort_by(|left, right| {
        right
            .1
            .model_elements
            .cmp(&left.1.model_elements)
            .then_with(|| left.0.cmp(right.0))
    });
    println!("\nWhich model elements reach a body:");
    println!("class\tmodel elements\town body\tplaced\tverified symbol body");
    for (name, row) in by_model_elements
        .iter()
        .filter(|(_, row)| row.model_elements > 0)
        .take(classes)
    {
        println!(
            "{name}\t{}\t{}\t{}\t{}",
            row.model_elements,
            row.model_elements_with_their_own_body,
            row.model_elements_with_a_placed_body,
            row.model_elements_with_a_verified_symbol_body
        );
    }
    println!(
        "total\t{}\t{}\t{}\t{}",
        total.model_elements,
        total.model_elements_with_their_own_body,
        total.model_elements_with_a_placed_body,
        total.model_elements_with_a_verified_symbol_body
    );
}

/// Report the funnel from "an instance names a symbol" to "that body is
/// placed in the world". A shortfall in exported geometry is almost always one
/// of these gates, and reading which one is what stops the next change being a
/// guess: it is how the category-equality gate was found to be costing
/// thousands of geometrically verified links while refusing nothing wrong.
fn report_symbol_link_funnel(statistics: &GeometryStatistics) {
    println!(
        "Instances naming a symbol through their GInstance transform: {}",
        statistics.instances_naming_a_symbol
    );
    println!(
        "  whose symbol carries a decoded body: {}",
        statistics.instances_whose_symbol_has_a_body
    );
    println!(
        "  and which carry their own bounds: {}",
        statistics.instances_with_a_symbol_body_and_own_bounds
    );
    println!(
        "  and whose category matches the symbol's: {}",
        statistics.instances_whose_category_matches_the_symbol
    );
    println!(
        "  and whose bounds match the transformed symbol box: {}",
        statistics.instances_whose_bounds_match_the_symbol
    );
    println!(
        "  bounds match with the category gate not applied: {}",
        statistics.instances_whose_bounds_match_ignoring_category
    );
    println!(
        "  the cross-check refuses {}: {} declare several placements, \
         the instance box holds the symbol's {}, is inside it {}, neither {}",
        statistics.instances_failing_the_box_cross_check,
        statistics.failing_with_several_placements,
        statistics.failing_whose_box_holds_the_symbols,
        statistics.failing_whose_box_is_inside_the_symbols,
        statistics.failing_with_neither_box_inside
    );
    let mut gaps = String::new();
    for (index, (name, _)) in BOX_GAP_BUCKETS.iter().enumerate() {
        let _ = write!(gaps, " {name}:{}", statistics.box_gap_buckets[index]);
    }
    println!("  by the largest of the six coordinate gaps, in feet:{gaps}");
    println!(
        "  of the refusals, agreeing with the id's last box instead: {}",
        statistics.failing_that_agree_with_the_id_s_last_box
    );
    println!(
        "  category gate: symbol has none {}, differs {}",
        statistics.instances_whose_symbol_has_no_category,
        statistics.instances_whose_category_differs_from_the_symbol
    );
    println!(
        "  placement declared by InstInfoBase: {} ({} elements declare several), \
         found only by the scan it replaced: {}",
        statistics.elements_declaring_a_placement,
        statistics.elements_declaring_several_placements,
        statistics.placements_only_the_scan_found
    );
    println!(
        "  read both ways: {}, agreeing: {}",
        statistics.placements_read_both_ways, statistics.placements_the_two_readings_agree_on
    );
    println!(
        "  elements declaring several, all naming a symbol: {} ({} placements), symbols decoded: {}, hull of the transformed symbol boxes = the element's box: {} ({} of those placements carry a body)",
        statistics.elements_with_several_placements_naming_symbols,
        statistics.placements_of_elements_with_several,
        statistics.elements_with_several_placements_whose_symbols_are_known,
        statistics.elements_with_several_placements_matching_their_box,
        statistics.placements_whose_symbol_has_a_body
    );
    for (index, label) in [
        "instance bBox = symbol bBox",
        "instance bBox = symbol tight",
        "instance tight = symbol bBox",
        "instance tight = symbol tight",
    ]
    .into_iter()
    .enumerate()
    {
        println!("  {label}: {}", statistics.bounds_match_by_box_pair[index]);
    }
}

/// Report where the nested-family path stops. The hull check is what verifies
/// the set is the element, so a refusal after it is geometry the file offers
/// and this reader did not take.
fn report_nested_assembly_funnel(statistics: &GeometryStatistics) {
    println!("Elements declaring several placements, by what the assembly path did:");
    let mut refusals = statistics.nested_refusals.iter().collect::<Vec<_>>();
    refusals.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
    for (refusal, count) in refusals {
        println!("  {count}\t{refusal}");
    }
    if !statistics.nested_members_without_a_body.is_empty() {
        println!("  the members of the elements a missing body held back:");
        let mut members = statistics
            .nested_members_without_a_body
            .iter()
            .collect::<Vec<_>>();
        members.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        for (verdict, count) in members {
            println!("  {count}\t{verdict}");
        }
        println!("  the classes of the elements a missing body holds back:");
        let mut classes = statistics.nested_refused_classes.iter().collect::<Vec<_>>();
        classes.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        for (class, count) in classes.into_iter().take(8) {
            println!("  {count}\t{}", escape_terminal_text(class));
        }
        println!("  the node classes those members' graphs carry:");
        let mut nodes = statistics
            .nested_bodiless_member_nodes
            .iter()
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        for (class, count) in nodes.into_iter().take(8) {
            println!("  {count}\t{}", escape_terminal_text(class));
        }
    }
    if !statistics.nested_member_bodies.is_empty() {
        println!("  the members of the elements a body held back:");
        let mut members = statistics.nested_member_bodies.iter().collect::<Vec<_>>();
        members.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        for (verdict, count) in members {
            println!("  {count}\t{verdict}");
        }
    }
}

fn same_existing_file(left: &Path, right: &Path) -> io::Result<bool> {
    if left == right {
        return Ok(true);
    }
    if !right.exists() {
        return Ok(false);
    }
    Ok(std::fs::canonicalize(left)? == std::fs::canonicalize(right)?)
}

fn parse_uuid(value: &str) -> Result<[u8; 16], io::Error> {
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

fn format_uuid(uuid: [u8; 16]) -> String {
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

fn current_utc_timestamp() -> Result<(i64, String), io::Error> {
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
fn civil_date_from_days(days: i64) -> (i64, i64, i64) {
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

struct ExportMetadata<'a> {
    schema: Option<&'a Schema>,
    partition_paths: &'a [String],
    parameter_names: &'a BTreeMap<i32, String>,
    parameter_specs: &'a BTreeMap<i32, String>,
    catalog: Option<Catalog>,
    /// Write every decoded section rather than the selection the IFC export
    /// would make. See [`write_model_json`] and [`write_body_json`].
    full: bool,
}

fn write_exported_elements(
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
fn write_resolved_reference_names(
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
fn write_model_json(
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
fn write_model_classes_json(
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
struct ModelTally {
    records: usize,
    with_class: usize,
    with_category: usize,
    with_level: usize,
    with_name: usize,
    with_type: usize,
    moribund: usize,
    parameter_values: usize,
    type_parameter_values: usize,
    transforms: usize,
    verified_symbol_links: usize,
    body_elements: usize,
    body_records: usize,
    placed_bodies: usize,
    complete_bodies: usize,
    faces: usize,
    excluded_faces: usize,
    edges: usize,
    failed_edges: usize,
    classes: BTreeMap<u16, usize>,
}

impl ModelTally {
    fn of(elements: &BTreeMap<u32, ExportedElement>) -> Self {
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
fn json_identifier(value: &str) -> String {
    value.parse::<i64>().map_or_else(
        |_| format!("\"{}\"", json_escape(value)),
        |id| id.to_string(),
    )
}

fn json_number(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        "null".to_owned()
    }
}

fn write_axis_json(writer: &mut impl Write, name: &str, axis: [f64; 3]) -> io::Result<()> {
    write!(
        writer,
        ",\"{name}\":[{},{},{}]",
        json_number(axis[0]),
        json_number(axis[1]),
        json_number(axis[2])
    )
}

fn write_point_json(writer: &mut impl Write, name: &str, point: &BimPoint3) -> io::Result<()> {
    write_axis_json(writer, name, point.coordinates)
}

/// One profile curve. A revolved surface holds its profile in the surface's
/// own frame; a ruled surface holds both of its profiles in world
/// coordinates, because that is where the source states them.
fn write_brep_profile_json(writer: &mut impl Write, profile: &BimBrepProfile) -> io::Result<()> {
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
fn write_brep_ruling_json(writer: &mut impl Write, ruling: &BimBrepRuling) -> io::Result<()> {
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
fn write_brep_surface_json(writer: &mut impl Write, surface: &BimBrepSurface) -> io::Result<()> {
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
fn write_brep_faces_json(writer: &mut impl Write, brep: &BimBrep) -> io::Result<()> {
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
fn write_decoded_sections_json(
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
fn write_body_json(
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
fn write_bounds_json(writer: &mut impl Write, element: &ExportedElement) -> io::Result<()> {
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
fn write_curve_candidates_json(
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
fn write_element_id_fields(writer: &mut impl Write, element: &ExportedElement) -> io::Result<()> {
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

fn write_element_json(
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
fn write_material_layers_json(
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

fn write_geometry_json(
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

fn write_family_instance_placement(
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

fn write_ginstance_transform(
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

fn write_parameters_json(
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

fn write_parameter_value_json(
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

fn json_escape(value: &str) -> String {
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

fn bodies(
    path: &Path,
    class_name: &str,
    count: usize,
    body_bytes: usize,
    tag: Option<u32>,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "Formats/Latest is required here")
    })?;
    let class_index = schema
        .class_by_name(class_name)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "schema class not found: {}",
                    escape_terminal_text(class_name)
                ),
            )
        })?
        .index;
    let partition_paths = partition_paths(&container);
    let mut printed = 0_usize;

    println!(
        "Bodies of class {class_index} {}:",
        escape_terminal_text(class_name)
    );
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, format_tag, layout, walk, payload| {
            if printed >= count || tag.is_some_and(|wanted| wanted != format_tag) {
                return;
            }
            for record in &walk.records {
                if printed >= count {
                    return;
                }
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != class_index {
                    continue;
                }
                let start = record.body_offset();
                let end = record.end().min(start + body_bytes).min(payload.len());
                let Some(slice) = payload.get(start..end) else {
                    continue;
                };
                printed += 1;
                println!(
                    "  id={}\ttag={format_tag}\tlen={}\tcompanion={}\tbody={}",
                    header.id,
                    header.body_bytes,
                    header.companion,
                    hex(slice)
                );
            }
        },
    )?;
    println!();
    println!("Records printed: {printed}");
    Ok(())
}

fn records(
    path: &Path,
    partition: Option<&str>,
    max_member_bytes: u64,
    dump: usize,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let element_ids = elem_table_ids(&container)?;
    let schema = read_schema(&container)?;
    let partition_paths = match partition {
        Some(name) => vec![name.to_owned()],
        None => partition_paths(&container),
    };
    let mut statistics = RecordStatistics {
        body_min: u64::MAX,
        ..RecordStatistics::default()
    };
    let mut dumped = 0_usize;

    println!("Record headers:");
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |partition_path, member, format_tag, layout, walk, payload| {
            observe_records(
                payload,
                walk,
                layout,
                format_tag,
                element_ids.as_ref(),
                schema.as_ref(),
                &mut statistics,
            );
            if dumped < dump {
                dumped += 1;
                dump_record_headers(partition_path, member.index, payload, walk);
            }
        },
    )?;
    print_record_statistics(&statistics, element_ids.as_ref(), schema.as_ref());
    Ok(())
}

fn observe_records(
    payload: &[u8],
    walk: &MemberWalk,
    layout: RecordLayout,
    format_tag: u32,
    element_ids: Option<&BTreeSet<u32>>,
    schema: Option<&Schema>,
    statistics: &mut RecordStatistics,
) {
    let mut previous_lead: Option<u32> = None;
    let mut ascending = true;
    for record in &walk.records {
        let Some(header) = payload.get(record.offset..record.offset + record.header_bytes) else {
            continue;
        };
        statistics.records += 1;
        *statistics.tag_records.entry(format_tag).or_default() += 1;
        match layout {
            RecordLayout::Narrow => statistics.narrow_records += 1,
            RecordLayout::Wide => statistics.wide_records += 1,
        }

        let lead = read_u32(&header[..4]);
        statistics.lead_values.insert(lead);
        if element_ids.is_some_and(|ids| ids.contains(&lead)) {
            statistics.lead_in_elem_table += 1;
            statistics.lead_hits.insert(lead);
        }
        if previous_lead.is_some_and(|previous| previous >= lead) {
            ascending = false;
        }
        previous_lead = Some(lead);

        // The trailing word is [class index:u16][unknown:u16] in every format.
        let tail_offset = match layout {
            RecordLayout::Narrow => 8,
            RecordLayout::Wide => 12,
        };
        let class_index = u16::from_le_bytes([header[tail_offset], header[tail_offset + 1]]);
        let companion = u16::from_le_bytes([header[tail_offset + 2], header[tail_offset + 3]]);
        if schema.is_some_and(|schema| schema.class_by_index(class_index).is_some()) {
            *statistics.class_counts.entry(class_index).or_default() += 1;
            *statistics
                .tag_class_counts
                .entry((format_tag, class_index))
                .or_default() += 1;
            *statistics.tag_class_hits.entry(format_tag).or_default() += 1;
        }
        *statistics.companion_words.entry(companion).or_default() += 1;
        if layout == RecordLayout::Wide {
            *statistics
                .wide_second_words
                .entry(read_u32(&header[4..8]))
                .or_default() += 1;
        }

        let body = record.body_bytes as u64;
        statistics.body_min = statistics.body_min.min(body);
        statistics.body_max = statistics.body_max.max(body);
        statistics.empty_bodies += u64::from(body == 0);
    }

    if !walk.records.is_empty() {
        statistics.checked_members += 1;
        statistics.ascending_members += usize::from(ascending);
    }
}

fn dump_record_headers(
    partition_path: &str,
    member_index: usize,
    payload: &[u8],
    walk: &MemberWalk,
) {
    for record in walk.records.iter().take(4) {
        let Some(header) = payload.get(record.offset..record.offset + record.header_bytes) else {
            continue;
        };
        println!(
            "  {}\tmember={}\toffset={}\theader={}\tbody={}",
            escape_terminal_text(partition_path),
            member_index,
            record.offset,
            hex(header),
            record.body_bytes
        );
    }
}

fn print_record_statistics(
    statistics: &RecordStatistics,
    element_ids: Option<&BTreeSet<u32>>,
    schema: Option<&Schema>,
) {
    println!();
    println!("Record summary:");
    println!("Records: {}", statistics.records);
    println!(
        "Narrow (12-byte) / wide (16-byte) headers: {} / {}",
        statistics.narrow_records, statistics.wide_records
    );
    println!(
        "Body bytes: min {} max {} (empty bodies: {})",
        if statistics.body_min == u64::MAX {
            0
        } else {
            statistics.body_min
        },
        statistics.body_max,
        statistics.empty_bodies
    );
    println!(
        "Members whose record lead values ascend: {} of {} ({})",
        statistics.ascending_members,
        statistics.checked_members,
        percentage(statistics.ascending_members, statistics.checked_members)
    );

    println!();
    println!("Lead u32 (+0):");
    println!("Distinct values: {}", statistics.lead_values.len());
    match element_ids {
        Some(ids) => {
            println!(
                "Records whose lead is a candidate ID: {} ({})",
                statistics.lead_in_elem_table,
                percentage(
                    usize::try_from(statistics.lead_in_elem_table).unwrap_or(usize::MAX),
                    usize::try_from(statistics.records).unwrap_or(usize::MAX)
                )
            );
            println!(
                "Distinct leads that are candidate IDs: {} ({} of the table)",
                statistics.lead_hits.len(),
                percentage(statistics.lead_hits.len(), ids.len())
            );
        }
        None => println!("Global/ElemTable not present; no cross-check"),
    }

    let Some(schema) = schema else {
        return;
    };
    println!();
    println!("Class index (u16 at +8 narrow / +12 wide):");
    for (tag, records) in &statistics.tag_records {
        let total = usize::try_from(*records).unwrap_or(usize::MAX);
        let hits = statistics.tag_class_hits.get(tag).copied().unwrap_or(0);
        println!(
            "Format tag {tag}: {records} records, resolved {hits} ({})",
            percentage(usize::try_from(hits).unwrap_or(usize::MAX), total)
        );
        let mut per_tag = statistics
            .tag_class_counts
            .iter()
            .filter(|((entry_tag, _), _)| entry_tag == tag)
            .map(|((_, index), count)| (*index, *count))
            .collect::<Vec<_>>();
        per_tag.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        for (index, count) in per_tag.into_iter().take(6) {
            print_class_row(schema, index, count);
        }
    }

    println!();
    println!(
        "Classes across every record: {}",
        statistics.class_counts.len()
    );
    for (index, count) in top_counts(&statistics.class_counts, 20) {
        print_class_row(schema, index, count);
    }

    println!();
    println!("Companion u16 beside the class index:");
    println!("Distinct values: {}", statistics.companion_words.len());
    for (value, count) in top_counts(&statistics.companion_words, HISTOGRAM_ROWS) {
        println!("  value={value}\tcount={count}");
    }

    println!();
    println!("Wide header second word (+4):");
    println!("Distinct values: {}", statistics.wide_second_words.len());
    for (value, count) in top_counts(&statistics.wide_second_words, HISTOGRAM_ROWS) {
        println!("  value={value}\tcount={count}");
    }
}

fn print_class_row(schema: &Schema, index: u16, count: u64) {
    let name = schema
        .class_by_index(index)
        .map_or("?", |class| class.name.as_str());
    println!("  {index}\t{}\tcount={count}", escape_terminal_text(name));
}

fn dump_member(
    path: &Path,
    partition: &str,
    logical_offset: u64,
    output: Option<&Path>,
    max_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let payload = container.decode_partition_member(partition, logical_offset, max_bytes)?;
    if let Some(output) = output {
        let mut writer = File::create(output)?;
        writer.write_all(&payload)?;
        writer.flush()?;
    } else {
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        writer.write_all(&payload)?;
        writer.flush()?;
    }
    Ok(())
}

/// Element-record inventory, counted over the whole container.
#[derive(Debug, Default)]
struct ObjectInventory {
    records: u64,
    resolved_classes: u64,
    identifiers: BTreeSet<u32>,
    identifiers_in_elem_table: BTreeSet<u32>,
    /// Classes taken from format-tag 102 records, which carry the element type.
    element_classes: BTreeMap<u16, u64>,
    /// Category codes read from `ElementHeader` bodies.
    categories: BTreeMap<i32, u64>,
    headers_with_a_category: u64,
    headers_with_a_family: u64,
    element_headers: u64,
    element_bodies: u64,
    anchored_by_pointer_block: u64,
    anchored_by_search: u64,
    with_a_level: u64,
}

fn inspect(path: &Path, streams_only: bool, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    println!("File: {}", path.display());
    println!("Container: valid CFB/OLE");
    println!("Stream inventory:");
    for name in KNOWN_STREAMS {
        match container.stream(name) {
            Some(stream) => println!("- {name}: present ({} bytes)", stream.len()),
            None => println!("- {name}: missing"),
        }
    }
    println!("- Partitions/*: {} stream(s)", container.partition_count());
    if streams_only {
        return Ok(());
    }

    let schema = read_schema(&container)?;
    let element_ids = elem_table_ids(&container)?;
    let partition_paths = partition_paths(&container);
    let header_class_index = schema
        .as_ref()
        .and_then(|schema| schema.class_by_name(ELEMENT_HEADER_CLASS))
        .map(|class| class.index);
    let mut inventory = ObjectInventory::default();

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, format_tag, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                inventory.records += 1;
                inventory.identifiers.insert(header.id);
                if element_ids
                    .as_ref()
                    .is_some_and(|ids| ids.contains(&header.id))
                {
                    inventory.identifiers_in_elem_table.insert(header.id);
                }
                let resolved = schema
                    .as_ref()
                    .is_some_and(|schema| schema.class_by_index(header.class_index).is_some());
                inventory.resolved_classes += u64::from(resolved);
                if resolved && format_tag == ELEMENT_CLASS_FORMAT_TAG {
                    *inventory
                        .element_classes
                        .entry(header.class_index)
                        .or_default() += 1;
                }
                if format_tag == ELEMENT_CLASS_FORMAT_TAG {
                    let body = record.body_in(payload);
                    if let Some(fields) = ElementFields::parse(body, header.id) {
                        inventory.element_bodies += 1;
                        match fields.anchor {
                            ElementAnchor::PointerBlock => {
                                inventory.anchored_by_pointer_block += 1;
                            }
                            ElementAnchor::IdentifierSearch => inventory.anchored_by_search += 1,
                        }
                        inventory.with_a_level += u64::from(fields.assoc_level_id.is_some());
                    }
                }
                if Some(header.class_index) == header_class_index {
                    inventory.element_headers += 1;
                    if let Some(fields) = ElementHeaderFields::parse(record.body_in(payload)) {
                        if let Some(category) = fields.category {
                            *inventory.categories.entry(category).or_default() += 1;
                            inventory.headers_with_a_category += 1;
                        }
                        inventory.headers_with_a_family += u64::from(fields.family_id.is_some());
                    }
                }
            }
        },
    )?;

    print_object_inventory(&inventory, element_ids.as_ref(), schema.as_ref());
    Ok(())
}

fn print_object_inventory(
    inventory: &ObjectInventory,
    element_ids: Option<&BTreeSet<u32>>,
    schema: Option<&Schema>,
) {
    let records = usize::try_from(inventory.records).unwrap_or(usize::MAX);
    println!();
    println!("Objects discovered: {}", inventory.identifiers.len());
    println!("Records: {}", inventory.records);
    println!(
        "Records with a resolved class: {} ({})",
        inventory.resolved_classes,
        percentage(
            usize::try_from(inventory.resolved_classes).unwrap_or(usize::MAX),
            records
        )
    );
    match element_ids {
        Some(ids) => println!(
            "Identifiers also in Global/ElemTable: {} ({} of the table)",
            inventory.identifiers_in_elem_table.len(),
            percentage(inventory.identifiers_in_elem_table.len(), ids.len())
        ),
        None => println!("Global/ElemTable: missing; identifiers not cross-checked"),
    }

    let Some(schema) = schema else {
        println!("Formats/Latest: missing; classes not resolved");
        return;
    };
    println!();
    println!(
        "Element classes (format tag {ELEMENT_CLASS_FORMAT_TAG}): {}",
        inventory.element_classes.len()
    );
    for (index, count) in top_counts(&inventory.element_classes, 20) {
        print_class_row(schema, index, count);
    }

    if inventory.element_bodies > 0 {
        let bodies = usize::try_from(inventory.element_bodies).unwrap_or(usize::MAX);
        println!();
        println!("Element bodies read: {}", inventory.element_bodies);
        println!(
            "Anchored by the pointer block: {} ({}); by identifier search: {}",
            inventory.anchored_by_pointer_block,
            percentage(
                usize::try_from(inventory.anchored_by_pointer_block).unwrap_or(usize::MAX),
                bodies
            ),
            inventory.anchored_by_search
        );
        println!(
            "With a level reference: {} ({})",
            inventory.with_a_level,
            percentage(
                usize::try_from(inventory.with_a_level).unwrap_or(usize::MAX),
                bodies
            )
        );
    }
    if inventory.element_headers > 0 {
        let headers = usize::try_from(inventory.element_headers).unwrap_or(usize::MAX);
        println!();
        println!(
            "{ELEMENT_HEADER_CLASS} records: {}",
            inventory.element_headers
        );
        println!(
            "With a category: {} ({}); with a family reference: {} ({})",
            inventory.headers_with_a_category,
            percentage(
                usize::try_from(inventory.headers_with_a_category).unwrap_or(usize::MAX),
                headers
            ),
            inventory.headers_with_a_family,
            percentage(
                usize::try_from(inventory.headers_with_a_family).unwrap_or(usize::MAX),
                headers
            )
        );
        println!("Distinct category codes: {}", inventory.categories.len());
        for (code, count) in top_counts(&inventory.categories, 12) {
            println!("  category={code}\tcount={count}");
        }
    }
}

#[cfg(test)]
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

    /// The reading `rebuild_rings` encodes: `Edge.identifiers` is
    /// `[pFace0, pFace1, next0, next1, ...]`, so the ring around a face
    /// continues at `next[side]` and ends where the chain leaves the edges.
    #[test]
    fn walks_the_edges_of_a_face_into_one_ring() {
        // A triangle on face 1, its far side on faces 2, 3 and 4, ending on
        // loop 99. Edge 12 also carries a link for face 2 that goes nowhere,
        // which is the shape of every ring no loop of a record ends.
        let links = BTreeMap::from([
            (10_u32, [1_u32, 2, 11, 98]),
            (11, [1, 3, 12, 97]),
            (12, [1, 4, 99, 96]),
        ]);
        let edges = [(10_u32, 0_usize), (11, 0), (12, 0)];
        assert_eq!(
            rebuild_rings(1, &edges, &links),
            Ok(vec![(10, 99, 3)]),
            "one ring of three edges, ending on the loop"
        );
        // Face 2 is named by one edge whose link names no edge of this record:
        // a fragment, not a boundary.
        assert_eq!(rebuild_rings(2, &[(10, 1)], &links), Ok(vec![(10, 98, 1)]));
        assert_eq!(
            rebuild_rings(5, &[], &links),
            Err("no edge names the face".to_owned())
        );
    }
}
