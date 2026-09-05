#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::Write as _,
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use bim_core::{
    BimBoundingBox, BimBrep, BimBrepArc, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepSurface,
    BimCategory, BimElement, BimElementId, BimElementType, BimExternalId, BimGeometry, BimLevel,
    BimLineSegment, BimModel, BimNumber, BimPlacement, BimPoint3, BimProperty, BimPropertyValue,
    BimSource, BimSweptDisk, BimUnit,
};
use clap::{Parser, Subcommand};
use ifc_export::{MetadataOptions, element_type_for_source, metadata_ifc, uuid_v5};
use revit_catalog::Catalog;
use rvt_container::{
    BasicFileInfo, DEFAULT_DECODE_LIMIT, MarkerEnvelope, MarkerEnvelopeOptions,
    PartitionReadOptions, REVIT_STORED_PAGE_BYTES, RvtContainer, StreamFraming,
    decode_known_framing, strip_revit_page_checksums,
};
use rvt_model::{
    ELEMENT_TAIL_BYTES, ElemTable, ElementAnchor, ElementFields, ElementHeaderFields,
    FamilyInstancePlacementFields, FittingCenterLineFields, GElementBounds, GElementGraphFields,
    GInstanceTransformFields, LevelFields, MemberWalk, ParameterSetClassIndexes, ParameterSets,
    ParameterSpec, ParameterValue, PipeLineGeometryFields, RECORD_LENGTH_TRAILER_BYTES,
    RecordFraming, RecordHeader, RecordLayout, RecordString,
};
use rvt_schema::{Schema, TypeReference};

