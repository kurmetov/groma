#![forbid(unsafe_code)]

use std::{error::Error, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use rvt_container::PartitionReadOptions;
// The whole semantic reconstruction moved into `rvt-import`. Glob-imported
// because the probe commands below read the same intermediate the pipeline
// builds, and naming each item here would be a second list to keep in step.

/// The furthest a triangle may sit from the surface it approximates, in
/// millimetres, unless a caller says otherwise.
///
/// Held once because two commands need the same answer: `export-scene` takes
/// it as a flag, and `export-ifc` has no flag for it but still drives a
/// reader that tessellates a curve to build its model.
pub(crate) const DEFAULT_CHORD_TOLERANCE_MM: f64 = 4.0;
/// Leading bytes summarized per envelope when looking for a record header.
pub(crate) const LEADING_PATTERN_BYTES: usize = 8;
/// Rows printed for each record-boundary histogram.
pub(crate) const HISTOGRAM_ROWS: usize = 8;
/// Strides tested when checking whether a marker is one element of an
/// ascending little-endian `u32` sequence rather than a record boundary.
pub(crate) const SEQUENCE_STRIDES: [usize; 4] = [4, 8, 12, 16];
/// Largest object identifier accepted when scanning a `GElement` body for
/// nested node references. Top-level identifiers stay far below this.
pub(crate) const NESTED_NODE_IDENTIFIER_LIMIT: u32 = 4_096;
/// Largest decoded member observed in the corpus; a record that runs past a
/// member of exactly this size is the continuation candidate.
pub(crate) const MEMBER_PAGE_LIMIT_BYTES: u64 = 128 * 1024;
/// `UUIDv5` namespace used only to turn a canonical source path into a model
/// namespace. Users can supply a persistent namespace explicitly when a model
/// may move between paths.
pub(crate) const SOURCE_PATH_NAMESPACE: [u8; 16] = [
    0x76, 0x26, 0xfd, 0xf2, 0xc2, 0xad, 0x51, 0xd0, 0xb9, 0x1f, 0xc6, 0xcc, 0x07, 0x36, 0x0c, 0x62,
];

pub(crate) const KNOWN_STREAMS: [&str; 4] = [
    "BasicFileInfo",
    "Formats/Latest",
    "Global/ElemTable",
    "Global/Latest",
];

mod export;
mod inspect;
mod json;
mod probe;
mod source;

#[allow(clippy::wildcard_imports)] // The modules of one binary, split for reading.
use crate::{export::*, inspect::*, probe::*};

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
