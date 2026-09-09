//! One geometry chunk: the meshes of a spatially adjacent run of elements,
//! encoded as the buffers a GPU wants.
//!
//! A chunk is the unit of transfer, of culling and of quantisation. Positions
//! are stored as sixteen-bit offsets inside the chunk's own extent rather than
//! as floats, which is what makes the quantisation lossless enough to measure
//! against: a chunk holding an eight-metre cell resolves to a eighth of a
//! millimetre, and the chunk states the extent it was quantised in so a reader
//! can say exactly what its numbers mean.

use bim_mesh::Mesh;

/// `RVCH`, little-endian, at the head of every chunk.
pub const CHUNK_MAGIC: u32 = 0x4843_5652;

/// The fixed header in front of a chunk's buffers, in `u32` words.
pub const HEADER_WORDS: usize = 12;

/// Words per entry of the mesh table.
pub const MESH_WORDS: usize = 7;

/// A chunk under construction.
#[derive(Default)]
pub struct ChunkBuilder {
    positions: Vec<f64>,
    normals: Vec<f32>,
    indices: Vec<u32>,
    edge_positions: Vec<f64>,
    edge_indices: Vec<u32>,
    meshes: Vec<MeshEntry>,
    min: [f64; 3],
    max: [f64; 3],
    started: bool,
}

/// Where one element's geometry sits inside the chunk's buffers.
struct MeshEntry {
    element: u32,
    vertex_start: u32,
    vertex_count: u32,
    index_start: u32,
    index_count: u32,
    edge_index_start: u32,
    edge_index_count: u32,
}

