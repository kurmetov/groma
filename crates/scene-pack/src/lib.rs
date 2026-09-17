#![forbid(unsafe_code)]
// A packer converts between counts, offsets and coordinates on nearly every
// line. Each cast below is bounded: a length by the buffer it measures, a
// coordinate by the extent it was clamped into.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
//! The binary scene a viewer loads: one file holding a model's elements, the
//! triangles they tessellate to, and their properties.
//!
//! # Why a format rather than JSON
//!
//! `rivet export-json --full` states the same model as text and costs 794 MB
//! on one 800 000-element file. Nearly all of it is geometry written as
//! decimal, which a browser then has to parse before it can draw anything.
//! This format writes geometry as the buffers a GPU already wants, so loading
//! it is a copy rather than a parse.
//!
//! # Layout
//!
//! ```text
//! "RIVETSCN"  u32 version  u32 reserved
//! ... sections, in any order, located by the manifest ...
//! manifest, raw-deflate JSON
//! u64 manifest offset   u32 stored length   u32 length   "RIVETEND"
//! ```
//!
//! The trailer is last so the writer never has to seek: a converter streams
//! chunks out as it builds them and states where they went afterwards. A
//! reader takes the last 24 bytes, then the manifest, then whichever sections
//! it needs - over HTTP that is two range requests before the first triangle.
//!
//! Every section is raw-deflate, which browsers decompress natively through
//! `DecompressionStream("deflate-raw")`, so a reader needs no library.

mod chunk;
mod manifest;

use std::io::{self, Write};

use bim_core::BimModel;
use bim_mesh::MeshOptions;

pub use chunk::{CHUNK_MAGIC, ChunkBuilder, encode_octahedral};
pub use manifest::Span;

/// Chunks deflated together before being written.
///
/// Bounds what is held uncompressed at once: a chunk of the default budget is
/// a few megabytes, so this is tens of megabytes, and it is enough parallelism
/// that compression stops being the cost of writing a scene.
const CHUNK_FLUSH_BATCH: usize = 8;

/// Elements tessellated in one batch.
///
/// Large enough to keep every core busy on a model of any size, small enough
/// that the meshes held at once stay a fraction of the scene.
const TESSELLATION_BATCH: usize = 1024;

/// `RIVETSCN`, the eight bytes every scene begins with.
pub const MAGIC: &[u8; 8] = b"RIVETSCN";
/// `RIVETEND`, the eight bytes every scene ends with.
pub const TRAILER_MAGIC: &[u8; 8] = b"RIVETEND";
/// The version a reader must understand to read a scene written by this crate.
pub const VERSION: u32 = 1;
/// Bytes at the end of the file: offset, stored length, length, magic.
pub const TRAILER_BYTES: usize = 24;

/// Where the model came from, carried into the manifest so a viewer can say
/// what it is showing.
#[derive(Clone, Debug, Default)]
pub struct SourceInfo {
    /// The file the model was read from, as the caller wishes it named.
    pub name: String,
    /// `rvt` or `ifc`.
    pub kind: String,
    pub application: Option<String>,
    pub release: Option<String>,
}

/// How to build a scene.
#[derive(Clone, Copy, Debug)]
pub struct PackOptions {
    pub mesh: MeshOptions,
    /// A chunk is closed once it holds this many triangles. Smaller chunks
    /// cull and stream better; larger ones cost fewer draw calls.
    pub chunk_triangle_budget: usize,
    /// How many elements share one block of properties. A viewer fetches a
    /// whole block to show one element, so this trades requests for bytes.
    pub property_block: usize,
    /// Compression effort, 0 to 9.
    pub compression: u32,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            mesh: MeshOptions::default(),
            chunk_triangle_budget: 250_000,
            property_block: 128,
            compression: 6,
        }
    }
}

/// What a conversion produced, for the caller to report.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub elements: usize,
    pub elements_with_geometry: usize,
    pub vertices: usize,
    pub triangles: usize,
    pub edges: usize,
    pub chunks: usize,
    /// Faces the source declared that the tessellator could not read.
    pub skipped_faces: usize,
    pub bytes: u64,
}

/// A writer that remembers how far it has written, so sections can be located
/// without seeking.
struct Counting<W: Write> {
    inner: W,
    at: u64,
}

impl<W: Write> Counting<W> {
    fn new(inner: W) -> Self {
        Self { inner, at: 0 }
    }

    /// Write one raw-deflate section and return where it went.
    fn section(&mut self, bytes: &[u8], compression: u32) -> io::Result<manifest::Span> {
        let stored = deflate(bytes, compression)?;
        self.stored_section(&stored, bytes.len())
    }

