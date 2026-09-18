//! The commands that report what a file contains: its container, its schema,
//! its records, and the objects recovered from them.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::Write as _,
    fs::File,
    io::{self, Read as _, Write},
    path::{Path, PathBuf},
};

use revit_catalog::Catalog;
use rvt_container::{DEFAULT_DECODE_LIMIT, PartitionReadOptions, RvtContainer, StreamFraming};
use rvt_model::{
    ELEMENT_TAIL_BYTES, ElementAnchor, ElementFields, ElementHeaderFields, MemberWalk,
    ParameterSets, ParameterValue, RecordHeader, RecordLayout,
};
use rvt_schema::{Schema, TypeReference};
// The whole semantic reconstruction moved into `rvt-import`. Glob-imported
// because the probe commands below read the same intermediate the pipeline
// builds, and naming each item here would be a second list to keep in step.
#[allow(clippy::wildcard_imports)] // The binary's own constants.
use crate::*;
use rvt_import::{
    ELEMENT_CLASS_FORMAT_TAG, ELEMENT_HEADER_CLASS, calibrate_names, decode_elem_table_stream,
    decode_schema_stream, elem_table_ids, for_each_member, parameter_set_class_indexes,
    partition_paths, read_basic_file_info, read_schema,
};

/// Leading bytes of an IFC read to find the identity its header states.
///
/// The header is the first thing in an ISO 10303-21 file and ends at
/// `ENDSEC;`, so this is a generous bound on it rather than a guess: a
/// `FILE_DESCRIPTION` naming a long view definition, a converter and several
/// source documents still fits, and reading this much of a 700 MB export
/// costs nothing.
const IFC_HEADER_BYTES: usize = 64 * 1024;

/// Which file, and which save of it, each of these is - and which of them are
/// the same file.
///
/// This is the question a pipeline asks before converting: an RVT states its
/// own identity, and an IFC this converter wrote states the identity of the
/// RVT it came from, so the two can be compared. Files are grouped by that
/// identity, which is what makes a duplicate visible rather than merely
/// reported.
///
/// Nothing here fails as a whole: an unreadable file is one line of the
/// report, so the scan always has an answer about the files it could read.
pub(crate) fn document_ids(paths: &[PathBuf]) {
    let mut by_guid: BTreeMap<String, Vec<&PathBuf>> = BTreeMap::new();
    let mut unidentified = Vec::new();
    for path in paths {
        // One unreadable file does not stop the scan. The question being
        // asked is about the set - which of these are the same document -
        // and answering it for 73 of 74 files beats answering it for none.
        let read = read_document_identity(path);
        println!("{}", path.display());
        let (format, identity) = match read {
            Ok(read) => read,
            Err(error) => {
                println!("  Unreadable: {error}");
                unidentified.push(path);
                println!();
                continue;
            }
        };
        println!(
            "  Format: {}",
            format.map_or("unrecognized", bim_convert::Format::label)
        );
        if let Some(identity) = identity {
            let guid = identity.document_guid.clone().unwrap_or_default();
            print_identity("  ", &identity);
            by_guid.entry(guid).or_default().push(path);
        } else {
            // Said plainly, because "no identity" and "a new file" are
            // different answers and only the file can tell them apart.
            println!("  Document GUID: none stated");
            unidentified.push(path);
        }
        println!();
    }
    let duplicates: Vec<_> = by_guid
        .iter()
        .filter(|(_, files)| files.len() > 1)
        .collect();
    if duplicates.is_empty() {
        println!(
            "Distinct documents: {} of {} files, no duplicates",
            by_guid.len(),
            paths.len() - unidentified.len()
        );
    } else {
        println!("Duplicates, by document GUID:");
        for (guid, files) in duplicates {
            println!("  {guid}");
            for file in files {
                println!("    {}", file.display());
            }
        }
    }
    if !unidentified.is_empty() {
        println!(
            "Stating no identity: {} file(s) - an RVT too old to carry one, or an \
             IFC this converter did not write",
            unidentified.len()
        );
    }
}

/// What one file is, and the identity it states, from whichever of the two
/// readers its format calls for.
type StatedIdentity = (
    Option<bim_convert::Format>,
    Option<bim_core::BimDocumentIdentity>,
);

