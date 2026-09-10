//! Reading a source file into the canonical model, and timing the read.
//!
//! This is where a conversion decides what a file is and hands it to the
//! reader for that format. Everything above it - the exports - works from a
//! [`Conversion`] and never asks which format produced it.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt::Write as _,
    io::{self, Write},
    path::{Path, PathBuf},
};

use bim_convert::Format;
use bim_core::{BimDocument, BimDocumentId, BimFederationReport, BimModel};
// The whole semantic reconstruction moved into `rvt-import`. Glob-imported
// because the probe commands below read the same intermediate the pipeline
// builds, and naming each item here would be a second list to keep in step.
#[allow(clippy::wildcard_imports)]
// Sibling modules of one binary; naming each item would be a second list to keep in step.
use crate::inspect::*;
use rvt_import::{
    BOX_GAP_BUCKETS, GeometryStatistics, geometry_statistics,
    mapped_family_instance_placement_counts, metadata_model, recover_elements,
};
use scene_pack::SourceInfo;

#[allow(clippy::too_many_arguments)]
/// What the geometry recovery found, which is the same tally whichever export
/// asks for it.
pub(crate) fn report_geometry_recovery(geometry_statistics: &GeometryStatistics) {
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

/// A clock over the conversion's stages.
///
/// With `--progress` each stage announces its beginning and its end on stdout
/// as one JSON object, flushed immediately, so a caller can show the stage
/// that is running as well as the ones that finished. Without it the same
/// numbers are printed as prose at the end, because a person reading a
/// terminal wants the summary and not a log.
pub(crate) struct Stage {
    pub(crate) started: std::time::Instant,
    pub(crate) began: std::time::Instant,
    pub(crate) progress: bool,
    pub(crate) elapsed: Vec<(&'static str, f64)>,
}

impl Stage {
    pub(crate) fn new(progress: bool) -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            began: now,
            progress,
            elapsed: Vec::new(),
        }
    }

    pub(crate) fn announce(&self, value: &str) {
        if self.progress {
            println!("{value}");
            let _ = io::stdout().flush();
        }
    }

    pub(crate) fn begins(&mut self, name: &'static str) {
        self.began = std::time::Instant::now();
        self.announce(&format!(
            "{{\"stage\":\"{name}\",\"event\":\"begin\",\"totalSeconds\":{:.3}}}",
            self.started.elapsed().as_secs_f64()
        ));
    }

    pub(crate) fn finished(&mut self, name: &'static str) {
        let seconds = self.began.elapsed().as_secs_f64();
        self.elapsed.push((name, seconds));
        self.announce(&format!(
            "{{\"stage\":\"{name}\",\"event\":\"end\",\"seconds\":{seconds:.3},\"totalSeconds\":{:.3}}}",
            self.started.elapsed().as_secs_f64()
        ));
    }

    /// The prose summary, one line per stage.
    ///
    /// Stages of the same name are summed rather than listed: a federation
    /// decodes once per source file, and three `Decode` lines answer nobody's
    /// question. What a reader wants is what reading the sources cost.
    pub(crate) fn total(&self) {
        if self.progress {
            return;
        }
        let mut summed: Vec<(&'static str, f64)> = Vec::new();
        for (name, seconds) in &self.elapsed {
            if let Some(entry) = summed.iter_mut().find(|(held, _)| *held == *name) {
                entry.1 += seconds;
            } else {
                summed.push((name, *seconds));
            }
        }
        for (name, seconds) in summed {
            println!("{}: {seconds:.2}s", stage_label(name));
        }
        println!("Total: {:.2}s", self.started.elapsed().as_secs_f64());
    }
}

pub(crate) fn stage_label(name: &str) -> &'static str {
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
pub(crate) struct ReadOptions {
    /// Include categorized records without a recovered level association.
    /// Meaningful only where the source leaves containment to be recovered,
    /// which an IFC does not.
    pub(crate) include_unplaced: bool,
    /// Stop after this many elements, per source file.
    pub(crate) limit: Option<usize>,
    /// Largest decoded RVT member, or largest IFC source text, accepted.
    pub(crate) max_bytes: u64,
    /// The furthest a chord may sit from the curve it approximates, in
    /// metres, where the reader tessellates a curve to build the model.
    pub(crate) chord_tolerance: f64,
}

/// One source file, read into the canonical model.
pub(crate) struct SourceModel {
    pub(crate) format: Format,
    /// The file's own name, which the scene records as its provenance.
    pub(crate) name: String,
    pub(crate) model: BimModel,
    pub(crate) detail: SourceDetail,
}

/// The findings that belong to one reader and have no counterpart in another.
///
/// These are deliberately not flattened into shared counters. "Products whose
/// representation held nothing this reads" and "faces the tessellator could
/// not read" are different facts about different stages, and a single number
/// covering both would say neither - which is what rules 8 and 12 of
/// `BRIEF.md` forbid. Generic code carries this and prints it through the
/// `report_*` methods below; it never interprets it.
pub(crate) enum SourceDetail {
    Rvt(Box<RvtDetail>),
    Ifc(IfcDetail),
}

/// What the record walk recovered and how much of each kind it established.
///
/// Only the RVT reader has any of this: containment, placement and typing are
/// all recovered rather than stated, so how much of each was recovered is
/// itself a result. Boxed into its variant, because the tallies alone are far
/// larger than everything the STEP reader reports.
pub(crate) struct RvtDetail {
    pub(crate) geometry: GeometryStatistics,
    /// Counted here rather than at report time because the join is by element
    /// identifier, and a federated model has qualified every one of them.
    pub(crate) mapped_family_instances: usize,
    pub(crate) mapped_family_instance_placements: usize,
    pub(crate) included_properties: usize,
    pub(crate) included_type_properties: usize,
    pub(crate) omitted_properties: usize,
}

/// What the STEP reader read, and the parts of the file it does not cover.
///
/// An IFC states its own containment, so nothing here is about recovery; it
/// is about what this reader does not yet read.
pub(crate) struct IfcDetail {
    pub(crate) entities: usize,
    pub(crate) skipped: usize,
    pub(crate) read: ifc_import::Read,
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
pub(crate) fn read_source(
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

pub(crate) fn read_rvt_source(
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

pub(crate) fn read_ifc_source(
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
    pub(crate) fn reports_read(&self) -> bool {
        matches!(self, Self::Ifc(_))
    }

    pub(crate) fn reports_model(&self) -> bool {
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

    pub(crate) fn reports_geometry_funnels(&self) -> bool {
        matches!(self, Self::Rvt(_))
    }

    pub(crate) fn reports_properties(&self) -> bool {
        matches!(self, Self::Rvt(_))
    }

    /// What reading the file itself found, printed before anything about the
    /// model built from it.
    pub(crate) fn report_read(&self) {
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
    pub(crate) fn report_model(&self) {
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
    pub(crate) fn report_geometry_funnels(&self) {
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
    pub(crate) fn report_properties(&self) {
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
pub(crate) struct Conversion {
    /// One source's own model, or several federated into one.
    pub(crate) model: BimModel,
    /// One entry per source file, in the order they were given, keeping the
    /// findings only that file's own reader could state.
    pub(crate) sources: Vec<SourceReport>,
    pub(crate) federation: BimFederationReport,
}

/// What reading one source file found, kept after its model has been merged
/// into the conversion's.
pub(crate) struct SourceReport {
    /// The name this file's elements are qualified by in a federated model,
    /// which is what a reader of the report needs to join the two.
    pub(crate) document: BimDocumentId,
    pub(crate) name: String,
    pub(crate) detail: SourceDetail,
}

/// Read every source file and assemble them into one model.
///
/// With one file this is that file's model unchanged, identifiers included.
/// With several it is a federated model: see [`bim_core::federate`] for what
/// that does to identifiers and, just as importantly, what it does not do to
/// coordinates.
pub(crate) fn read_sources(
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

/// Disjoint document pairs named in full before the rest become a count.
const DISJOINT_PAIRS_SHOWN: usize = 5;

/// A short, stable name for one source file inside a federated model.
///
/// The file's own stem, because that is what a person reading a federated
/// model recognises, with a counter appended where two files in one set share
/// it. The identifier prefixes every element id in the output, so it has to
/// be both readable and unique.
pub(crate) fn document_id(path: &Path, taken: &mut BTreeSet<String>) -> String {
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
    pub(crate) fn info(&self) -> SourceInfo {
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
    pub(crate) fn report_read(&self) {
        self.per_source(SourceDetail::reports_read, SourceDetail::report_read);
    }

    /// What the model does and does not carry, per source.
    pub(crate) fn report_model(&self) {
        self.per_source(SourceDetail::reports_model, SourceDetail::report_model);
    }

    /// The recovery funnels `export-ifc` reports.
    pub(crate) fn report_geometry_funnels(&self) {
        self.per_source(
            SourceDetail::reports_geometry_funnels,
            SourceDetail::report_geometry_funnels,
        );
    }

    /// The properties the readers recovered, where recovering them was their
    /// job.
    pub(crate) fn report_properties(&self) {
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
    pub(crate) fn per_source(
        &self,
        has: impl Fn(&SourceDetail) -> bool,
        report: impl Fn(&SourceDetail),
    ) {
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
    pub(crate) fn report_federation(&self) {
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
        // Every pair is checked, so a set where nothing shares a frame yields
        // one warning per pair - 45 of them for ten documents. A handful says
        // the same thing; the rest is a count.
        for (left, right) in self.federation.disjoint.iter().take(DISJOINT_PAIRS_SHOWN) {
            // Reported and not corrected: nothing here knows the transform
            // that would reconcile two origins. See `BimFederationReport`.
            println!(
                "warning: {} and {} state no overlapping geometry, so they are probably about \
                 different origins; nothing was moved",
                left.0, right.0
            );
        }
        if let Some(more) = self
            .federation
            .disjoint
            .len()
            .checked_sub(DISJOINT_PAIRS_SHOWN)
            .filter(|more| *more > 0)
        {
            println!("warning: and {more} further pairs that share no geometry");
        }
    }
}

/// Report the funnel from "an instance names a symbol" to "that body is
/// placed in the world". A shortfall in exported geometry is almost always one
/// of these gates, and reading which one is what stops the next change being a
/// guess: it is how the category-equality gate was found to be costing
/// thousands of geometrically verified links while refusing nothing wrong.
pub(crate) fn report_symbol_link_funnel(statistics: &GeometryStatistics) {
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
pub(crate) fn report_nested_assembly_funnel(statistics: &GeometryStatistics) {
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