    /// Write a section already compressed, for a caller that deflated it off
    /// this thread, and return where it went.
    fn stored_section(&mut self, stored: &[u8], length: usize) -> io::Result<manifest::Span> {
        let offset = self.at;
        self.write_all(stored)?;
        Ok(manifest::Span {
            offset,
            stored: stored.len() as u64,
            length: length as u64,
        })
    }
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.at += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn deflate(bytes: &[u8], compression: u32) -> io::Result<Vec<u8>> {
    let mut encoder = flate2::write::DeflateEncoder::new(
        Vec::with_capacity(bytes.len() / 3),
        flate2::Compression::new(compression),
    );
    encoder.write_all(bytes)?;
    encoder.finish()
}

/// Write `model` as a scene.
///
/// # Errors
///
/// Fails only where the underlying writer does.
pub fn write_scene<W: Write>(
    model: &BimModel,
    source: &SourceInfo,
    options: &PackOptions,
    output: W,
) -> io::Result<Stats> {
    let mut out = Counting::new(output);
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&0_u32.to_le_bytes())?;

    let mut builder = manifest::Builder::new(model, source);
    let mut stats = Stats {
        elements: model.elements.len(),
        ..Stats::default()
    };

    // Elements are visited in the order their geometry sits in space, so each
    // chunk covers a small extent: that is what keeps a sixteen-bit position
    // exact to a fraction of a millimetre and lets a viewer cull whole chunks.
    let order = spatial_order(model);
    let mut chunk = ChunkBuilder::new();
    // Chunks that are full but not yet written. They are deflated together, on
    // every core, and then written in the order they closed - which is the
    // order they were written in before.
    let mut closed: Vec<ClosedChunk> = Vec::new();
    let mut element_bounds = vec![f32::NAN; model.elements.len() * 6];
    // Tessellated a batch at a time on every core, then pushed into chunks in
    // the spatial order above. One element's triangles do not depend on
    // another's, so this only moves where the work happens; the batch bounds
    // how many meshes are held at once, and the order the chunks are built in
    // is the order they were built in before.
    for batch in order.chunks(TESSELLATION_BATCH) {
        let meshes = bim_core::work::map_in_order(batch, |element_index| {
            model.elements[*element_index]
                .geometry
                .as_ref()
                .map(|geometry| bim_mesh::tessellate(geometry, &options.mesh))
        });
        for (element_index, mesh) in batch.iter().copied().zip(meshes) {
            let Some(mesh) = mesh else {
                continue;
            };
            stats.skipped_faces += mesh.skipped_faces;
            if mesh.is_empty() {
                continue;
            }
            if let Some((min, max)) = mesh.bounds() {
                for axis in 0..3 {
                    element_bounds[element_index * 6 + axis] = min[axis] as f32;
                    element_bounds[element_index * 6 + 3 + axis] = max[axis] as f32;
                }
            }
            stats.elements_with_geometry += 1;
            stats.vertices += mesh.vertex_count();
            stats.triangles += mesh.triangle_count();
            stats.edges += mesh.edge_indices.len() / 2;
            chunk.push(u32::try_from(element_index).unwrap_or(u32::MAX), &mesh);
            builder.mark_geometry(element_index);
            if chunk.triangle_count() >= options.chunk_triangle_budget {
                closed.push(close_chunk(&mut chunk));
                if closed.len() >= CHUNK_FLUSH_BATCH {
                    flush_chunks(&mut out, &mut builder, &mut closed, options, &mut stats)?;
                }
            }
        }
    }
    if !chunk.is_empty() {
        closed.push(close_chunk(&mut chunk));
    }
    flush_chunks(&mut out, &mut builder, &mut closed, options, &mut stats)?;

    let mut bounds_bytes = Vec::with_capacity(element_bounds.len() * 4);
    for value in &element_bounds {
        bounds_bytes.extend_from_slice(&value.to_le_bytes());
    }
    let bounds_span = out.section(&bounds_bytes, options.compression)?;
    builder.set_element_bounds(bounds_span);

    // Each block states its own elements and is compressed on its own, so the
    // blocks are built and deflated on every core and then written in order.
    let blocks = model
        .elements
        .chunks(options.property_block)
        .collect::<Vec<_>>();
    let stored_blocks = bim_core::work::map_in_order(&blocks, |block| {
        let json = manifest::property_block(block);
        deflate(json.as_bytes(), options.compression).map(|stored| (json.len(), stored))
    });
    for stored in stored_blocks {
        let (length, stored) = stored?;
        builder.push_property_block(out.stored_section(&stored, length)?);
    }
    builder.set_property_block_size(options.property_block);

    let manifest = builder.finish(&stats).to_string();
    let stored = deflate(manifest.as_bytes(), options.compression)?;
    let manifest_offset = out.at;
    out.write_all(&stored)?;
    out.write_all(&manifest_offset.to_le_bytes())?;
    out.write_all(&(stored.len() as u32).to_le_bytes())?;
    out.write_all(&(manifest.len() as u32).to_le_bytes())?;
    out.write_all(TRAILER_MAGIC)?;
    out.flush()?;
    stats.bytes = out.at;
    Ok(stats)
}