fn read_document_identity(path: &Path) -> Result<StatedIdentity, Box<dyn Error>> {
    let format = bim_convert::Format::sniff_file(path)?;
    let identity = match format {
        Some(bim_convert::Format::Rvt) => {
            rvt_import::document_identity(&RvtContainer::open(path)?)?
        }
        Some(bim_convert::Format::Ifc) => ifc_header_identity(path)?,
        _ => None,
    };
    Ok((format, identity))
}

fn print_identity(indent: &str, identity: &bim_core::BimDocumentIdentity) {
    println!(
        "{indent}Document GUID: {}",
        identity.document_guid.as_deref().unwrap_or("none stated")
    );
    if let Some(increment) = identity.increment {
        println!("{indent}Save number: {increment}");
    }
    if let Some(worksharing) = identity.worksharing.as_deref() {
        println!("{indent}Worksharing: {worksharing}");
    }
    // Named as lineage where they are printed, too: these group the saves of
    // one model and are shared by unrelated files, so nothing should compare
    // them to decide two files are the same.
    if let Some(creation) = identity.creation_guid.as_deref() {
        println!("{indent}Template lineage: {creation}");
    }
    if let Some(detach) = identity.detach_guid.as_deref() {
        println!("{indent}Central lineage: {detach}");
    }
}

/// The identity an IFC's own header states, read without parsing the file.
///
/// The exporter writes it into `FILE_DESCRIPTION` precisely so that this
/// question needs the head of the file and not the whole of it. The property
/// set on `IfcProject` carries the same GUID and more beside it; reading that
/// is a conversion, which is what `export-json` does.
fn ifc_header_identity(
    path: &Path,
) -> Result<Option<bim_core::BimDocumentIdentity>, Box<dyn Error>> {
    let mut head = vec![0_u8; IFC_HEADER_BYTES];
    let read = {
        let mut file = File::open(path)?;
        let mut filled = 0;
        loop {
            match file.read(&mut head[filled..])? {
                0 => break filled,
                count => filled += count,
            }
            if filled == head.len() {
                break filled;
            }
        }
    };
    let head = String::from_utf8_lossy(&head[..read]);
    let header = head.split("ENDSEC;").next().unwrap_or(&head);
    let Some(guid) = header
        .split("SourceDocument [")
        .skip(1)
        .find_map(|rest| rest.split(']').next())
    else {
        return Ok(None);
    };
    Ok(Some(bim_core::BimDocumentIdentity {
        document_guid: Some(guid.to_owned()),
        ..bim_core::BimDocumentIdentity::default()
    }))
}