impl ChunkBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            min: [f64::INFINITY; 3],
            max: [f64::NEG_INFINITY; 3],
            ..Self::default()
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.meshes.is_empty()
    }

    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.positions.len() / 3
    }

    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edge_indices.len() / 2
    }

    #[must_use]
    pub fn element_count(&self) -> usize {
        self.meshes.len()
    }

    /// The extent the chunk's positions will be quantised in.
    #[must_use]
    pub fn bounds(&self) -> ([f64; 3], [f64; 3]) {
        if self.started {
            (self.min, self.max)
        } else {
            ([0.0; 3], [0.0; 3])
        }
    }

    /// Add one element's mesh. `element` is the index the manifest's element
    /// table gives it, which is what a picked fragment resolves to.
    pub fn push(&mut self, element: u32, mesh: &Mesh) {
        let vertex_start = u32::try_from(self.positions.len() / 3).unwrap_or(u32::MAX);
        let index_start = u32::try_from(self.indices.len()).unwrap_or(u32::MAX);
        let edge_index_start = u32::try_from(self.edge_indices.len()).unwrap_or(u32::MAX);
        let edge_vertex_start = u32::try_from(self.edge_positions.len() / 3).unwrap_or(u32::MAX);

        for at in mesh
            .positions
            .chunks_exact(3)
            .chain(mesh.edge_positions.chunks_exact(3))
        {
            self.started = true;
            for ((low, high), value) in self.min.iter_mut().zip(&mut self.max).zip(at) {
                *low = low.min(*value);
                *high = high.max(*value);
            }
        }

        self.positions.extend_from_slice(&mesh.positions);
        self.normals.extend_from_slice(&mesh.normals);
        self.indices
            .extend(mesh.indices.iter().map(|index| index + vertex_start));
        self.edge_positions.extend_from_slice(&mesh.edge_positions);
        self.edge_indices.extend(
            mesh.edge_indices
                .iter()
                .map(|index| index + edge_vertex_start),
        );

        self.meshes.push(MeshEntry {
            element,
            vertex_start,
            vertex_count: u32::try_from(mesh.positions.len() / 3).unwrap_or(u32::MAX),
            index_start,
            index_count: u32::try_from(mesh.indices.len()).unwrap_or(u32::MAX),
            edge_index_start,
            edge_index_count: u32::try_from(mesh.edge_indices.len()).unwrap_or(u32::MAX),
        });
    }

    /// Encode the chunk. The returned bytes are self-describing given the
    /// extent [`Self::bounds`] reports, which the manifest carries.
    #[must_use]
    pub fn finish(&self) -> Vec<u8> {
        let (min, max) = self.bounds();
        let span = [
            (max[0] - min[0]).max(f64::MIN_POSITIVE),
            (max[1] - min[1]).max(f64::MIN_POSITIVE),
            (max[2] - min[2]).max(f64::MIN_POSITIVE),
        ];

        let vertex_count = self.positions.len() / 3;
        let edge_vertex_count = self.edge_positions.len() / 3;
        // Every buffer starts on a four-byte boundary so a reader can view it
        // without copying.
        let positions_bytes = align4(vertex_count * 6);
        let normals_bytes = align4(vertex_count * 4);
        let indices_bytes = self.indices.len() * 4;
        let edge_positions_bytes = align4(edge_vertex_count * 6);
        let edge_indices_bytes = self.edge_indices.len() * 4;
        let meshes_bytes = self.meshes.len() * MESH_WORDS * 4;

        let header_bytes = HEADER_WORDS * 4;
        let off_positions = header_bytes;
        let off_normals = off_positions + positions_bytes;
        let off_indices = off_normals + normals_bytes;
        let off_edge_positions = off_indices + indices_bytes;
        let off_edge_indices = off_edge_positions + edge_positions_bytes;
        let off_meshes = off_edge_indices + edge_indices_bytes;
        let total = off_meshes + meshes_bytes;

        let mut out = Vec::with_capacity(total);
        for word in [
            CHUNK_MAGIC,
            u32::try_from(vertex_count).unwrap_or(u32::MAX),
            u32::try_from(self.indices.len()).unwrap_or(u32::MAX),
            u32::try_from(edge_vertex_count).unwrap_or(u32::MAX),
            u32::try_from(self.edge_indices.len()).unwrap_or(u32::MAX),
            u32::try_from(self.meshes.len()).unwrap_or(u32::MAX),
            u32::try_from(off_positions).unwrap_or(u32::MAX),
            u32::try_from(off_normals).unwrap_or(u32::MAX),
            u32::try_from(off_indices).unwrap_or(u32::MAX),
            u32::try_from(off_edge_positions).unwrap_or(u32::MAX),
            u32::try_from(off_edge_indices).unwrap_or(u32::MAX),
            u32::try_from(off_meshes).unwrap_or(u32::MAX),
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }

        for at in self.positions.chunks_exact(3) {
            write_quantised(&mut out, at, min, span);
        }
        pad4(&mut out);
        for normal in self.normals.chunks_exact(3) {
            let [u, v] = encode_octahedral([normal[0], normal[1], normal[2]]);
            out.extend_from_slice(&u.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        pad4(&mut out);
        for index in &self.indices {
            out.extend_from_slice(&index.to_le_bytes());
        }
        for at in self.edge_positions.chunks_exact(3) {
            write_quantised(&mut out, at, min, span);
        }
        pad4(&mut out);
        for index in &self.edge_indices {
            out.extend_from_slice(&index.to_le_bytes());
        }
        for mesh in &self.meshes {
            for word in [
                mesh.element,
                mesh.vertex_start,
                mesh.vertex_count,
                mesh.index_start,
                mesh.index_count,
                mesh.edge_index_start,
                mesh.edge_index_count,
            ] {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        out
    }
}

fn align4(bytes: usize) -> usize {
    bytes.div_ceil(4) * 4
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// One position as three sixteen-bit offsets inside the chunk's extent.
fn write_quantised(out: &mut Vec<u8>, at: &[f64], min: [f64; 3], span: [f64; 3]) {
    for axis in 0..3 {
        let ratio = ((at[axis] - min[axis]) / span[axis]).clamp(0.0, 1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // The ratio is clamped into [0, 1], so the product lands in range.
        let value = (ratio * f64::from(u16::MAX)).round() as u16;
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// A unit normal as two sixteen-bit numbers on the octahedron, which holds
/// direction to well under a tenth of a degree at a third of the space three
/// floats would take.
#[must_use]
pub fn encode_octahedral(normal: [f32; 3]) -> [i16; 2] {
    let magnitude = normal[0].abs() + normal[1].abs() + normal[2].abs();
    if magnitude <= f32::EPSILON {
        return [0, 0];
    }
    let mut x = normal[0] / magnitude;
    let mut y = normal[1] / magnitude;
    if normal[2] < 0.0 {
        let (folded_x, folded_y) = (x, y);
        x = (1.0 - folded_y.abs()) * if folded_x >= 0.0 { 1.0 } else { -1.0 };
        y = (1.0 - folded_x.abs()) * if folded_y >= 0.0 { 1.0 } else { -1.0 };
    }
    #[allow(clippy::cast_possible_truncation)]
    // Both components are clamped to [-1, 1] before scaling.
    let encode = |value: f32| (value.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    [encode(x), encode(y)]
}

#[cfg(test)]
mod tests {
    use super::{CHUNK_MAGIC, ChunkBuilder, HEADER_WORDS, encode_octahedral};
    use bim_mesh::Mesh;

    /// The reader's side of [`encode_octahedral`], to check the round trip.
    fn decode_octahedral(encoded: [i16; 2]) -> [f32; 3] {
        let mut x = f32::from(encoded[0]) / 32767.0;
        let mut y = f32::from(encoded[1]) / 32767.0;
        let z = 1.0 - x.abs() - y.abs();
        if z < 0.0 {
            let (folded_x, folded_y) = (x, y);
            x = (1.0 - folded_y.abs()) * if folded_x >= 0.0 { 1.0 } else { -1.0 };
            y = (1.0 - folded_x.abs()) * if folded_y >= 0.0 { 1.0 } else { -1.0 };
        }
        let length = x.mul_add(x, y.mul_add(y, z * z)).sqrt();
        [x / length, y / length, z / length]
    }

    #[test]
    fn a_normal_survives_the_octahedral_round_trip() {
        for normal in [
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
            [1.0, 0.0, 0.0],
            [-0.577_35, 0.577_35, -0.577_35],
            [0.267_26, 0.534_52, 0.801_78],
        ] {
            let back = decode_octahedral(encode_octahedral(normal));
            let error = (0..3)
                .map(|axis| (back[axis] - normal[axis]).abs())
                .fold(0.0_f32, f32::max);
            assert!(error < 1e-3, "{normal:?} came back as {back:?}");
        }
    }

    #[test]
    fn a_chunk_states_the_extent_it_quantised_in() {
        let mut mesh = Mesh::default();
        mesh.positions
            .extend_from_slice(&[0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 3.0, 0.0]);
        mesh.normals.extend_from_slice(&[0.0, 0.0, 1.0].repeat(3));
        mesh.indices.extend_from_slice(&[0, 1, 2]);
        let mut builder = ChunkBuilder::new();
        builder.push(7, &mesh);
        assert_eq!(builder.bounds(), ([0.0; 3], [4.0, 3.0, 0.0]));
        let bytes = builder.finish();
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes")),
            CHUNK_MAGIC
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes")),
            3
        );
        assert!(bytes.len() > HEADER_WORDS * 4);
    }

    #[test]
    fn a_quantised_corner_lands_on_the_extent_it_came_from() {
        let mut mesh = Mesh::default();
        mesh.positions
            .extend_from_slice(&[10.0, 20.0, 30.0, 18.0, 20.0, 30.0, 10.0, 28.0, 30.0]);
        mesh.normals.extend_from_slice(&[0.0, 0.0, 1.0].repeat(3));
        mesh.indices.extend_from_slice(&[0, 1, 2]);
        let mut builder = ChunkBuilder::new();
        builder.push(0, &mesh);
        let bytes = builder.finish();
        let at = HEADER_WORDS * 4;
        let first = u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes"));
        let second = u16::from_le_bytes(bytes[at + 6..at + 8].try_into().expect("two bytes"));
        assert_eq!(first, 0);
        assert_eq!(second, u16::MAX);
    }
}