/// A chunk that has taken its last element: its bytes, and the builder the
/// manifest reads its extent and element list from.
struct ClosedChunk {
    chunk: ChunkBuilder,
    bytes: Vec<u8>,
}

/// Take the chunk's bytes and start a new one.
fn close_chunk(chunk: &mut ChunkBuilder) -> ClosedChunk {
    let chunk = std::mem::replace(chunk, ChunkBuilder::new());
    let bytes = chunk.finish();
    ClosedChunk { chunk, bytes }
}

/// Deflate the closed chunks on every core and write them in the order they
/// closed.
///
/// Deflating is most of what writing a scene costs - on a three-million-
/// triangle model it was four fifths of the pack stage - and one chunk's
/// compression says nothing about another's, so the only thing that has to
/// stay in order is the writing.
fn flush_chunks<W: Write>(
    out: &mut Counting<W>,
    builder: &mut manifest::Builder,
    closed: &mut Vec<ClosedChunk>,
    options: &PackOptions,
    stats: &mut Stats,
) -> io::Result<()> {
    if closed.is_empty() {
        return Ok(());
    }
    let stored =
        bim_core::work::map_in_order(closed, |closed| deflate(&closed.bytes, options.compression));
    for (closed, stored) in closed.iter().zip(stored) {
        let span = out.stored_section(&stored?, closed.bytes.len())?;
        builder.push_chunk(span, &closed.chunk);
        stats.chunks += 1;
    }
    closed.clear();
    Ok(())
}