pub(crate) fn info(path: &Path) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let basic_info = read_basic_file_info(&container)?;

    println!("File: {}", path.display());
    println!("Container: CFB/OLE");
    let release = basic_info.as_ref().and_then(|info| info.revit_version);
    println!(
        "Revit version: {}",
        release.map_or_else(|| "unknown".to_owned(), |year| year.to_string())
    );
    // Reading the release is one thing; having tables for it is another, and
    // only the second decides whether this file's built-in codes get names.
    println!(
        "Identifier catalog: {}",
        match release {
            Some(year) if Catalog::supports_release(year) => format!("Revit {year}"),
            _ => format!(
                "none for this release (this build carries {})",
                revit_catalog::RELEASES
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    );
    if let Some(identity) = rvt_import::document_identity(&container)? {
        print_identity("", &identity);
    }
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

pub(crate) fn streams(path: &Path) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    for stream in container.streams() {
        println!("{}\t{}", stream.len(), stream.path());
    }
    Ok(())
}

pub(crate) fn dump_stream(
    path: &Path,
    name: &str,
    output: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
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

pub(crate) fn partitions(
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

pub(crate) fn schema(
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
pub(crate) fn print_class_properties(
    schema: &Schema,
    class_name: &str,
) -> Result<(), Box<dyn Error>> {
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
pub(crate) fn print_matching_properties(schema: &Schema, text: &str) {
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

pub(crate) fn escape_terminal_text(value: &str) -> String {
    value.escape_default().collect()
}

pub(crate) fn elem_table(path: &Path, show_records: bool) -> Result<(), Box<dyn Error>> {
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
    println!("Object class index: {}", table.class_index);
    println!("Declared records: {}", table.declared_records);
    println!("Parsed records: {}", table.records.len());
    println!("Unique element IDs: {}", table.unique_id_count());
    println!(
        "Copied in from another document: {}",
        table.copied_in_count()
    );
    println!("Preserved header bytes: {}", table.leading_bytes().len());
    println!("Preserved trailing bytes: {}", table.trailing_bytes().len());

    if show_records {
        println!();
        println!(
            "Record inventory (offset, id, original, creation, last change, \
             last user change, partition, owner):"
        );
        for (index, record) in table.records.iter().enumerate() {
            println!(
                "{index}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                record.offset,
                record.id,
                record.original_id,
                record.creation_episode,
                record.last_modification_episode,
                record.last_user_modification_episode,
                record.partition_id,
                record.owning_element_id
            );
        }
    }
    Ok(())
}

pub(crate) fn percentage(numerator: usize, denominator: usize) -> String {
    if denominator == 0 {
        return "0.000%".to_owned();
    }
    let thousandths = (numerator as u128 * 100_000) / denominator as u128;
    format!("{}.{:03}%", thousandths / 1000, thousandths % 1000)
}

pub(crate) fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(crate) fn top_counts<K: Clone + Ord, V: Copy + Ord>(
    counts: &BTreeMap<K, V>,
    limit: usize,
) -> Vec<(K, V)> {
    let mut entries = counts
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    entries.truncate(limit);
    entries
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Counters for the record header under test.
#[derive(Debug, Default)]
pub(crate) struct RecordStatistics {
    pub(crate) records: u64,
    pub(crate) narrow_records: u64,
    pub(crate) wide_records: u64,
    /// First `u32` of every record header.
    pub(crate) lead_values: BTreeSet<u32>,
    pub(crate) lead_in_elem_table: u64,
    pub(crate) lead_hits: BTreeSet<u32>,
    /// Members whose record lead values are strictly ascending.
    pub(crate) ascending_members: usize,
    pub(crate) checked_members: usize,
    pub(crate) tag_records: BTreeMap<u32, u64>,
    pub(crate) tag_class_hits: BTreeMap<u32, u64>,
    pub(crate) tag_class_counts: BTreeMap<(u32, u16), u64>,
    pub(crate) class_counts: BTreeMap<u16, u64>,
    /// The `u16` sharing the trailing word with the class index.
    pub(crate) companion_words: BTreeMap<u16, u64>,
    /// Second word of the wide header, which the narrow layout does not have.
    pub(crate) wide_second_words: BTreeMap<u32, u64>,
    pub(crate) body_min: u64,
    pub(crate) body_max: u64,
    pub(crate) empty_bodies: u64,
}

pub(crate) fn element(
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
pub(crate) fn parameters(
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

pub(crate) fn describe_parameter_value(value: &ParameterValue) -> String {
    match value {
        ParameterValue::Double(number) => format!("double={number}"),
        ParameterValue::Integer(number) => format!("int={number}"),
        ParameterValue::Text(text) => format!("text={text:?}"),
        ParameterValue::Reference(id) => format!("ref={id}"),
    }
}

pub(crate) fn bodies(
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

pub(crate) fn records(
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

pub(crate) fn observe_records(
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

pub(crate) fn dump_record_headers(
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

pub(crate) fn print_record_statistics(
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

pub(crate) fn print_class_row(schema: &Schema, index: u16, count: u64) {
    let name = schema
        .class_by_index(index)
        .map_or("?", |class| class.name.as_str());
    println!("  {index}\t{}\tcount={count}", escape_terminal_text(name));
}

pub(crate) fn dump_member(
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
pub(crate) struct ObjectInventory {
    pub(crate) records: u64,
    pub(crate) resolved_classes: u64,
    pub(crate) identifiers: BTreeSet<u32>,
    pub(crate) identifiers_in_elem_table: BTreeSet<u32>,
    /// Classes taken from format-tag 102 records, which carry the element type.
    pub(crate) element_classes: BTreeMap<u16, u64>,
    /// Category codes read from `ElementHeader` bodies.
    pub(crate) categories: BTreeMap<i32, u64>,
    pub(crate) headers_with_a_category: u64,
    pub(crate) headers_with_a_family: u64,
    pub(crate) element_headers: u64,
    pub(crate) element_bodies: u64,
    pub(crate) anchored_by_pointer_block: u64,
    pub(crate) anchored_by_search: u64,
    pub(crate) with_a_level: u64,
}

pub(crate) fn inspect(
    path: &Path,
    streams_only: bool,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
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

pub(crate) fn print_object_inventory(
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