/// Leading bytes summarized per envelope when looking for a record header.
const LEADING_PATTERN_BYTES: usize = 8;
/// Rows printed for each record-boundary histogram.
const HISTOGRAM_ROWS: usize = 8;
/// Strides tested when checking whether a marker is one element of an
/// ascending little-endian `u32` sequence rather than a record boundary.
const SEQUENCE_STRIDES: [usize; 4] = [4, 8, 12, 16];
/// Share of a class's records that must place their first readable string at
/// the same offset before that offset is treated as the class's name field.
const NAME_OFFSET_AGREEMENT: u64 = 90;
/// Largest object identifier accepted when scanning a `GElement` body for
/// nested node references. Top-level identifiers stay far below this.
const NESTED_NODE_IDENTIFIER_LIMIT: u32 = 4_096;
/// Schema class whose record body starts with the element's identifier block.
const ELEMENT_HEADER_CLASS: &str = "ElementHeader";
/// Descriptor format tag whose records carry the element's own class; the
/// other tags carry a header record and a serialized/geometry record.
const ELEMENT_CLASS_FORMAT_TAG: u32 = 102;
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
        /// Maximum decoded bytes accepted from one member.
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
    /// Export an IFC4 spatial tree, typed elements, and verified geometry.
    ExportIfc {
        file: PathBuf,
        /// Write to this path instead of replacing the `.rvt` extension with `.ifc`.
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
        /// Maximum decoded bytes accepted from one member.
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
        Command::Names {
            file,
            classes,
            max_member_bytes,
        } => names(&file, classes, max_member_bytes),
        Command::ExportJson {
            file,
            output,
            limit,
            max_member_bytes,
        } => export_json(&file, output.as_deref(), limit, max_member_bytes),
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
        Command::GeometryGraphProbe {
            file,
            rows,
            max_member_bytes,
        } => geometry_graph_probe(&file, rows, max_member_bytes),
        Command::ExportIfc {
            file,
            output,
            model_namespace,
            include_unplaced,
            limit,
            max_member_bytes,
        } => export_ifc(
            &file,
            output.as_deref(),
            model_namespace.as_deref(),
            include_unplaced,
            limit,
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

/// Paths of the top-level `Partitions/*` streams, in container order.
fn partition_paths(container: &RvtContainer) -> Vec<String> {
    container
        .streams()
        .iter()
        .filter(|stream| {
            stream
                .path()
                .strip_prefix("Partitions/")
                .is_some_and(|name| !name.is_empty() && !name.contains('/'))
        })
        .map(|stream| stream.path().to_owned())
        .collect()
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

fn decode_schema_stream(
    raw: &[u8],
) -> Result<(rvt_container::DecodedStream, Schema, bool), Box<dyn Error>> {
    match decode_schema_attempt(raw) {
        Ok((decoded, schema)) => Ok((decoded, schema, false)),
        Err(raw_error) if raw.len() >= REVIT_STORED_PAGE_BYTES => {
            let stripped = strip_revit_page_checksums(raw);
            match decode_schema_attempt(&stripped) {
                Ok((decoded, schema)) => Ok((decoded, schema, true)),
                Err(stripped_error) => Err(schema_retry_error(&raw_error, &stripped_error).into()),
            }
        }
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error).into()),
    }
}

fn decode_schema_attempt(stored: &[u8]) -> Result<(rvt_container::DecodedStream, Schema), String> {
    let decoded =
        decode_known_framing(stored, DEFAULT_DECODE_LIMIT).map_err(|error| error.to_string())?;
    let schema = Schema::parse(&decoded.payload).map_err(|error| error.to_string())?;
    Ok((decoded, schema))
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

fn decode_elem_table_stream(
    raw: &[u8],
) -> Result<(rvt_container::DecodedStream, ElemTable, bool), Box<dyn Error>> {
    let has_complete_pages = raw.len() >= REVIT_STORED_PAGE_BYTES;
    let prepared = if has_complete_pages {
        strip_revit_page_checksums(raw)
    } else {
        raw.to_vec()
    };
    let (decoded, table) = decode_elem_table_attempt(&prepared)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((decoded, table, has_complete_pages))
}

fn decode_elem_table_attempt(
    stored: &[u8],
) -> Result<(rvt_container::DecodedStream, ElemTable), String> {
    let decoded =
        decode_known_framing(stored, DEFAULT_DECODE_LIMIT).map_err(|error| error.to_string())?;
    let table = ElemTable::parse(&decoded.payload).map_err(|error| error.to_string())?;
    Ok((decoded, table))
}

/// Candidate element identifiers from `Global/ElemTable`, or `None` when the
/// container has no such stream.
fn elem_table_ids(container: &RvtContainer) -> Result<Option<BTreeSet<u32>>, Box<dyn Error>> {
    if container.stream("Global/ElemTable").is_none() {
        return Ok(None);
    }
    let raw = container.read_stream_with_limit("Global/ElemTable", DEFAULT_DECODE_LIMIT as u64)?;
    let (_, table, _) = decode_elem_table_stream(&raw)?;
    Ok(Some(
        table
            .records
            .iter()
            .flat_map(|record| [record.id_primary, record.id_secondary])
            .filter(|id| *id != 0 && *id != u32::MAX)
            .collect(),
    ))
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

/// Visit every member that carries a usable descriptor, in partition order,
/// threading the continuation carry between them.
fn for_each_member(
    container: &RvtContainer,
    partition_paths: &[String],
    max_member_bytes: u64,
    mut visit: impl FnMut(&str, &rvt_container::PartitionMember, u32, RecordLayout, &MemberWalk, &[u8]),
) -> Result<(), Box<dyn Error>> {
    for partition_path in partition_paths {
        let report =
            container.inspect_partition(partition_path, PartitionReadOptions::default())?;
        let mut carry = 0_u64;
        for member in &report.members {
            let Some(descriptor) = member.descriptor else {
                carry = 0;
                continue;
            };
            let Some(layout) = RecordLayout::from_format_tag(descriptor.format_tag) else {
                carry = 0;
                continue;
            };
            let payload = container.decode_partition_member(
                partition_path,
                member.logical_offset,
                max_member_bytes,
            )?;
            let leading_carry = usize::try_from(carry).unwrap_or(usize::MAX);
            let Ok(walk) = MemberWalk::parse(&payload, layout, leading_carry) else {
                carry = 0;
                continue;
            };
            visit(
                partition_path,
                member,
                descriptor.format_tag,
                layout,
                &walk,
                &payload,
            );
            carry = walk.trailing_deficit;
        }
    }
    Ok(())
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

/// Properties naming the type an element is an instance of, tried in order.
///
/// There is no one such property: `Element` declares none, and a class names
/// its type under whatever its own chain calls it. `m_masterSymbolId` is a
/// `FamilyInstance` property and covers loadable families; a system family
/// carries its own - `RbsCurve.m_idType` for a pipe or duct. Each is read by
/// [`rvt_model::record_declared_id`] out of the record's own header, so a class
/// that declares none of them simply has no type reference and a class that
/// declares one cannot pick up another's.
///
/// Measured, not guessed, on SMALL. All 10 011 `FamilyInstance` records that
/// carry a value name an element that is a `FamilySymbol` - 10 011 of 10 011,
/// where a misread field would land on one class or another at random - and
/// they name only 226 distinct ones, the type-to-instance ratio a real type
/// reference has. Two independent checks agree with it: on the 70 instances
/// whose symbol was separately verified by matching the instance's bounds
/// against the symbol's transformed box, the declared property names that same
/// symbol in 70 of 70; and every instance shares a family with the type it
/// names, 250 of 250 where both carry one.
///
/// Reading it replaces inferring the type from geometry, which covered only the
/// 198 instances whose bounds could be matched. The bounds-verified symbol
/// stays as the fallback, and where both are present they agree.
///
/// It is *not* the same thing as `GInstance`'s symbol, which is the symbol
/// whose geometry a record instantiates: across all 7 055 records carrying one
/// the two agree in only 7.4% of cases, because a record's geometry is usually
/// instantiated from a nested symbol rather than from the instance's own type.
/// Only on the bounds-verified subset, where the `GInstance` symbol is proven
/// to be this instance's, do they agree everywhere.
///
/// `RbsCurve.m_idType` was measured the same way; see the working notes for its
/// numbers.
const TYPE_ELEMENT_ID_PROPERTIES: &[&str] = &["m_masterSymbolId", "m_idType"];

/// One element as it is emitted to JSON.
#[derive(Debug, Default)]
struct ExportedElement {
    class_index: Option<u16>,
    category: Option<i32>,
    /// Where [`ExportedElement::category`] came from: `"declared"` when the
    /// element's own record carried it, `"symbol"` when it was taken from the
    /// symbol its bounds verified it against. See [`attach_symbol_bounds`].
    category_source: Option<&'static str>,
    header_family_id: Option<i32>,
    level_id: Option<i32>,
    family_id: Option<i32>,
    owner_view_id: Option<i32>,
    created_phase_id: Option<i32>,
    design_option_id: Option<i32>,
    /// `Plane.m_origin[2]` for a `Level`, in Revit internal feet.
    elevation_feet: Option<f64>,
    /// First readable string in the body, with how it was located.
    name: Option<(String, &'static str)>,
    parameters: Vec<rvt_model::Parameter>,
    /// The element this one is an instance of, from its own declarations.
    type_element_id: Option<i32>,
    /// Parameters read from the record of this element's type. See
    /// `inherit_symbol_parameters`.
    type_parameters: Vec<rvt_model::Parameter>,
    /// Forge spec carried by this element when it defines a parameter.
    parameter_spec: Option<String>,
    pipe_line_candidate: Option<PipeLineGeometryFields>,
    fitting_center_line_candidate: Option<FittingCenterLineFields>,
    fitting_axis_candidate: Option<FittingCenterLineFields>,
    family_instance_placement_candidates: Vec<FamilyInstancePlacementFields>,
    family_instance_placement: Option<FamilyInstancePlacementFields>,
    ginstance_transform: Option<GInstanceTransformFields>,
    geometry_graph: Option<GElementGraphFields>,
    geometry_bounds: Option<GElementBounds>,
    placement_bounds: Option<GElementBounds>,
    verified_symbol_bounds: Option<VerifiedSymbolBounds>,
    /// The boundary representation decoded from this id's own `GElement`
    /// record, in its own local frame and Revit internal feet. Populated for
    /// any id that carries one - typically a `FamilySymbol` - and looked up
    /// by an instance through its verified symbol id, not copied per instance.
    brep: Option<rvt_model::SymbolBrep>,
    /// `m_moribund` from the `Element` tail: the element is marked deleted.
    moribund: bool,
    locked: bool,
    source: Option<(usize, usize, usize)>,
    record_count: usize,
}

impl ExportedElement {
    /// The element this one is an instance of: its declared type reference,
    /// or - for a record whose declarations did not yield one - the symbol its
    /// bounds were verified against. See [`TYPE_ELEMENT_ID_PROPERTIES`].
    fn type_element_reference(&self) -> Option<u32> {
        self.type_element_id
            .and_then(|id| u32::try_from(id).ok())
            .or_else(|| {
                self.verified_symbol_bounds
                    .map(|symbol| symbol.symbol_element_id)
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct VerifiedSymbolBounds {
    symbol_element_id: u32,
    bounds: GElementBounds,
}

/// Per-class string calibration plus the identifiers of parameter elements.
type ExportContext = (BTreeMap<u16, NameCalibration>, BTreeSet<i32>);

/// Where one class keeps its first readable string, and how consistently.
#[derive(Debug, Default)]
struct NameCalibration {
    /// Offsets relative to the end of the `Element` tail, and how often each
    /// was the first readable string.
    offsets: BTreeMap<usize, u64>,
    bodies: u64,
    samples: Vec<String>,
}

impl NameCalibration {
    /// The offset the class agrees on, if the agreement is strong enough.
    fn settled_offset(&self) -> Option<usize> {
        let (offset, count) = self.offsets.iter().max_by_key(|(_, count)| **count)?;
        (count * 100 >= self.bodies * NAME_OFFSET_AGREEMENT).then_some(*offset)
    }

    fn agreement(&self) -> u64 {
        self.offsets
            .values()
            .max()
            .map_or(0, |count| count * 100 / self.bodies.max(1))
    }
}

/// Collect, in one pass, where each class keeps its string and which elements
/// are parameter definitions.
fn calibrate_names(
    container: &RvtContainer,
    schema: Option<&Schema>,
    partition_paths: &[String],
    max_member_bytes: u64,
) -> Result<ExportContext, Box<dyn Error>> {
    let parameter_classes = parameter_class_indexes(schema);
    let mut parameter_ids = BTreeSet::new();
    let mut calibrations: BTreeMap<u16, NameCalibration> = BTreeMap::new();
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
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                if parameter_classes.contains(&header.class_index) {
                    if let Ok(id) = i32::try_from(header.id) {
                        parameter_ids.insert(id);
                    }
                }
                let Some(fields) = ElementFields::parse(body, header.id) else {
                    continue;
                };
                let tail_end = fields.id_offset + 4 + ELEMENT_TAIL_BYTES;
                let entry = calibrations.entry(header.class_index).or_default();
                entry.bodies += 1;
                if let Some(found) = RecordString::scan_from(body, tail_end) {
                    *entry.offsets.entry(found.offset - tail_end).or_default() += 1;
                    if entry.samples.len() < 3 {
                        entry.samples.push(found.value);
                    }
                }
            }
        },
    )?;
    Ok((calibrations, parameter_ids))
}

/// Schema classes whose elements define parameters.
fn parameter_class_indexes(schema: Option<&Schema>) -> BTreeSet<u16> {
    schema
        .map(|schema| {
            schema
                .classes
                .iter()
                .filter(|class| class.name.starts_with("Param"))
                .map(|class| class.index)
                .collect()
        })
        .unwrap_or_default()
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
    let classes = (|| {
        Some(rvt_model::BrepClassIndexes {
            face: schema_class_index(schema.as_ref(), "Face")?,
            edge_loop: schema_class_index(schema.as_ref(), "EdgeLoop")?,
            edge: schema_class_index(schema.as_ref(), "Edge")?,
            plane: schema_class_index(schema.as_ref(), "Plane")?,
            cyl_surf: schema_class_index(schema.as_ref(), "CylSurf")?,
        })
    })();
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
    let mut why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut exact_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut edge_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut exact_edge_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut gaps: Vec<f64> = Vec::new();
    let mut gaps_with_a_cylinder = 0_u64;
    let mut gaps_in_exact_records = 0_u64;
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
                let assembled = rvt_model::assemble_symbol_brep(&objects, &classes);
                if assembled.is_empty() && assembled.excluded_faces.is_empty() {
                    continue;
                }
                records += 1;
                exact_records += u64::from(exact);
                complete += u64::from(assembled.excluded_faces.is_empty());
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
                    }
                }
                for exclusion in &assembled.excluded_faces {
                    *why.entry(exclusion.reason).or_default() += 1;
                    if exact {
                        *exact_why.entry(exclusion.reason).or_default() += 1;
                    }
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
    println!("Faces resolved: {faces}");
    println!("  loops: {loops}, edges: {edges}, of which arcs: {arcs}");
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
        // How far apart the two faces put a shared endpoint separates a
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
        println!("Cross-face endpoint disagreements: {total}");
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

#[allow(clippy::too_many_lines)] // One streaming pass keeps large RVT payloads out of memory.
fn recover_elements(
    path: &Path,
    max_member_bytes: u64,
) -> Result<RecoveredElements, Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let release = read_basic_file_info(&container)?.and_then(|info| info.revit_version);
    let catalog = release.and_then(Catalog::for_release);
    let schema = read_schema(&container)?;
    let header_class_index = schema_class_index(schema.as_ref(), ELEMENT_HEADER_CLASS);
    let level_class_index = schema_class_index(schema.as_ref(), "Level");
    let plane_class_index = schema_class_index(schema.as_ref(), "Plane");
    let pipe_curve_class_index = schema_class_index(schema.as_ref(), "RbsPipeCurve");
    let family_instance_class_index = schema_class_index(schema.as_ref(), "FamilyInstance");
    let family_symbol_class_index = schema_class_index(schema.as_ref(), "FamilySymbol");
    let curve_driver_class_index = schema_class_index(schema.as_ref(), "RbsCurveDriver");
    let pipe_fitting_center_line_class_index =
        schema_class_index(schema.as_ref(), "PipeFittingCenterLine");
    let gline_class_index = schema_class_index(schema.as_ref(), "GLine");
    let ginstance_class_index = schema_class_index(schema.as_ref(), "GInstance");
    let gnode_class_index = schema_class_index(schema.as_ref(), "GNode");
    let geometry_element_class_index = schema_class_index(schema.as_ref(), "GElement");
    let parameter_set_classes = parameter_set_class_indexes(schema.as_ref());
    let brep_classes = (|| {
        Some(rvt_model::BrepClassIndexes {
            face: schema_class_index(schema.as_ref(), "Face")?,
            edge_loop: schema_class_index(schema.as_ref(), "EdgeLoop")?,
            edge: schema_class_index(schema.as_ref(), "Edge")?,
            plane: plane_class_index?,
            cyl_surf: schema_class_index(schema.as_ref(), "CylSurf")?,
        })
    })();
    let partition_paths = partition_paths(&container);
    let (calibrations, parameter_ids) = calibrate_names(
        &container,
        schema.as_ref(),
        &partition_paths,
        max_member_bytes,
    )?;
    let accept_parameter = |id: i32| parameter_ids.contains(&id);
    let accept_verified_parameter = |id: i32| {
        if id < 0 {
            catalog.is_some_and(|catalog| catalog.built_in_parameter(id).is_some())
        } else {
            parameter_ids.contains(&id)
        }
    };
    let mut elements: BTreeMap<u32, ExportedElement> = BTreeMap::new();

    for (partition_index, partition_path) in partition_paths.iter().enumerate() {
        for_each_member(
            &container,
            std::slice::from_ref(partition_path),
            max_member_bytes,
            |_, member, format_tag, layout, walk, payload| {
                for record in &walk.records {
                    let Some(header) = RecordHeader::parse(payload, record, layout) else {
                        continue;
                    };
                    let entry = elements.entry(header.id).or_default();
                    entry.record_count += 1;
                    let body = record.body_in(payload);

                    if Some(header.class_index) == geometry_element_class_index {
                        let exact_bounds = GElementBounds::parse(body);
                        let graph = gnode_class_index.and_then(|gnode_class_index| {
                            GElementGraphFields::parse(body, |class_index| {
                                schema.as_ref().is_some_and(|schema| {
                                    schema_class_is_a(schema, class_index, gnode_class_index)
                                })
                            })
                        });
                        let placement_bounds = exact_bounds
                            .or_else(|| graph.as_ref().map(|graph| graph.bounds))
                            .or_else(|| GElementBounds::parse_near_duplicate(body));
                        if let Some(bounds) = exact_bounds {
                            entry.geometry_bounds = Some(bounds);
                        }
                        entry.geometry_graph = graph;
                        if let Some(bounds) = placement_bounds {
                            entry.placement_bounds = Some(bounds);
                            if let Some(ginstance_class_index) = ginstance_class_index {
                                entry.ginstance_transform = GInstanceTransformFields::parse(
                                    body,
                                    ginstance_class_index,
                                    &bounds,
                                );
                            }
                        }
                        if let (Some(schema), Some(classes)) = (schema.as_ref(), &brep_classes) {
                            let (_walk, objects) =
                                rvt_model::walk_record_collecting(schema, header.class_index, body);
                            let brep = rvt_model::assemble_symbol_brep(&objects, classes);
                            if !brep.is_empty() {
                                entry.brep = Some(brep);
                            }
                        }
                    }

                    if Some(header.class_index) == header_class_index {
                        if let Some(fields) = ElementHeaderFields::parse(body) {
                            if entry.category.is_none() && fields.category.is_some() {
                                entry.category = fields.category;
                                entry.category_source = Some("declared");
                            }
                            entry.header_family_id = entry.header_family_id.or(fields.family_id);
                        }
                    } else if format_tag == ELEMENT_CLASS_FORMAT_TAG {
                        entry.class_index = Some(header.class_index);
                        entry.source = Some((partition_index, member.index, record.offset));
                        // The declarations name the four parameter sets
                        // outright, so they are read from the walk rather than
                        // searched for, and without waiting on the heuristic
                        // that locates the element's fixed tail.
                        if entry.parameters.is_empty() {
                            let declared = (|| {
                                let classes = parameter_set_classes?;
                                ParameterSets::from_record(
                                    schema.as_ref()?,
                                    header.class_index,
                                    body,
                                    classes,
                                )
                            })();
                            if let Some(found) = declared {
                                entry.parameters = found.parameters;
                            }
                        }
                        // The declarations name the property a record's name
                        // is the value of, so it is read rather than scanned
                        // for. The scan stays as the fallback for a record
                        // whose class declares no such property.
                        if entry.name.is_none() {
                            entry.name = schema
                                .as_ref()
                                .and_then(|schema| {
                                    rvt_model::record_name(schema, header.class_index, body)
                                })
                                .map(|name| (name, "declared"));
                        }
                        // The type an element is an instance of is a declared
                        // property, so it is read rather than inferred from
                        // geometry. See `TYPE_ELEMENT_ID_PROPERTIES`.
                        if entry.type_element_id.is_none() {
                            entry.type_element_id = schema.as_ref().and_then(|schema| {
                                TYPE_ELEMENT_ID_PROPERTIES.iter().find_map(|property| {
                                    rvt_model::record_declared_id(
                                        schema,
                                        header.class_index,
                                        body,
                                        property,
                                    )
                                })
                            });
                        }
                        if let Some(fields) = ElementFields::parse(body, header.id) {
                            let tail_end = fields.id_offset + 4 + ELEMENT_TAIL_BYTES;
                            if entry.name.is_none() {
                                entry.name = read_name(
                                    body,
                                    tail_end,
                                    calibrations
                                        .get(&header.class_index)
                                        .and_then(NameCalibration::settled_offset),
                                );
                            }
                            // The scan stays as the fallback for a record the
                            // walk cannot reach the sets in.
                            if entry.parameters.is_empty() {
                                let found = if let (Some(classes), Some(_)) =
                                    (parameter_set_classes, catalog)
                                {
                                    ParameterSets::scan_schema_bound(
                                        body,
                                        tail_end,
                                        fields.id_offset,
                                        classes,
                                        &accept_verified_parameter,
                                    )
                                } else {
                                    ParameterSets::scan(body, &accept_parameter)
                                };
                                if let Some(found) = found {
                                    entry.parameters = found.parameters;
                                }
                            }
                            if Some(header.class_index) == family_instance_class_index {
                                entry.family_instance_placement_candidates =
                                    FamilyInstancePlacementFields::candidates(body, tail_end);
                            }
                            entry.moribund |= fields.moribund;
                            entry.locked |= fields.locked;
                            entry.level_id = entry.level_id.or(fields.assoc_level_id);
                            entry.family_id = entry.family_id.or(fields.family_id);
                            entry.owner_view_id = entry.owner_view_id.or(fields.owner_view_id);
                            entry.created_phase_id =
                                entry.created_phase_id.or(fields.created_phase_id);
                            entry.design_option_id =
                                entry.design_option_id.or(fields.design_option_id);
                        }
                        if Some(header.class_index) == level_class_index {
                            if let Some(plane_index) = plane_class_index {
                                if let Some(fields) = LevelFields::parse(body, plane_index) {
                                    entry.elevation_feet = Some(fields.elevation_feet);
                                }
                            }
                        }
                        if Some(header.class_index) == pipe_curve_class_index {
                            if let Some(curve_driver_class_index) = curve_driver_class_index {
                                entry.pipe_line_candidate =
                                    PipeLineGeometryFields::parse(body, curve_driver_class_index);
                            }
                        }
                        if Some(header.class_index) == pipe_fitting_center_line_class_index {
                            if let Some(gline_class_index) = gline_class_index {
                                entry.fitting_center_line_candidate =
                                    FittingCenterLineFields::parse(body, gline_class_index);
                            }
                        }
                        if i32::try_from(header.id).is_ok_and(|id| parameter_ids.contains(&id)) {
                            entry.parameter_spec =
                                ParameterSpec::scan(body).map(|spec| spec.type_id);
                        }
                    }
                }
            },
        )?;
    }

    attach_symbol_bounds(&mut elements, family_symbol_class_index);
    attach_fitting_axes(&mut elements, catalog);
    verify_family_instance_placements(&mut elements);
    inherit_symbol_names(&mut elements);
    inherit_symbol_parameters(&mut elements);

    let (parameter_names, parameter_specs) = parameter_metadata(&elements);
    Ok(RecoveredElements {
        release,
        catalog,
        parameter_values_schema_bound: catalog.is_some() && parameter_set_classes.is_some(),
        schema,
        partition_paths,
        parameter_names,
        parameter_specs,
        elements,
    })
}

/// Give an instance the name of the symbol it was verified against.
///
/// An instance's own declarations carry no name property - in Revit its name is
/// its type's - so `FamilySymbol`'s declared `SymbolInfo.m_name` is the one to
/// use, reached through the instance's declared type reference (see
/// [`TYPE_ELEMENT_ID_PROPERTIES`]) or, failing that, the symbol its bounds were
/// verified against. It takes precedence over a scanned name, which for an
/// instance is whatever string the body happened to hold first, and stands
/// aside for a declared one.
fn inherit_symbol_names(elements: &mut BTreeMap<u32, ExportedElement>) {
    let symbol_names = elements
        .iter()
        .filter_map(|(id, element)| {
            let (name, source) = element.name.as_ref()?;
            (*source == "declared").then(|| (*id, name.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for element in elements.values_mut() {
        if element
            .name
            .as_ref()
            .is_some_and(|(_, source)| *source == "declared")
        {
            continue;
        }
        let Some(symbol) = element.type_element_reference() else {
            continue;
        };
        if let Some(name) = symbol_names.get(&symbol) {
            element.name = Some((name.clone(), "symbol"));
        }
    }
}

/// Give an instance the parameters stored on the symbol it was verified
/// against.
///
/// An element's own record carries only what was set on the instance - around
/// three values for a typical family instance in the corpus. The rest are
/// stored once on its type, and the symbol verified by bounds is the same link
/// `inherit_symbol_names` already trusts for the name.
///
/// They are kept in their own list rather than merged into the element's, so
/// that "this element carries this value" and "every element of this type
/// carries this value" stay distinguishable downstream. A parameter the
/// element already sets is not inherited: an instance value overrides its
/// type's, and no corpus body was found where both are written, so the rule
/// only guards against a duplicate rather than resolving a measured conflict.
fn inherit_symbol_parameters(elements: &mut BTreeMap<u32, ExportedElement>) {
    let symbol_parameters = elements
        .iter()
        .filter(|(_, element)| !element.parameters.is_empty())
        .map(|(id, element)| (*id, element.parameters.clone()))
        .collect::<BTreeMap<_, _>>();
    for element in elements.values_mut() {
        let Some(symbol) = element.type_element_reference() else {
            continue;
        };
        let Some(parameters) = symbol_parameters.get(&symbol) else {
            continue;
        };
        let own = element
            .parameters
            .iter()
            .map(|parameter| parameter.id)
            .collect::<BTreeSet<_>>();
        element.type_parameters = parameters
            .iter()
            .filter(|parameter| !own.contains(&parameter.id))
            .cloned()
            .collect();
    }
}

fn verify_family_instance_placements(elements: &mut BTreeMap<u32, ExportedElement>) {
    for element in elements.values_mut() {
        let Some(bounds) = element.placement_bounds else {
            continue;
        };
        let mut inside = element
            .family_instance_placement_candidates
            .iter()
            .copied()
            .filter(|candidate| bounds.contains_point(candidate.origin));
        let Some(candidate) = inside.next() else {
            continue;
        };
        if inside.next().is_none() {
            element.family_instance_placement = Some(candidate);
        }
    }
}

fn attach_symbol_bounds(
    elements: &mut BTreeMap<u32, ExportedElement>,
    family_symbol_class_index: Option<u16>,
) {
    let Some(family_symbol_class_index) = family_symbol_class_index else {
        return;
    };
    let symbols = elements
        .iter()
        .filter_map(|(id, element)| {
            if element.class_index != Some(family_symbol_class_index) {
                return None;
            }
            let bounds = element.geometry_graph.as_ref()?.bounds;
            // A symbol box that is flat on an axis carries no volume and cannot
            // become an `IfcBoundingBox`, so it is refused here rather than
            // counted and then silently dropped by the IFC writer.
            bounds
                .is_volumetric()
                .then_some((*id, (element.category, bounds)))
        })
        .collect::<BTreeMap<_, _>>();

    for element in elements.values_mut() {
        let Some(transform) = element.ginstance_transform else {
            continue;
        };
        let Some(symbol_element_id) = transform.symbol_element_id else {
            continue;
        };
        let Some((symbol_category, symbol_bounds)) = symbols.get(&symbol_element_id).copied()
        else {
            continue;
        };
        let Some(instance_bounds) = element.placement_bounds else {
            continue;
        };
        // The bounds cross-check is the verification: all six coordinates of
        // the instance's own independently decoded box must agree with the
        // symbol's box carried through the instance's rigid transform, to
        // within 1e-8 feet. Chance agreement is not a real possibility.
        if !instance_bounds.matches_transformed(&symbol_bounds, &transform) {
            continue;
        }
        // The categories are required not to *disagree*, rather than required
        // to be equal. Measured: among the links the cross-check accepts,
        // every one where both sides carry a category has the same category on
        // both - on BIG that is every accepted link without exception - so
        // demanding equality only ever rejected links where one side's
        // category was not recovered. That cost 2 491 / 1 983 / 199
        // geometrically verified links across the corpus and refused nothing
        // that was actually wrong. Kept as a disagreement guard because it is
        // free and would catch a mislinked symbol in a file unlike these; it
        // fires on nothing in this corpus.
        if let (Some(instance_category), Some(symbol_category)) =
            (element.category, symbol_category)
        {
            if instance_category != symbol_category {
                continue;
            }
        }
        element.verified_symbol_bounds = Some(VerifiedSymbolBounds {
            symbol_element_id,
            bounds: symbol_bounds,
        });
        // An instance whose own record did not yield a category takes its
        // symbol's. In Revit a family instance's category *is* its family's,
        // and the corpus confirms that reading rather than assuming it: among
        // the links this cross-check accepts, every one where both sides carry
        // a category carries the same one, with no exception on any of the
        // three files. Without this, 6 576 of SMALL's 7 014 placed family
        // instances have no category and the exporter drops them, bodies and
        // all, because an element with no category is not a candidate.
        //
        // Marked as inherited rather than silently merged: the JSON reports
        // `category_source`, so a consumer can tell a declared category from
        // one taken from the symbol.
        if element.category.is_none() {
            if let Some(symbol_category) = symbol_category {
                element.category = Some(symbol_category);
                element.category_source = Some("symbol");
            }
        }
    }
}

fn attach_fitting_axes(elements: &mut BTreeMap<u32, ExportedElement>, catalog: Option<Catalog>) {
    let mut by_owner: BTreeMap<u32, Vec<FittingCenterLineFields>> = BTreeMap::new();
    for element in elements.values() {
        let Some(line) = element.fitting_center_line_candidate else {
            continue;
        };
        if category_name(element, catalog) == Some("OST_PipeFittingCenterLine") {
            by_owner
                .entry(line.owner_element_id)
                .or_default()
                .push(line);
        }
    }
    for (owner_id, lines) in by_owner {
        if lines.len() != 1 {
            continue;
        }
        let Some(owner) = elements.get_mut(&owner_id) else {
            continue;
        };
        if category_name(owner, catalog) == Some("OST_PipeFitting")
            && owner
                .geometry_bounds
                .is_some_and(|bounds| bounds.contains_line_segment(lines[0].start, lines[0].end))
        {
            owner.fitting_axis_candidate = lines.first().copied();
        }
    }
}

fn category_name(element: &ExportedElement, catalog: Option<Catalog>) -> Option<&'static str> {
    catalog?
        .built_in_category(element.category?)
        .map(|category| category.enum_name)
}

fn export_json(
    path: &Path,
    output: Option<&Path>,
    limit: Option<usize>,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let recovered = recover_elements(path, max_member_bytes)?;
    let writer: Box<dyn Write> = match output {
        Some(output) => Box::new(File::create(output)?),
        None => Box::new(io::stdout().lock()),
    };
    let metadata = ExportMetadata {
        schema: recovered.schema.as_ref(),
        partition_paths: &recovered.partition_paths,
        parameter_names: &recovered.parameter_names,
        parameter_specs: &recovered.parameter_specs,
        catalog: recovered.catalog,
    };
    let written = write_exported_elements(writer, &recovered.elements, &metadata, limit)?;
    if output.is_some() {
        println!(
            "Elements written: {written} of {}",
            recovered.elements.len()
        );
    }
    Ok(())
}

struct RecoveredElements {
    release: Option<u16>,
    catalog: Option<Catalog>,
    parameter_values_schema_bound: bool,
    schema: Option<Schema>,
    partition_paths: Vec<String>,
    parameter_names: BTreeMap<i32, String>,
    parameter_specs: BTreeMap<i32, String>,
    elements: BTreeMap<u32, ExportedElement>,
}

#[allow(clippy::too_many_arguments)]
fn export_ifc(
    path: &Path,
    output: Option<&Path>,
    model_namespace: Option<&str>,
    include_unplaced: bool,
    limit: Option<usize>,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let output = output.map_or_else(|| path.with_extension("ifc"), Path::to_path_buf);
    if same_existing_file(path, &output)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IFC output must not overwrite the source RVT file",
        )
        .into());
    }

    let recovered = recover_elements(path, max_member_bytes)?;
    let geometry_statistics = geometry_statistics(&recovered.elements);
    let namespace = if let Some(value) = model_namespace {
        parse_uuid(value)?
    } else {
        let canonical = std::fs::canonicalize(path)?;
        uuid_v5(
            SOURCE_PATH_NAMESPACE,
            canonical.as_os_str().as_encoded_bytes(),
        )
    };
    let (creation_time, timestamp) = current_utc_timestamp()?;
    let project_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Rivet Project")
        .to_owned();
    let (model, included_properties, included_type_properties, omitted_properties) =
        metadata_model(&recovered, include_unplaced, limit);
    let level_count = model.levels.len();
    let element_count = model.elements.len();
    let geometry_count = model
        .elements
        .iter()
        .filter(|element| element.geometry.is_some())
        .count();
    let (mapped_family_instances, mapped_family_instance_placements) =
        mapped_family_instance_placement_counts(&model, &recovered);
    let options = MetadataOptions {
        model_namespace: namespace,
        file_name: output.to_string_lossy().into_owned(),
        timestamp,
        creation_time,
        project_name,
        site_name: "Site".to_owned(),
        building_name: "Building".to_owned(),
    };
    let file = metadata_ifc(&model, &options)?;
    let mut writer = File::create(&output)?;
    file.write_to(&mut writer)?;
    writer.flush()?;

    println!("IFC written: {}", output.display());
    println!("Model namespace: {}", format_uuid(namespace));
    println!("Building storeys: {level_count}");
    println!("Elements: {element_count}");
    println!("Elements with verified geometry: {geometry_count}");
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
    report_symbol_link_funnel(&geometry_statistics);
    println!(
        "Mapped family instances with verified placement: {mapped_family_instance_placements} of {mapped_family_instances}"
    );
    println!("Recovered Revit properties: {included_properties}");
    println!("Recovered Revit properties from the element's type: {included_type_properties}");
    if omitted_properties > 0 {
        println!(
            "Unverified parameter candidates omitted for this Revit release: {omitted_properties}"
        );
    }
    Ok(())
}

fn mapped_family_instance_placement_counts(
    model: &BimModel,
    recovered: &RecoveredElements,
) -> (usize, usize) {
    let mapped = model
        .elements
        .iter()
        .filter(|element| carries_family_symbol_geometry(element.element_type));
    let mut total = 0;
    let mut placed = 0;
    for element in mapped {
        total += 1;
        placed += usize::from(
            element
                .id
                .0
                .parse::<u32>()
                .ok()
                .and_then(|id| recovered.elements.get(&id))
                .is_some_and(|element| element.ginstance_transform.is_some()),
        );
    }
    (total, placed)
}

fn metadata_model(
    recovered: &RecoveredElements,
    include_unplaced: bool,
    limit: Option<usize>,
) -> (BimModel, usize, usize, usize) {
    let class_name = |element: &ExportedElement| {
        element.class_index.and_then(|index| {
            recovered
                .schema
                .as_ref()
                .and_then(|schema| schema.class_by_index(index))
                .map(|class| class.name.as_str())
        })
    };
    let levels = recovered
        .elements
        .iter()
        .filter(|(_, element)| {
            !element.moribund
                && class_name(element) == Some("Level")
                // Family documents contribute their own reference levels to
                // the project database. In the corpus those carry a family
                // reference; top-level project storeys do not.
                && element.family_id.is_none()
                && element.header_family_id.is_none()
        })
        .map(|(id, element)| BimLevel {
            id: BimElementId(id.to_string()),
            name: element.name.as_ref().map(|(name, _)| name.clone()),
            elevation: element.elevation_feet.and_then(|value| {
                Some(BimNumber {
                    value: revit_catalog::internal_feet_to_metres(value)?,
                    unit: Some(BimUnit {
                        id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
                        name: "Meters".to_owned(),
                    }),
                })
            }),
        })
        .collect::<Vec<_>>();
    let level_ids = levels
        .iter()
        .map(|level| level.id.clone())
        .collect::<BTreeSet<_>>();
    let candidates = recovered.elements.iter().filter(|(_, element)| {
        // An element carrying verified geometry is a candidate whether or not
        // its level was recovered. The default export otherwise requires a
        // level so that everything lands in a storey, and on BIG that is what
        // was hiding the model: of its 1 031 placed instances 461 have a level
        // and 658 have a category but only 88 have both, so the 78 that got
        // through were an unrepresentative slice whose transforms happened to
        // be near the origin - the file looked like one pile at (0,0,0) while
        // the decoded transforms actually spread over 108 x 141 x 33 m.
        //
        // Such an element is contained in the building rather than a storey,
        // which is what `--include-unplaced` already does for everything; this
        // extends it only to elements whose body and placement are verified,
        // so the default export gains geometry without gaining 272 000 rows.
        let verified_geometry = element.verified_symbol_bounds.is_some();
        !element.moribund
            && class_name(element) != Some("Level")
            && (element.category.is_some() || verified_geometry)
            && (include_unplaced || element.level_id.is_some() || verified_geometry)
    });
    let mut included_properties = 0_usize;
    let mut included_type_properties = 0_usize;
    let mut omitted_properties = 0_usize;
    let elements = candidates
        .take(limit.unwrap_or(usize::MAX))
        .map(|(id, element)| {
            let mut normalized = normalize_element(
                *id,
                element,
                &recovered.elements,
                recovered.schema.as_ref(),
                &recovered.parameter_names,
                &recovered.parameter_specs,
                recovered.catalog,
            );
            if normalized
                .level_id
                .as_ref()
                .is_some_and(|level_id| !level_ids.contains(level_id))
            {
                normalized.level_id = None;
            }
            let mut properties = trusted_source_properties(&normalized);
            // The type's values come from the same reader as the element's own
            // and carry the same risk of an unverified parameter code, so they
            // are held to the same catalogue check rather than to none.
            if recovered.parameter_values_schema_bound {
                included_properties += normalized.properties.len();
                properties.append(&mut normalized.properties);
                included_type_properties += normalized.type_properties.len();
            } else {
                omitted_properties += normalized.properties.len();
                omitted_properties += normalized.type_properties.len();
                normalized.type_properties.clear();
            }
            normalized.properties = properties;
            normalized
        })
        .collect();

    (
        BimModel {
            source: Some(BimSource {
                application: "Autodesk Revit".to_owned(),
                release: recovered.release.map(|release| release.to_string()),
            }),
            elements,
            levels,
            relations: Vec::new(),
        },
        included_properties,
        included_type_properties,
        omitted_properties,
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct GeometryStatistics {
    pipe_candidates: usize,
    pipe_candidates_with_bounds: usize,
    verified_pipe_lines: usize,
    fitting_center_line_candidates: usize,
    verified_fitting_axes: usize,
    family_instances_with_placement_candidates: usize,
    verified_family_instance_placements: usize,
    verified_ginstance_transforms: usize,
    verified_symbol_bounds: usize,
    /// Funnel from "an instance names a symbol" to "that body is placed in the
    /// world", so a shortfall can be attributed to the gate that caused it
    /// rather than guessed at. Each field counts the instances that passed
    /// every gate up to and including its own.
    instances_naming_a_symbol: usize,
    instances_whose_symbol_has_a_body: usize,
    instances_with_a_symbol_body_and_own_bounds: usize,
    instances_whose_category_matches_the_symbol: usize,
    instances_whose_bounds_match_the_symbol: usize,
    /// The bounds cross-check on its own, with the category-equality gate not
    /// applied, so the two gates can be told apart.
    instances_whose_bounds_match_ignoring_category: usize,
    /// Of the instances whose symbol has a body, how the category gate fails.
    instances_whose_symbol_has_no_category: usize,
    instances_whose_category_differs_from_the_symbol: usize,
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
        "  category gate: symbol has none {}, differs {}",
        statistics.instances_whose_symbol_has_no_category,
        statistics.instances_whose_category_differs_from_the_symbol
    );
}

fn geometry_statistics(elements: &BTreeMap<u32, ExportedElement>) -> GeometryStatistics {
    let mut statistics = GeometryStatistics::default();
    for element in elements.values() {
        if let Some(line) = element.pipe_line_candidate {
            statistics.pipe_candidates += 1;
            statistics.pipe_candidates_with_bounds +=
                usize::from(element.geometry_bounds.is_some());
            statistics.verified_pipe_lines += usize::from(
                element
                    .geometry_bounds
                    .is_some_and(|bounds| line.matches_bounds(&bounds)),
            );
        }
        statistics.fitting_center_line_candidates +=
            usize::from(element.fitting_center_line_candidate.is_some());
        statistics.verified_fitting_axes += usize::from(element.fitting_axis_candidate.is_some());
        statistics.family_instances_with_placement_candidates +=
            usize::from(!element.family_instance_placement_candidates.is_empty());
        if let Some(symbol_id) = element
            .ginstance_transform
            .and_then(|transform| transform.symbol_element_id)
        {
            statistics.instances_naming_a_symbol += 1;
            let symbol = elements.get(&symbol_id);
            if symbol.is_some_and(|symbol| symbol.brep.is_some()) {
                statistics.instances_whose_symbol_has_a_body += 1;
                if element.placement_bounds.is_some() {
                    statistics.instances_with_a_symbol_body_and_own_bounds += 1;
                }
                if element.category.is_some()
                    && element.category == symbol.and_then(|symbol| symbol.category)
                {
                    statistics.instances_whose_category_matches_the_symbol += 1;
                }
                if element.verified_symbol_bounds.is_some() {
                    statistics.instances_whose_bounds_match_the_symbol += 1;
                }
                let symbol_category = symbol.and_then(|symbol| symbol.category);
                if symbol_category.is_none() {
                    statistics.instances_whose_symbol_has_no_category += 1;
                } else if element.category != symbol_category {
                    statistics.instances_whose_category_differs_from_the_symbol += 1;
                }
                if let (Some(transform), Some(instance_bounds), Some(symbol_bounds)) = (
                    element.ginstance_transform,
                    element.placement_bounds,
                    symbol
                        .and_then(|symbol| symbol.geometry_graph.as_ref())
                        .map(|graph| graph.bounds),
                ) {
                    if instance_bounds.matches_transformed(&symbol_bounds, &transform) {
                        statistics.instances_whose_bounds_match_ignoring_category += 1;
                    }
                }
            }
        }
        statistics.verified_family_instance_placements +=
            usize::from(element.family_instance_placement.is_some());
        statistics.verified_ginstance_transforms +=
            usize::from(element.ginstance_transform.is_some());
        statistics.verified_symbol_bounds += usize::from(element.verified_symbol_bounds.is_some());
    }
    statistics
}

fn trusted_source_properties(element: &BimElement) -> Vec<BimProperty> {
    let mut properties = vec![BimProperty {
        id: None,
        name: "Revit Element Id".to_owned(),
        specification: None,
        value: BimPropertyValue::Text(element.id.0.clone()),
    }];
    for (name, value) in [
        ("Revit Class", element.class_name.as_deref()),
        (
            "Revit Category",
            element
                .category
                .as_ref()
                .map(|category| category.name.as_str()),
        ),
    ] {
        if let Some(value) = value {
            properties.push(BimProperty {
                id: None,
                name: name.to_owned(),
                specification: None,
                value: BimPropertyValue::Text(value.to_owned()),
            });
        }
    }
    properties
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
}

fn write_exported_elements(
    mut writer: Box<dyn Write>,
    elements: &BTreeMap<u32, ExportedElement>,
    metadata: &ExportMetadata<'_>,
    limit: Option<usize>,
) -> io::Result<usize> {
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

fn schema_class_index(schema: Option<&Schema>, name: &str) -> Option<u16> {
    schema
        .and_then(|schema| schema.class_by_name(name))
        .map(|class| class.index)
}

fn schema_class_is_a(schema: &Schema, class_index: u16, ancestor_index: u16) -> bool {
    let mut current = Some(class_index);
    let mut remaining = schema.classes.len();
    while let Some(index) = current {
        if index == ancestor_index {
            return true;
        }
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        current = schema
            .class_by_index(index)
            .and_then(|class| class.parent.index());
    }
    false
}

fn parameter_set_class_indexes(schema: Option<&Schema>) -> Option<ParameterSetClassIndexes> {
    Some(ParameterSetClassIndexes {
        double: schema_class_index(schema, "ParamValueSetDouble")?,
        integer: schema_class_index(schema, "ParamValueSetInt")?,
        text: schema_class_index(schema, "ParamValueSetAString")?,
        reference: schema_class_index(schema, "ParamValueSetElementId")?,
    })
}

/// Parameter identifiers point at definition elements in this same export.
fn parameter_metadata(
    elements: &BTreeMap<u32, ExportedElement>,
) -> (BTreeMap<i32, String>, BTreeMap<i32, String>) {
    let names = elements
        .iter()
        .filter_map(|(id, element)| {
            let name = element.name.as_ref()?;
            Some((i32::try_from(*id).ok()?, name.0.clone()))
        })
        .collect();
    let specs = elements
        .iter()
        .filter_map(|(id, element)| {
            Some((
                i32::try_from(*id).ok()?,
                element.parameter_spec.as_ref()?.clone(),
            ))
        })
        .collect();
    (names, specs)
}

/// Cross the format boundary once: raw Revit identifiers stay available as
/// external IDs, while numbers with a known spec become unit-bearing values.
fn normalize_element(
    id: u32,
    element: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
    schema: Option<&Schema>,
    parameter_names: &BTreeMap<i32, String>,
    parameter_specs: &BTreeMap<i32, String>,
    catalog: Option<Catalog>,
) -> BimElement {
    let class_name = element.class_index.and_then(|class_index| {
        schema
            .and_then(|schema| schema.class_by_index(class_index))
            .map(|class| class.name.clone())
    });
    let category = element.category.map(|code| BimCategory {
        id: Some(BimExternalId {
            system: "autodesk.revit.builtInCategory".to_owned(),
            value: code.to_string(),
        }),
        name: catalog
            .and_then(|catalog| catalog.built_in_category(code))
            .map_or_else(
                || code.to_string(),
                |category| category.enum_name.to_owned(),
            ),
    });
    let properties = element
        .parameters
        .iter()
        .map(|parameter| normalize_property(parameter, parameter_names, parameter_specs, catalog))
        .collect();
    let type_properties = element
        .type_parameters
        .iter()
        .map(|parameter| normalize_property(parameter, parameter_names, parameter_specs, catalog))
        .collect();
    let element_type = element_type_for_source(
        class_name.as_deref(),
        category.as_ref().map(|category| category.name.as_str()),
    );

    let geometry = normalize_geometry(element, element_type, elements);
    if let Some(BimGeometry::Brep(brep)) = &geometry {
        if let Some(extent) = brep_extent_metres(brep) {
            if extent < MIN_PLAUSIBLE_BREP_EXTENT_METRES {
                let name = element.name.as_ref().map_or("<unnamed>", |(name, _)| name);
                eprintln!(
                    "warning: element {id} ({name}) has a boundary representation only {:.3} mm across - likely degenerate source geometry, not a decode error (both the Face/Edge reconstruction and the record's own declared bounding box agree on this size)",
                    extent * 1000.0
                );
            }
        }
    }

    BimElement {
        id: BimElementId(id.to_string()),
        element_type,
        class_name,
        name: element.name.as_ref().map(|(name, _)| name.clone()),
        category,
        level_id: element.level_id.map(|id| BimElementId(id.to_string())),
        // The declared type reference, or the symbol verified by bounds; the
        // family reference the header carries is neither.
        type_id: element
            .type_element_reference()
            .map(|id| BimElementId(id.to_string())),
        placement: normalize_placement(element.ginstance_transform),
        geometry,
        properties,
        type_properties,
    }
}

/// Below this, a `Brep` body is flagged as likely-degenerate source geometry
/// rather than a real physical part (see `normalize_element`'s warning). Not
/// a hard rule - chosen to be well under any real MEP fitting while still
/// catching sub-millimetre slivers like a family authored with a units bug.
const MIN_PLAUSIBLE_BREP_EXTENT_METRES: f64 = 0.005;

/// The largest axis-aligned extent across every vertex the body's edges
/// name, in metres. `None` for a body with no edges.
fn brep_extent_metres(brep: &BimBrep) -> Option<f64> {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut seen = false;
    for face in &brep.faces {
        for loop_edges in &face.loops {
            for edge in loop_edges {
                for point in [&edge.start, &edge.end] {
                    seen = true;
                    for axis in 0..3 {
                        min[axis] = min[axis].min(point.coordinates[axis]);
                        max[axis] = max[axis].max(point.coordinates[axis]);
                    }
                }
            }
        }
    }
    seen.then(|| (0..3).map(|axis| max[axis] - min[axis]).fold(0.0, f64::max))
}

fn normalize_placement(transform: Option<GInstanceTransformFields>) -> Option<BimPlacement> {
    let transform = transform?;
    let origin = transform
        .origin
        .coordinates_feet
        .map(revit_catalog::internal_feet_to_metres);
    let [Some(origin_x), Some(origin_y), Some(origin_z)] = origin else {
        return None;
    };
    Some(BimPlacement {
        origin: BimPoint3 {
            coordinates: [origin_x, origin_y, origin_z],
            unit: metres_unit(),
        },
        reference_direction: transform.basis[0],
        axis: transform.basis[2],
    })
}

/// Whether a mapped type is placed from a family symbol, and can therefore
/// carry a verified symbol extent. A pipe segment is a swept curve rather than
/// a placed symbol, and is handled before this.
///
/// An unclassified source is *not* excluded. Whether the exporter can name the
/// kind of building element something is, and whether its geometry was
/// verified, are independent questions: the symbol link is accepted only when
/// the instance's own independently decoded box agrees with the symbol's box
/// carried through its transform to within 1e-8 feet, which says nothing about
/// the category and does not need to. Excluding `Unknown` discarded the body of
/// every element whose category the mapping does not cover, which on this
/// corpus is most of them, and `IfcBuildingElementProxy` - what an unclassified
/// element is exported as - carries a shape representation perfectly well.
fn carries_family_symbol_geometry(element_type: BimElementType) -> bool {
    !matches!(element_type, BimElementType::PipeSegment)
}

fn normalize_geometry(
    element: &ExportedElement,
    element_type: BimElementType,
    elements: &BTreeMap<u32, ExportedElement>,
) -> Option<BimGeometry> {
    let metres = |value| revit_catalog::internal_feet_to_metres(value);
    let point = |coordinates: [f64; 3]| {
        Some(BimPoint3 {
            coordinates: [
                metres(coordinates[0])?,
                metres(coordinates[1])?,
                metres(coordinates[2])?,
            ],
            unit: metres_unit(),
        })
    };
    if let (Some(line), Some(bounds)) = (element.pipe_line_candidate, element.geometry_bounds) {
        if let Some(radius_feet) = line.swept_radius_feet(&bounds) {
            return Some(BimGeometry::SweptDisk(BimSweptDisk {
                directrix: BimLineSegment {
                    start: point(line.start.coordinates_feet)?,
                    end: point(line.end.coordinates_feet)?,
                },
                radius: BimNumber {
                    value: metres(radius_feet)?,
                    unit: Some(metres_unit()),
                },
            }));
        }
    }
    if let Some(line) = element.fitting_axis_candidate {
        return Some(BimGeometry::AxisLine(BimLineSegment {
            start: point(line.start.coordinates_feet)?,
            end: point(line.end.coordinates_feet)?,
        }));
    }
    if !carries_family_symbol_geometry(element_type) {
        return None;
    }
    let symbol = element.verified_symbol_bounds?;
    if let (Some(local_brep), Some(transform)) = (
        elements
            .get(&symbol.symbol_element_id)
            .and_then(|symbol_element| symbol_element.brep.as_ref()),
        element.ginstance_transform,
    ) {
        if let Some(brep) = normalize_brep(local_brep, &transform) {
            // Only a body whose every face resolved is emitted. An incomplete
            // one is schema-valid as an open `IfcShellBasedSurfaceModel`, and
            // for a handful of records it geometrizes, but at corpus scale it
            // does not: of SMALL's 1 771 incomplete shells IfcOpenShell builds
            // 831 and fails on 940, while all 695 complete bodies build. A
            // body the reference kernel refuses is not something to ship, so
            // an incomplete one falls back to the symbol's verified box.
            if brep.complete {
                return Some(BimGeometry::Brep(brep));
            }
        }
    }
    Some(BimGeometry::BoundingBox(BimBoundingBox {
        min: point(symbol.bounds.min)?,
        max: point(symbol.bounds.max)?,
    }))
}

/// Place a symbol-local `SymbolBrep` (Revit internal feet) into world
/// coordinates via the instance's own `GInstance` transform, and convert to
/// metres. Reuses the same rigid transform
/// [`GElementBounds::matches_transformed`] already applies to bounding-box
/// corners: `world = origin + sum_i local[i] * basis[i]` for a point, and
/// the same sum without `origin` for a direction.
fn normalize_brep(
    local: &rvt_model::SymbolBrep,
    transform: &GInstanceTransformFields,
) -> Option<BimBrep> {
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value);
    let world_point = |local_point: [f64; 3]| -> Option<BimPoint3> {
        let mut world = transform.origin.coordinates_feet;
        for (axis_local, value) in local_point.into_iter().enumerate() {
            for (axis_world, component) in world.iter_mut().enumerate() {
                *component += transform.basis[axis_local][axis_world] * value;
            }
        }
        Some(BimPoint3 {
            coordinates: [metres(world[0])?, metres(world[1])?, metres(world[2])?],
            unit: metres_unit(),
        })
    };
    let world_direction = |local_direction: [f64; 3]| -> [f64; 3] {
        let mut world = [0.0; 3];
        for (axis_local, value) in local_direction.into_iter().enumerate() {
            for (axis_world, component) in world.iter_mut().enumerate() {
                *component += transform.basis[axis_local][axis_world] * value;
            }
        }
        world
    };
    let mut faces = Vec::with_capacity(local.faces.len());
    for face in &local.faces {
        let surface = match face.surface {
            rvt_model::BrepSurface::Plane {
                origin,
                x_axis,
                y_axis,
            } => BimBrepSurface::Plane {
                origin: world_point(origin)?,
                x_axis: world_direction(x_axis),
                y_axis: world_direction(y_axis),
            },
            rvt_model::BrepSurface::Cylinder {
                center,
                x_axis,
                y_axis,
                z_axis,
                radius,
            } => BimBrepSurface::Cylinder {
                center: world_point(center)?,
                x_axis: world_direction(x_axis),
                y_axis: world_direction(y_axis),
                z_axis: world_direction(z_axis),
                radius: BimNumber {
                    value: metres(radius)?,
                    unit: Some(metres_unit()),
                },
            },
        };
        let mut loops = Vec::with_capacity(face.loops.len());
        for loop_edges in &face.loops {
            let mut edges = Vec::with_capacity(loop_edges.len());
            for edge in loop_edges {
                let curve = match edge.curve {
                    rvt_model::BrepCurve::Line => BimBrepCurve::Line,
                    rvt_model::BrepCurve::Arc(arc) => BimBrepCurve::Arc(BimBrepArc {
                        center: world_point(arc.center)?,
                        x_axis: world_direction(arc.x_axis),
                        z_axis: world_direction(arc.z_axis),
                        radius: BimNumber {
                            value: metres(arc.radius)?,
                            unit: Some(metres_unit()),
                        },
                        start_angle: arc.start_angle,
                        end_angle: arc.end_angle,
                    }),
                };
                edges.push(BimBrepEdge {
                    start: world_point(edge.start)?,
                    end: world_point(edge.end)?,
                    curve,
                });
            }
            loops.push(edges);
        }
        faces.push(BimBrepFace { surface, loops });
    }
    Some(BimBrep {
        faces,
        complete: local.excluded_faces.is_empty(),
    })
}

fn metres_unit() -> BimUnit {
    BimUnit {
        id: "autodesk.unit.unit:meters-1.0.0".to_owned(),
        name: "Meters".to_owned(),
    }
}

fn normalize_property(
    parameter: &rvt_model::Parameter,
    parameter_names: &BTreeMap<i32, String>,
    parameter_specs: &BTreeMap<i32, String>,
    catalog: Option<Catalog>,
) -> BimProperty {
    let built_in = catalog.and_then(|catalog| catalog.built_in_parameter(parameter.id));
    let name = parameter_names
        .get(&parameter.id)
        .cloned()
        .or_else(|| built_in.map(|parameter| parameter.display_name.to_owned()))
        .unwrap_or_else(|| format!("param_{}", parameter.id));
    let specification = parameter_specs.get(&parameter.id).cloned();
    let value = match &parameter.value {
        ParameterValue::Double(number) => {
            let normalized = specification
                .as_deref()
                .and_then(|type_id| catalog.and_then(|catalog| catalog.specification(type_id)))
                .and_then(|specification| {
                    Some((specification, specification.from_internal(*number)?))
                });
            let (value, unit) = normalized.map_or((*number, None), |(specification, value)| {
                (
                    value,
                    Some(BimUnit {
                        id: specification.storage_unit.to_owned(),
                        name: specification.storage_unit_name.to_owned(),
                    }),
                )
            });
            BimPropertyValue::Number(BimNumber { value, unit })
        }
        ParameterValue::Integer(number) => BimPropertyValue::Integer(i64::from(*number)),
        ParameterValue::Text(text) => BimPropertyValue::Text(text.clone()),
        ParameterValue::Reference(reference) => {
            BimPropertyValue::Reference(BimElementId(reference.to_string()))
        }
    };
    BimProperty {
        id: Some(BimExternalId {
            system: if parameter.is_built_in() {
                "autodesk.revit.builtInParameter"
            } else {
                "autodesk.revit.parameterElementId"
            }
            .to_owned(),
            value: parameter.id.to_string(),
        }),
        name,
        specification,
        value,
    }
}

/// Emit one element as a JSON object on its own line. Fields that were not
/// recovered are omitted rather than written as a guessed value.
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
    for (key, value) in [
        ("category", element.category),
        ("level_id", element.level_id),
        ("family_id", element.family_id.or(element.header_family_id)),
        ("type_id", element.type_element_id),
        ("owner_view_id", element.owner_view_id),
        ("created_phase_id", element.created_phase_id),
        ("design_option_id", element.design_option_id),
    ] {
        if let Some(value) = value {
            write!(writer, ",\"{key}\":{value}")?;
        }
    }
    if let Some(category) = &normalized.category {
        write!(
            writer,
            ",\"category_name\":\"{}\"",
            json_escape(&category.name)
        )?;
    }
    write_geometry_json(writer, normalized.geometry.as_ref())?;
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

fn write_geometry_json(writer: &mut impl Write, geometry: Option<&BimGeometry>) -> io::Result<()> {
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
        Some(BimGeometry::Brep(brep)) => {
            let (mut lines, mut arcs) = (0_usize, 0_usize);
            for edge in brep
                .faces
                .iter()
                .flat_map(|face| face.loops.iter())
                .flat_map(|edge_loop| edge_loop.iter())
            {
                match edge.curve {
                    BimBrepCurve::Line => lines += 1,
                    BimBrepCurve::Arc(_) => arcs += 1,
                }
            }
            write!(
                writer,
                ",\"geometry\":{{\"kind\":\"brep\",\"faces\":{},\"complete\":{},\"line_edges\":{lines},\"arc_edges\":{arcs}}}",
                brep.faces.len(),
                brep.complete
            )
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

/// Read the element's name, preferring the offset the class agrees on and
/// falling back to the first readable string after the `Element` tail.
fn read_name(
    body: &[u8],
    tail_end: usize,
    settled_offset: Option<usize>,
) -> Option<(String, &'static str)> {
    if let Some(offset) = settled_offset {
        if let Some(found) = RecordString::parse_at(body, tail_end + offset) {
            return Some((found.value, "offset"));
        }
    }
    RecordString::scan_from(body, tail_end).map(|found| (found.value, "scan"))
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

fn read_schema(container: &RvtContainer) -> Result<Option<Schema>, Box<dyn Error>> {
    if container.stream("Formats/Latest").is_none() {
        return Ok(None);
    }
    let raw = container.read_stream_with_limit("Formats/Latest", DEFAULT_DECODE_LIMIT as u64)?;
    Ok(Some(decode_schema_stream(&raw)?.1))
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

fn schema_retry_error(raw: &str, stripped: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "schema parse failed before checksum-page cleanup ({raw}); retry also failed ({stripped})"
        ),
    )
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

fn read_basic_file_info(container: &RvtContainer) -> Result<Option<BasicFileInfo>, Box<dyn Error>> {
    if container.stream("BasicFileInfo").is_none() {
        return Ok(None);
    }
    let bytes = container.read_stream_with_limit("BasicFileInfo", 16 * 1024 * 1024)?;
    Ok(Some(BasicFileInfo::parse(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promotes_only_a_bounds_verified_pipe_to_metric_geometry() {
        let element = ExportedElement {
            pipe_line_candidate: Some(PipeLineGeometryFields {
                line_offset: 100,
                nominal_diameter_feet: 0.2,
                start: rvt_model::RvtPoint3 {
                    coordinates_feet: [12.0, 20.0, 30.0],
                },
                end: rvt_model::RvtPoint3 {
                    coordinates_feet: [15.0, 20.0, 30.0],
                },
            }),
            geometry_bounds: Some(GElementBounds {
                offset: 24,
                min: [12.0, 19.88, 29.88],
                max: [15.0, 20.12, 30.12],
            }),
            ..ExportedElement::default()
        };

        let Some(BimGeometry::SweptDisk(geometry)) =
            normalize_geometry(&element, BimElementType::PipeSegment, &BTreeMap::new())
        else {
            panic!("verified pipe geometry was not promoted");
        };
        assert!((geometry.directrix.start.coordinates[0] - 3.6576).abs() < 1.0e-12);
        assert!((geometry.directrix.end.coordinates[0] - 4.572).abs() < 1.0e-12);
        assert!((geometry.radius.value - 0.036_576).abs() < 1.0e-12);

        let mut mismatched = element;
        mismatched.geometry_bounds.as_mut().unwrap().max[2] += 1.0;
        assert!(
            normalize_geometry(&mismatched, BimElementType::PipeSegment, &BTreeMap::new())
                .is_none()
        );
    }

    #[test]
    fn promotes_an_owner_verified_fitting_axis_without_inventing_a_body() {
        let element = ExportedElement {
            fitting_axis_candidate: Some(FittingCenterLineFields {
                owner_element_id: 417_660,
                start: rvt_model::RvtPoint3 {
                    coordinates_feet: [10.0, 20.0, 30.0],
                },
                end: rvt_model::RvtPoint3 {
                    coordinates_feet: [10.0, 20.0, 32.0],
                },
            }),
            ..ExportedElement::default()
        };
        let Some(BimGeometry::AxisLine(line)) =
            normalize_geometry(&element, BimElementType::PipeFitting, &BTreeMap::new())
        else {
            panic!("verified fitting axis was not promoted");
        };
        for (actual, expected) in line
            .start
            .coordinates
            .into_iter()
            .chain(line.end.coordinates)
            .zip([3.048, 6.096, 9.144, 3.048, 6.096, 9.7536])
        {
            assert!((actual - expected).abs() < 1.0e-12);
        }
    }

    #[test]
    fn promotes_a_bounds_verified_ginstance_transform_to_metric_placement() {
        let transform = GInstanceTransformFields {
            offset: 164,
            basis: [[0.0, 0.0, 1.0], [0.0, -1.0, 0.0], [1.0, 0.0, 0.0]],
            origin: rvt_model::RvtPoint3 {
                coordinates_feet: [10.0, 20.0, 30.0],
            },
            symbol_element_id: Some(417_391),
        };
        let placement = normalize_placement(Some(transform)).unwrap();
        for (actual, expected) in placement
            .origin
            .coordinates
            .into_iter()
            .zip([3.048, 6.096, 9.144])
        {
            assert!((actual - expected).abs() < 1.0e-12);
        }
        for (actual, expected) in placement
            .reference_direction
            .into_iter()
            .chain(placement.axis)
            .zip(transform.basis[0].into_iter().chain(transform.basis[2]))
        {
            assert!((actual - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn attaches_symbol_bounds_only_when_the_symbol_box_has_volume() {
        let symbol_bounds = |max: [f64; 3]| GElementBounds {
            offset: 54,
            min: [-1.0, -2.0, -3.0],
            max,
        };
        let model = |max: [f64; 3]| {
            let mut elements = BTreeMap::new();
            elements.insert(
                5,
                ExportedElement {
                    class_index: Some(7),
                    category: Some(1),
                    geometry_graph: Some(GElementGraphFields {
                        top_level_nodes: Vec::new(),
                        bounds: symbol_bounds(max),
                    }),
                    ..ExportedElement::default()
                },
            );
            elements.insert(
                9,
                ExportedElement {
                    class_index: Some(3),
                    category: Some(1),
                    ginstance_transform: Some(GInstanceTransformFields {
                        offset: 0,
                        basis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                        origin: rvt_model::RvtPoint3 {
                            coordinates_feet: [10.0, 20.0, 30.0],
                        },
                        symbol_element_id: Some(5),
                    }),
                    placement_bounds: Some(GElementBounds {
                        offset: 12,
                        min: [9.0, 18.0, 27.0],
                        max: [10.0 + max[0], 20.0 + max[1], 30.0 + max[2]],
                    }),
                    ..ExportedElement::default()
                },
            );
            elements
        };

        let mut volumetric = model([1.0, 2.0, 3.0]);
        attach_symbol_bounds(&mut volumetric, Some(7));
        assert_eq!(
            volumetric[&9].verified_symbol_bounds,
            Some(VerifiedSymbolBounds {
                symbol_element_id: 5,
                bounds: symbol_bounds([1.0, 2.0, 3.0]),
            })
        );

        let mut flat = model([1.0, 2.0, -3.0]);
        attach_symbol_bounds(&mut flat, Some(7));
        assert_eq!(flat[&9].verified_symbol_bounds, None);
    }

    #[test]
    fn an_instance_without_a_category_takes_its_verified_symbol_s() {
        // The bounds cross-check is the verification; the category is then
        // inherited rather than required to match, because most placed family
        // instances in the corpus carry no category of their own and the
        // exporter drops a categoryless element with its body. A category that
        // *disagrees* still refuses the link.
        let symbol_bounds = GElementBounds {
            offset: 54,
            min: [-1.0, -2.0, -3.0],
            max: [1.0, 2.0, 3.0],
        };
        let model = |instance_category: Option<i32>| {
            let mut elements = BTreeMap::new();
            elements.insert(
                5,
                ExportedElement {
                    class_index: Some(7),
                    category: Some(-2_008_049),
                    geometry_graph: Some(GElementGraphFields {
                        top_level_nodes: Vec::new(),
                        bounds: symbol_bounds,
                    }),
                    ..ExportedElement::default()
                },
            );
            elements.insert(
                9,
                ExportedElement {
                    class_index: Some(3),
                    category: instance_category,
                    ginstance_transform: Some(GInstanceTransformFields {
                        offset: 0,
                        basis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                        origin: rvt_model::RvtPoint3 {
                            coordinates_feet: [10.0, 20.0, 30.0],
                        },
                        symbol_element_id: Some(5),
                    }),
                    placement_bounds: Some(GElementBounds {
                        offset: 12,
                        min: [9.0, 18.0, 27.0],
                        max: [11.0, 22.0, 33.0],
                    }),
                    ..ExportedElement::default()
                },
            );
            elements
        };

        let mut inherited = model(None);
        attach_symbol_bounds(&mut inherited, Some(7));
        assert!(inherited[&9].verified_symbol_bounds.is_some());
        assert_eq!(inherited[&9].category, Some(-2_008_049));
        assert_eq!(inherited[&9].category_source, Some("symbol"));

        // A category the element declared is kept, and its provenance with it.
        let mut declared = model(Some(-2_008_049));
        attach_symbol_bounds(&mut declared, Some(7));
        assert!(declared[&9].verified_symbol_bounds.is_some());
        assert_eq!(declared[&9].category_source, None);

        // Two categories that disagree refuse the link outright.
        let mut disagreeing = model(Some(-2_000_151));
        attach_symbol_bounds(&mut disagreeing, Some(7));
        assert_eq!(disagreeing[&9].verified_symbol_bounds, None);
        assert_eq!(disagreeing[&9].category, Some(-2_000_151));
    }

    #[test]
    fn a_type_s_parameters_reach_its_instances_without_joining_their_own() {
        let parameter = |id: i32, value: &str| rvt_model::Parameter {
            id,
            value: ParameterValue::Text(value.to_owned()),
        };
        let mut elements = BTreeMap::new();
        elements.insert(
            5,
            ExportedElement {
                parameters: vec![
                    parameter(214_890, "SANEXT"),
                    parameter(214_891, "\u{448}\u{442}."),
                ],
                ..ExportedElement::default()
            },
        );
        // One instance reaches its type through the declared reference, one
        // through bounds, and one names a type that stores nothing.
        elements.insert(
            9,
            ExportedElement {
                type_element_id: Some(5),
                parameters: vec![parameter(214_891, "\u{43c}")],
                ..ExportedElement::default()
            },
        );
        elements.insert(
            11,
            ExportedElement {
                verified_symbol_bounds: Some(VerifiedSymbolBounds {
                    symbol_element_id: 5,
                    bounds: GElementBounds {
                        offset: 0,
                        min: [0.0; 3],
                        max: [1.0; 3],
                    },
                }),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            13,
            ExportedElement {
                type_element_id: Some(7),
                ..ExportedElement::default()
            },
        );
        inherit_symbol_parameters(&mut elements);

        // The instance sets 214891 itself, so only the type's other value is
        // inherited, and the element's own list is untouched either way.
        assert_eq!(
            elements[&9].type_parameters,
            vec![parameter(214_890, "SANEXT")]
        );
        assert_eq!(elements[&9].parameters, vec![parameter(214_891, "\u{43c}")]);
        assert_eq!(elements[&11].type_parameters.len(), 2);
        assert!(elements[&13].type_parameters.is_empty());
        // A type carries none of its own instances' values.
        assert!(elements[&5].type_parameters.is_empty());
    }

    #[test]
    fn a_declared_type_reference_is_preferred_over_a_bounds_verified_one() {
        let bounds = VerifiedSymbolBounds {
            symbol_element_id: 5,
            bounds: GElementBounds {
                offset: 0,
                min: [0.0; 3],
                max: [1.0; 3],
            },
        };
        let element = |declared: Option<i32>| ExportedElement {
            type_element_id: declared,
            verified_symbol_bounds: Some(bounds),
            ..ExportedElement::default()
        };
        assert_eq!(element(Some(9)).type_element_reference(), Some(9));
        assert_eq!(element(None).type_element_reference(), Some(5));
        // A negative identifier is not an element, and must not wrap round.
        assert_eq!(element(Some(-2)).type_element_reference(), Some(5));
        assert_eq!(ExportedElement::default().type_element_reference(), None);
    }

    #[test]
    fn promotes_only_verified_mapped_symbol_bounds_as_a_box() {
        let element = ExportedElement {
            verified_symbol_bounds: Some(VerifiedSymbolBounds {
                symbol_element_id: 417_391,
                bounds: GElementBounds {
                    offset: 54,
                    min: [-1.0, -2.0, -3.0],
                    max: [1.0, 2.0, 3.0],
                },
            }),
            ..ExportedElement::default()
        };
        let Some(BimGeometry::BoundingBox(bounds)) =
            normalize_geometry(&element, BimElementType::SanitaryTerminal, &BTreeMap::new())
        else {
            panic!("verified symbol bounds were not promoted");
        };
        for (actual, expected) in bounds
            .min
            .coordinates
            .into_iter()
            .chain(bounds.max.coordinates)
            .zip([-0.3048, -0.6096, -0.9144, 0.3048, 0.6096, 0.9144])
        {
            assert!((actual - expected).abs() < 1.0e-12);
        }
        // A pipe segment is a swept curve, not a placed symbol, so it takes no
        // symbol extent. An unclassified source does: whether the exporter can
        // name the kind of element something is has nothing to do with whether
        // its geometry was verified, and refusing `Unknown` discarded the body
        // of most of the corpus. See `carries_family_symbol_geometry`.
        assert!(
            normalize_geometry(&element, BimElementType::PipeSegment, &BTreeMap::new()).is_none()
        );
        for carried in [BimElementType::Unknown, BimElementType::DistributionElement] {
            assert!(matches!(
                normalize_geometry(&element, carried, &BTreeMap::new()),
                Some(BimGeometry::BoundingBox(_))
            ));
        }
    }
}