/// Element indices ordered along a Morton curve through the model's extent,
/// with the elements carrying no geometry left out.
fn spatial_order(model: &BimModel) -> Vec<usize> {
    let mut placed: Vec<(u64, usize)> = Vec::with_capacity(model.elements.len());
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut centres: Vec<(usize, [f64; 3])> = Vec::with_capacity(model.elements.len());
    for (index, element) in model.elements.iter().enumerate() {
        let Some(geometry) = element.geometry.as_ref() else {
            continue;
        };
        let Some((low, high)) = bim_mesh::approximate_bounds(geometry) else {
            continue;
        };
        let centre = [
            f64::midpoint(low[0], high[0]),
            f64::midpoint(low[1], high[1]),
            f64::midpoint(low[2], high[2]),
        ];
        for axis in 0..3 {
            min[axis] = min[axis].min(low[axis]);
            max[axis] = max[axis].max(high[axis]);
        }
        centres.push((index, centre));
    }
    for (index, centre) in centres {
        let mut key = 0_u64;
        for axis in 0..3 {
            let span = (max[axis] - min[axis]).max(f64::MIN_POSITIVE);
            let ratio = ((centre[axis] - min[axis]) / span).clamp(0.0, 1.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // The ratio is clamped into [0, 1] before it is scaled.
            let cell = (ratio * f64::from(u32::from(u16::MAX))) as u32 & 0x1f_ffff;
            key |= interleave(u64::from(cell)) << axis;
        }
        placed.push((key, index));
    }
    placed.sort_unstable();
    placed.into_iter().map(|(_, index)| index).collect()
}

/// Spread the low 21 bits of `value` out so every third bit is one of them.
fn interleave(value: u64) -> u64 {
    let mut spread = value & 0x1f_ffff;
    spread = (spread | spread << 32) & 0x001f_0000_0000_ffff;
    spread = (spread | spread << 16) & 0x001f_0000_ff00_00ff;
    spread = (spread | spread << 8) & 0x100f_00f0_0f00_f00f;
    spread = (spread | spread << 4) & 0x10c3_0c30_c30c_30c3;
    spread = (spread | spread << 2) & 0x1249_2492_4924_9249;
    spread
}

#[cfg(test)]
mod tests {
    use super::{MAGIC, PackOptions, SourceInfo, TRAILER_MAGIC, interleave, write_scene};
    use bim_core::{
        BimBoundingBox, BimElement, BimElementId, BimElementType, BimGeometry, BimModel, BimPoint3,
        BimProperty, BimPropertyValue, BimUnit,
    };

    fn metres() -> BimUnit {
        BimUnit::new(bim_mesh::METRES, "Meters")
    }

    fn at(coordinates: [f64; 3]) -> BimPoint3 {
        BimPoint3 {
            coordinates,
            unit: metres(),
        }
    }

    fn boxy(id: &str, origin: [f64; 3]) -> BimElement {
        BimElement {
            id: BimElementId(id.to_owned()),
            document: None,
            authored_uuid: None,
            element_type: BimElementType::Wall,
            class_name: Some("SWall".to_owned()),
            name: Some(format!("Wall {id}")),
            long_name: None,
            category: None,
            level_id: None,
            type_id: None,
            type_name: None,
            host_id: None,
            placement: None,
            geometry: Some(BimGeometry::BoundingBox(BimBoundingBox {
                min: at(origin),
                max: at([origin[0] + 1.0, origin[1] + 1.0, origin[2] + 1.0]),
            })),
            properties: vec![BimProperty {
                id: None,
                name: "Comment".to_owned(),
                specification: None,
                value: BimPropertyValue::Text("read only".to_owned()),
            }],
            type_properties: Vec::new(),
            material_layers: None,
        }
    }

    #[test]
    fn a_scene_opens_and_closes_with_its_own_magic() {
        let model = BimModel {
            elements: vec![boxy("1", [0.0; 3]), boxy("2", [10.0, 0.0, 0.0])],
            ..BimModel::default()
        };
        let mut bytes = Vec::new();
        let stats = write_scene(
            &model,
            &SourceInfo {
                name: "test".to_owned(),
                kind: "rvt".to_owned(),
                ..SourceInfo::default()
            },
            &PackOptions::default(),
            &mut bytes,
        )
        .expect("in-memory writer");
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(&bytes[bytes.len() - 8..], TRAILER_MAGIC);
        assert_eq!(stats.elements, 2);
        assert_eq!(stats.elements_with_geometry, 2);
        assert_eq!(stats.triangles, 24);
        assert_eq!(stats.chunks, 1);
        assert_eq!(stats.bytes, bytes.len() as u64);
    }

    #[test]
    fn the_trailer_locates_a_manifest_that_reads_back() {
        let model = BimModel {
            elements: vec![boxy("77", [0.0; 3])],
            ..BimModel::default()
        };
        let mut bytes = Vec::new();
        write_scene(
            &model,
            &SourceInfo {
                name: "test".to_owned(),
                kind: "rvt".to_owned(),
                ..SourceInfo::default()
            },
            &PackOptions::default(),
            &mut bytes,
        )
        .expect("in-memory writer");
        let tail = bytes.len() - super::TRAILER_BYTES;
        let offset = u64::from_le_bytes(bytes[tail..tail + 8].try_into().expect("eight bytes"));
        let stored = u32::from_le_bytes(bytes[tail + 8..tail + 12].try_into().expect("four bytes"));
        let manifest = inflate(&bytes[offset as usize..offset as usize + stored as usize]);
        let parsed: serde_json::Value =
            serde_json::from_slice(&manifest).expect("the manifest is JSON");
        assert_eq!(parsed["format"], "rivet-scene");
        assert_eq!(parsed["elements"]["ids"][0], "77");
        assert_eq!(parsed["chunks"].as_array().expect("chunks").len(), 1);
    }

    #[test]
    fn an_ifc_scene_keeps_the_exact_source_entity_class() {
        let mut element = boxy("77", [0.0; 3]);
        // Beam has no dedicated canonical kind. Folding it through that kind
        // would call it IFCBUILDINGELEMENTPROXY and make the viewer's only
        // useful IFC filter lie about the source file.
        element.class_name = Some("IFCBEAM".to_owned());
        element.element_type = BimElementType::Unknown;
        let model = BimModel {
            elements: vec![element],
            ..BimModel::default()
        };
        let mut bytes = Vec::new();
        write_scene(
            &model,
            &SourceInfo {
                name: "beam.ifc".to_owned(),
                kind: "ifc".to_owned(),
                ..SourceInfo::default()
            },
            &PackOptions::default(),
            &mut bytes,
        )
        .expect("in-memory writer");
        let tail = bytes.len() - super::TRAILER_BYTES;
        let offset = u64::from_le_bytes(bytes[tail..tail + 8].try_into().expect("eight bytes"));
        let stored = u32::from_le_bytes(bytes[tail + 8..tail + 12].try_into().expect("four bytes"));
        let manifest = inflate(&bytes[offset as usize..offset as usize + stored as usize]);
        let parsed: serde_json::Value = serde_json::from_slice(&manifest).expect("manifest JSON");
        assert_eq!(parsed["ifcClasses"][0]["name"], "IFCBEAM");
        assert_eq!(parsed["elements"]["ifcClasses"][0], 0);
    }

    fn inflate(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut decoder = flate2::write::DeflateDecoder::new(Vec::new());
        decoder.write_all(bytes).expect("valid deflate");
        decoder.finish().expect("valid deflate")
    }

    #[test]
    fn interleaving_spreads_bits_three_apart() {
        assert_eq!(interleave(0b1), 0b1);
        assert_eq!(interleave(0b11), 0b1001);
        assert_eq!(interleave(0b101), 0b1_000_001);
    }
}
