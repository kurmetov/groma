/// Serialized marker observed before a built-in straight-line curve. The
/// `04 00` type tag is not taken from `Formats/Latest`; the surrounding
/// structure and field meaning are accepted only through the validations in
/// [`PipeLineGeometryFields::parse`].
const SERIALIZED_LINE_MARKER: [u8; 8] = [0xff, 0xff, 0xff, 0xff, 0x04, 0x00, 0x08, 0x01];
/// Marker observed before the eight `GCurve`/`GLine` doubles embedded in a
/// `FittingCenterLine`. This is deliberately separate from the built-in curve
/// marker used by `RbsPipeCurve` records.
const SERIALIZED_GLINE_MARKER: [u8; 8] = [0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x08, 0x01];
const DIRECTION_TOLERANCE: f64 = 1.0e-8;
const DIMENSION_TOLERANCE: f64 = 1.0e-10;
const BOUNDS_TOLERANCE_FEET: f64 = 1.0e-8;
const GELEMENT_NODE_COUNT_OFFSET: usize = 14;
const GELEMENT_NODE_REFERENCES_OFFSET: usize = 18;
const GELEMENT_NODE_REFERENCE_BYTES: usize = 6;
const MAX_GELEMENT_TOP_LEVEL_NODES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RvtPoint3 {
    /// Revit internal length coordinates (feet).
    pub coordinates_feet: [f64; 3],
}

/// Straight pipe geometry recovered from an `RbsPipeCurve` body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PipeLineGeometryFields {
    /// Offset of the serialized line marker inside the element body.
    pub line_offset: usize,
    /// `RbsCurve.m_dWidthOrDiameter`, in Revit internal feet.
    pub nominal_diameter_feet: f64,
    pub start: RvtPoint3,
    pub end: RvtPoint3,
}

/// One unambiguous straight centerline recovered from a fitting helper.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FittingCenterLineFields {
    /// Element tag stored by the line's `GInfo`; corpus validation associates
    /// it with the owning fitting element.
    pub owner_element_id: u32,
    pub start: RvtPoint3,
    pub end: RvtPoint3,
}

impl FittingCenterLineFields {
    /// Recover a single `GLine` from a `PipeFittingCenterLine` body.
    ///
    /// The collection prefix limits this reader to exactly one center curve.
    /// Elbows, tees, and other multi-curve or non-linear centerlines therefore
    /// remain undecoded instead of being approximated as straight segments.
    #[must_use]
    pub fn parse(body: &[u8], gline_class_index: u16) -> Option<Self> {
        let mut curve_prefix = [0_u8; 10];
        curve_prefix[..8].copy_from_slice(&[1, 0, 0, 0, 3, 0, 0, 0]);
        curve_prefix[8..].copy_from_slice(&gline_class_index.to_le_bytes());
        let mut candidates = marker_offsets(body, &curve_prefix);
        let gline_offset = candidates.next()?.checked_add(8)?;
        if candidates.next().is_some() {
            return None;
        }
        let owner_element_id = read_u32(body, gline_offset + 2)?;
        if owner_element_id == 0 || owner_element_id > i32::MAX as u32 {
            return None;
        }
        let (line_offset, line) = unique_valid_line_with_marker(body, &SERIALIZED_GLINE_MARKER)?;
        if line_offset <= gline_offset {
            return None;
        }
        let start = point_on_line(
            [line[2], line[3], line[4]],
            [line[5], line[6], line[7]],
            line[0],
        )?;
        let end = point_on_line(
            [line[2], line[3], line[4]],
            [line[5], line[6], line[7]],
            line[1],
        )?;
        if squared_norm(array_subtract(end, start)) <= f64::EPSILON {
            return None;
        }
        Some(Self {
            owner_element_id,
            start: RvtPoint3 {
                coordinates_feet: start,
            },
            end: RvtPoint3 {
                coordinates_feet: end,
            },
        })
    }
}

/// Candidate for the three fixed `FamilyInstance` placement fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FamilyInstancePlacementFields {
    pub offset: usize,
    pub origin: RvtPoint3,
    pub reference_direction: [f64; 3],
    pub axis: [f64; 3],
}

impl FamilyInstancePlacementFields {
    /// Find orthonormal `m_instOrigin`/`m_RefDir`/`m_zAxis` candidates after
    /// the variable inherited prefix. Independent element bounds are applied
    /// later because the `GElement` record is stored separately.
    #[must_use]
    pub fn candidates(body: &[u8], search_start: usize) -> Vec<Self> {
        let Some(last_offset) = body.len().checked_sub(9 * 8) else {
            return Vec::new();
        };
        if search_start > last_offset {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        for offset in search_start..=last_offset {
            let Some(values) = read_f64_array::<9>(body, offset) else {
                continue;
            };
            if values.into_iter().any(|value| !value.is_finite()) {
                continue;
            }
            let reference_direction = [values[3], values[4], values[5]];
            let axis = [values[6], values[7], values[8]];
            if (squared_norm(reference_direction) - 1.0).abs() > DIRECTION_TOLERANCE
                || (squared_norm(axis) - 1.0).abs() > DIRECTION_TOLERANCE
                || dot(reference_direction, axis).abs() > DIRECTION_TOLERANCE
            {
                continue;
            }
            candidates.push(Self {
                offset,
                origin: RvtPoint3 {
                    coordinates_feet: [values[0], values[1], values[2]],
                },
                reference_direction,
                axis,
            });
        }
        candidates
    }
}

/// World transform carried by a `GInstance` node in the element's separate
/// geometry record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GInstanceTransformFields {
    pub offset: usize,
    /// Local X/Y/Z basis vectors expressed in source world coordinates.
    pub basis: [[f64; 3]; 3],
    pub origin: RvtPoint3,
    /// Referenced family-symbol element when the instance uses shared symbol
    /// geometry. Embedded or otherwise unresolved geometry leaves this empty.
    pub symbol_element_id: Option<u32>,
}

impl GInstanceTransformFields {
    /// Recover one right-handed rigid `Trf` after a schema-resolved
    /// `GInstance` marker and require its origin to lie in the independently
    /// serialized element bounds.
    #[must_use]
    pub fn parse(body: &[u8], ginstance_class_index: u16, bounds: &GElementBounds) -> Option<Self> {
        let marker = ginstance_class_index.to_le_bytes();
        let marker_offset = marker_offsets(body, &marker).next()?;
        if marker_offsets(body, &marker).nth(1).is_some() {
            return None;
        }
        let start = marker_offset.checked_add(marker.len())?;
        let last_offset = body.len().checked_sub(12 * 8)?;
        let mut found = None;
        for offset in start..=last_offset {
            let Some(values) = read_f64_array::<12>(body, offset) else {
                continue;
            };
            if values.into_iter().any(|value| !value.is_finite()) {
                continue;
            }
            let basis = [
                [values[0], values[1], values[2]],
                [values[3], values[4], values[5]],
                [values[6], values[7], values[8]],
            ];
            if basis
                .into_iter()
                .any(|direction| (squared_norm(direction) - 1.0).abs() > DIRECTION_TOLERANCE)
                || dot(basis[0], basis[1]).abs() > DIRECTION_TOLERANCE
                || dot(basis[0], basis[2]).abs() > DIRECTION_TOLERANCE
                || dot(basis[1], basis[2]).abs() > DIRECTION_TOLERANCE
                || (determinant(basis) - 1.0).abs() > DIRECTION_TOLERANCE
            {
                continue;
            }
            let origin = RvtPoint3 {
                coordinates_feet: [values[9], values[10], values[11]],
            };
            if !bounds.contains_point(origin) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            let raw_symbol_id = read_u32(body, offset.checked_add(12 * 8)?)?;
            let symbol_element_id = (raw_symbol_id > 0 && i32::try_from(raw_symbol_id).is_ok())
                .then_some(raw_symbol_id);
            found = Some(Self {
                offset,
                basis,
                origin,
                symbol_element_id,
            });
        }
        found
    }
}

impl PipeLineGeometryFields {
    /// Recover a straight pipe axis and diameter.
    ///
    /// `curve_driver_class_index` is resolved from `Formats/Latest`. A result
    /// requires one unambiguous positive circular section, one built-in line,
    /// a finite non-empty parameter domain, and a unit line direction.
    #[must_use]
    pub fn parse(body: &[u8], curve_driver_class_index: u16) -> Option<Self> {
        let nominal_diameter_feet = unique_circular_section(body, curve_driver_class_index)?;
        let (line_offset, line) = unique_valid_line(body)?;
        let [
            parameter_start,
            parameter_end,
            origin_x,
            origin_y,
            origin_z,
            direction_x,
            direction_y,
            direction_z,
        ] = line;
        let start = point_on_line(
            [origin_x, origin_y, origin_z],
            [direction_x, direction_y, direction_z],
            parameter_start,
        )?;
        let end = point_on_line(
            [origin_x, origin_y, origin_z],
            [direction_x, direction_y, direction_z],
            parameter_end,
        )?;
        if squared_norm(array_subtract(end, start)) <= f64::EPSILON {
            return None;
        }
        Some(Self {
            line_offset,
            nominal_diameter_feet,
            start: RvtPoint3 {
                coordinates_feet: start,
            },
            end: RvtPoint3 {
                coordinates_feet: end,
            },
        })
    }

    /// Cross-check the axis and radius against a separately serialized
    /// `GElement` axis-aligned bounding box.
    #[must_use]
    pub fn matches_bounds(&self, bounds: &GElementBounds) -> bool {
        self.swept_radius_feet(bounds).is_some()
    }

    /// Derive the geometric outer radius from the independently serialized
    /// bounds. Revit's width/diameter field may be a nominal diameter (for
    /// example 40 mm while the physical outside diameter is 48 mm), so it is
    /// retained but not substituted for the geometric radius.
    #[must_use]
    pub fn swept_radius_feet(&self, bounds: &GElementBounds) -> Option<f64> {
        let delta = array_subtract(self.end.coordinates_feet, self.start.coordinates_feet);
        let length = squared_norm(delta).sqrt();
        if !length.is_finite() || length <= 0.0 {
            return None;
        }
        let direction = delta.map(|value| value / length);
        let bounds_span = array_subtract(bounds.max, bounds.min);
        let mut radii = Vec::with_capacity(3);
        for axis in 0..3 {
            let radial_factor = (1.0 - direction[axis] * direction[axis]).max(0.0).sqrt();
            if radial_factor <= DIRECTION_TOLERANCE {
                continue;
            }
            let radius = (bounds_span[axis] - delta[axis].abs()) / (2.0 * radial_factor);
            if !radius.is_finite() || radius <= 0.0 {
                return None;
            }
            radii.push(radius);
        }
        let divisor = match radii.len() {
            2 => 2.0,
            3 => 3.0,
            _ => return None,
        };
        let radius = radii.iter().sum::<f64>() / divisor;
        if radii
            .into_iter()
            .any(|candidate| (candidate - radius).abs() > BOUNDS_TOLERANCE_FEET)
        {
            return None;
        }
        let mut expected_min = [0.0; 3];
        let mut expected_max = [0.0; 3];
        for axis in 0..3 {
            let radial_extent = radius * (1.0 - direction[axis] * direction[axis]).max(0.0).sqrt();
            expected_min[axis] = self.start.coordinates_feet[axis]
                .min(self.end.coordinates_feet[axis])
                - radial_extent;
            expected_max[axis] = self.start.coordinates_feet[axis]
                .max(self.end.coordinates_feet[axis])
                + radial_extent;
        }
        let matches = expected_min
            .into_iter()
            .chain(expected_max)
            .zip(bounds.min.into_iter().chain(bounds.max))
            .all(|(expected, actual)| (expected - actual).abs() <= BOUNDS_TOLERANCE_FEET);
        matches.then_some(radius)
    }
}

/// Duplicated six-coordinate bounds block carried by a `GElement` record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GElementBounds {
    pub offset: usize,
    /// Minimum XYZ in Revit internal feet.
    pub min: [f64; 3],
    /// Maximum XYZ in Revit internal feet.
    pub max: [f64; 3],
}

/// One dynamic top-level node reference in `GElement`'s inherited
/// `GGroup.m_subNodes` collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GElementNodeReference {
    pub object_id: u32,
    pub class_index: u16,
}

/// Structurally located `GElement` graph header and its local or world bounds.
#[derive(Clone, Debug, PartialEq)]
pub struct GElementGraphFields {
    pub top_level_nodes: Vec<GElementNodeReference>,
    pub bounds: GElementBounds,
}

impl GElementGraphFields {
    /// Decode the leading `GGroup.m_subNodes` reference array and the two
    /// immediately following `GRep` bounds blocks. The caller resolves node
    /// classes against the active schema and accepts only `GNode` subclasses.
    #[must_use]
    pub fn parse(body: &[u8], mut accepts_node_class: impl FnMut(u16) -> bool) -> Option<Self> {
        let node_count = usize::try_from(read_u32(body, GELEMENT_NODE_COUNT_OFFSET)?).ok()?;
        if node_count > MAX_GELEMENT_TOP_LEVEL_NODES {
            return None;
        }
        let bounds_offset = GELEMENT_NODE_REFERENCES_OFFSET
            .checked_add(node_count.checked_mul(GELEMENT_NODE_REFERENCE_BYTES)?)?;
        let mut top_level_nodes = Vec::with_capacity(node_count);
        for index in 0..node_count {
            let offset = GELEMENT_NODE_REFERENCES_OFFSET
                .checked_add(index.checked_mul(GELEMENT_NODE_REFERENCE_BYTES)?)?;
            let object_id = read_u32(body, offset)?;
            let class_index = read_u16(body, offset.checked_add(4)?)?;
            if object_id == 0
                || !accepts_node_class(class_index)
                || top_level_nodes
                    .iter()
                    .any(|node: &GElementNodeReference| node.object_id == object_id)
            {
                return None;
            }
            top_level_nodes.push(GElementNodeReference {
                object_id,
                class_index,
            });
        }
        Some(Self {
            top_level_nodes,
            bounds: GElementBounds::parse_adjacent_at(body, bounds_offset, BOUNDS_TOLERANCE_FEET)?,
        })
    }
}

impl GElementBounds {
    /// Find exactly one adjacent pair of identical six-`f64` bounds blocks.
    #[must_use]
    pub fn parse(body: &[u8]) -> Option<Self> {
        const BLOCK_BYTES: usize = 6 * 8;
        const DUPLICATED_BYTES: usize = 2 * BLOCK_BYTES;

        let mut found = None;
        for offset in 0..=body.len().checked_sub(DUPLICATED_BYTES)? {
            let first = body.get(offset..offset + BLOCK_BYTES)?;
            let second = body.get(offset + BLOCK_BYTES..offset + DUPLICATED_BYTES)?;
            if first != second {
                continue;
            }
            let values = read_f64_array::<6>(first, 0)?;
            if values.into_iter().any(|value| !value.is_finite()) {
                continue;
            }
            let min = [values[0], values[1], values[2]];
            let max = [values[3], values[4], values[5]];
            if min.into_iter().zip(max).any(|(low, high)| low > high)
                || !min.into_iter().zip(max).any(|(low, high)| low < high)
            {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(Self { offset, min, max });
        }
        found
    }

    /// Find one adjacent pair of numerically equal bounds blocks. Some
    /// `FamilyInstance` records serialize the two copies with sub-tolerance
    /// floating-point differences, so placement validation uses this reader
    /// while swept-body validation continues to require [`Self::parse`].
    #[must_use]
    pub fn parse_near_duplicate(body: &[u8]) -> Option<Self> {
        const BLOCK_BYTES: usize = 6 * 8;
        const DUPLICATED_BYTES: usize = 2 * BLOCK_BYTES;

        let mut found = None;
        for offset in (0..=body.len().checked_sub(DUPLICATED_BYTES)?).step_by(8) {
            let first = read_f64_array::<6>(body, offset)?;
            let second = read_f64_array::<6>(body, offset + BLOCK_BYTES)?;
            if first.into_iter().zip(second).any(|(left, right)| {
                !left.is_finite() || (left - right).abs() > BOUNDS_TOLERANCE_FEET
            }) {
                continue;
            }
            let min = [
                first[0].min(second[0]),
                first[1].min(second[1]),
                first[2].min(second[2]),
            ];
            let max = [
                first[3].max(second[3]),
                first[4].max(second[4]),
                first[5].max(second[5]),
            ];
            if min.into_iter().zip(max).any(|(low, high)| low > high)
                || !min
                    .into_iter()
                    .zip(max)
                    .any(|(low, high)| high - low > BOUNDS_TOLERANCE_FEET)
            {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(Self { offset, min, max });
        }
        found
    }

    /// Check that both endpoints of a source line lie inside these independent
    /// element bounds. This verifies association, but does not claim that the
    /// bounds describe a swept body.
    #[must_use]
    pub fn contains_line_segment(&self, start: RvtPoint3, end: RvtPoint3) -> bool {
        self.contains_point(start) && self.contains_point(end)
    }

    #[must_use]
    pub fn contains_point(&self, point: RvtPoint3) -> bool {
        point
            .coordinates_feet
            .into_iter()
            .enumerate()
            .all(|(axis, value)| {
                value.is_finite()
                    && value >= self.min[axis] - BOUNDS_TOLERANCE_FEET
                    && value <= self.max[axis] + BOUNDS_TOLERANCE_FEET
            })
    }

    /// Report whether the box has a strictly positive extent on every axis.
    /// A box that is flat on one axis is a valid extent for containment tests
    /// but not a solid volume, so it must not become a Box representation.
    #[must_use]
    pub fn is_volumetric(&self) -> bool {
        self.min
            .into_iter()
            .zip(self.max)
            .all(|(low, high)| high - low > BOUNDS_TOLERANCE_FEET)
    }

    /// Check that transforming every corner of `local` by the verified rigid
    /// instance transform reproduces this world-axis-aligned box.
    #[must_use]
    pub fn matches_transformed(&self, local: &Self, transform: &GInstanceTransformFields) -> bool {
        let mut transformed_min = [f64::INFINITY; 3];
        let mut transformed_max = [f64::NEG_INFINITY; 3];
        for corner in 0_u8..8 {
            let local_point = [
                if corner & 1 == 0 {
                    local.min[0]
                } else {
                    local.max[0]
                },
                if corner & 2 == 0 {
                    local.min[1]
                } else {
                    local.max[1]
                },
                if corner & 4 == 0 {
                    local.min[2]
                } else {
                    local.max[2]
                },
            ];
            let mut world = transform.origin.coordinates_feet;
            for (local_axis, coordinate) in local_point.into_iter().enumerate() {
                for (world_axis, value) in world.iter_mut().enumerate() {
                    *value += transform.basis[local_axis][world_axis] * coordinate;
                }
            }
            for (axis, value) in world.into_iter().enumerate() {
                transformed_min[axis] = transformed_min[axis].min(value);
                transformed_max[axis] = transformed_max[axis].max(value);
            }
        }
        transformed_min
            .into_iter()
            .chain(transformed_max)
            .zip(self.min.into_iter().chain(self.max))
            .all(|(expected, actual)| {
                expected.is_finite()
                    && actual.is_finite()
                    && (expected - actual).abs() <= BOUNDS_TOLERANCE_FEET
            })
    }

    fn parse_adjacent_at(body: &[u8], offset: usize, tolerance: f64) -> Option<Self> {
        const BLOCK_BYTES: usize = 6 * 8;
        let first = read_f64_array::<6>(body, offset)?;
        let second = read_f64_array::<6>(body, offset.checked_add(BLOCK_BYTES)?)?;
        if first
            .into_iter()
            .zip(second)
            .any(|(left, right)| !left.is_finite() || (left - right).abs() > tolerance)
        {
            return None;
        }
        let min = [
            first[0].min(second[0]),
            first[1].min(second[1]),
            first[2].min(second[2]),
        ];
        let max = [
            first[3].max(second[3]),
            first[4].max(second[4]),
            first[5].max(second[5]),
        ];
        if min.into_iter().zip(max).any(|(low, high)| low > high)
            || !min
                .into_iter()
                .zip(max)
                .any(|(low, high)| high - low > tolerance)
        {
            return None;
        }
        Some(Self { offset, min, max })
    }
}

fn unique_circular_section(body: &[u8], curve_driver_class_index: u16) -> Option<f64> {
    let marker = [
        0xff,
        0xff,
        0xff,
        0xff,
        curve_driver_class_index.to_le_bytes()[0],
        curve_driver_class_index.to_le_bytes()[1],
    ];
    let mut found = None;
    for offset in marker_offsets(body, &marker) {
        let Some([width, height]) = read_f64_array::<2>(body, offset + marker.len()) else {
            continue;
        };
        if !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || (width - height).abs() > DIMENSION_TOLERANCE
        {
            continue;
        }
        if found.replace(width).is_some() {
            return None;
        }
    }
    found
}

fn unique_valid_line(body: &[u8]) -> Option<(usize, [f64; 8])> {
    unique_valid_line_with_marker(body, &SERIALIZED_LINE_MARKER)
}

fn unique_valid_line_with_marker(body: &[u8], marker: &[u8]) -> Option<(usize, [f64; 8])> {
    let mut found = None;
    for offset in marker_offsets(body, marker) {
        let Some(values) = read_f64_array::<8>(body, offset + marker.len()) else {
            continue;
        };
        if values.into_iter().any(|value| !value.is_finite())
            || (values[0] - values[1]).abs() <= f64::EPSILON
        {
            continue;
        }
        let direction = [values[5], values[6], values[7]];
        if (squared_norm(direction) - 1.0).abs() > DIRECTION_TOLERANCE {
            continue;
        }
        if found.replace((offset, values)).is_some() {
            return None;
        }
    }
    found
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn marker_offsets<'a>(body: &'a [u8], marker: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    body.windows(marker.len())
        .enumerate()
        .filter_map(move |(offset, candidate)| (candidate == marker).then_some(offset))
}

fn read_f64_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[f64; N]> {
    let mut values = [0.0; N];
    for (index, value) in values.iter_mut().enumerate() {
        let at = offset.checked_add(index.checked_mul(8)?)?;
        *value = f64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
    }
    Some(values)
}

fn point_on_line(origin: [f64; 3], direction: [f64; 3], parameter: f64) -> Option<[f64; 3]> {
    let point = [
        origin[0] + parameter * direction[0],
        origin[1] + parameter * direction[1],
        origin[2] + parameter * direction[2],
    ];
    point.into_iter().all(f64::is_finite).then_some(point)
}

fn array_subtract(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn squared_norm(vector: [f64; 3]) -> f64 {
    vector.into_iter().map(|value| value * value).sum()
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}

fn determinant(matrix: [[f64; 3]; 3]) -> f64 {
    let [a, b, c] = matrix;
    a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
        + a[2] * (b[0] * c[1] - b[1] * c[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVE_DRIVER: u16 = 3234;
    const GLINE: u16 = 1798;
    const GINSTANCE: u16 = 2042;

    fn pipe_body() -> Vec<u8> {
        let mut body = vec![0x55; 17];
        body.extend([0xff; 4]);
        body.extend(CURVE_DRIVER.to_le_bytes());
        body.extend(0.2_f64.to_le_bytes());
        body.extend(0.2_f64.to_le_bytes());
        body.extend([0x66; 31]);
        body.extend(SERIALIZED_LINE_MARKER);
        for value in [2.0_f64, 5.0, 10.0, 20.0, 30.0, 1.0, 0.0, 0.0] {
            body.extend(value.to_le_bytes());
        }
        body
    }

    fn matching_bounds() -> GElementBounds {
        GElementBounds {
            offset: 24,
            min: [12.0, 19.9, 29.9],
            max: [15.0, 20.1, 30.1],
        }
    }

    fn fitting_center_line_body() -> Vec<u8> {
        let mut body = vec![0x55; 23];
        body.extend([1, 0, 0, 0, 3, 0, 0, 0]);
        body.extend(GLINE.to_le_bytes());
        body.extend(417_660_u32.to_le_bytes());
        body.extend([0x66; 79]);
        body.extend(SERIALIZED_GLINE_MARKER);
        for value in [0.0_f64, 2.0, 10.0, 20.0, 30.0, 0.0, 0.0, 1.0] {
            body.extend(value.to_le_bytes());
        }
        body
    }

    fn family_instance_body() -> Vec<u8> {
        let mut body = vec![0x55; 37];
        for value in [10.0_f64, 20.0, 30.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
            body.extend(value.to_le_bytes());
        }
        body.extend([0x66; 11]);
        body
    }

    fn ginstance_body() -> Vec<u8> {
        let mut body = vec![0x55; 22];
        body.extend(GINSTANCE.to_le_bytes());
        body.extend([0x66; 31]);
        for value in [
            0.0_f64, 0.0, 1.0, 0.0, -1.0, 0.0, 1.0, 0.0, 0.0, 10.0, 20.0, 30.0,
        ] {
            body.extend(value.to_le_bytes());
        }
        body.extend(417_391_u32.to_le_bytes());
        body
    }

    fn gelement_graph_body() -> Vec<u8> {
        let mut body = vec![0x55; GELEMENT_NODE_COUNT_OFFSET];
        body.extend(2_u32.to_le_bytes());
        body.extend(3_u32.to_le_bytes());
        body.extend(2_081_u16.to_le_bytes());
        body.extend(4_u32.to_le_bytes());
        body.extend(GINSTANCE.to_le_bytes());
        let first = [-1.0_f64, -2.0, -3.0, 1.0, 2.0, 3.0];
        let second = [-1.0_f64 + 1.0e-12, -2.0, -3.0, 1.0, 2.0, 3.0];
        for value in first.into_iter().chain(second) {
            body.extend(value.to_le_bytes());
        }
        body
    }

    #[test]
    fn reads_a_schema_resolved_straight_pipe_and_checks_its_bounds() {
        let pipe = PipeLineGeometryFields::parse(&pipe_body(), CURVE_DRIVER).unwrap();
        assert!((pipe.nominal_diameter_feet - 0.2).abs() < f64::EPSILON);
        for (actual, expected) in pipe
            .start
            .coordinates_feet
            .into_iter()
            .zip([12.0, 20.0, 30.0])
        {
            assert!((actual - expected).abs() < f64::EPSILON);
        }
        for (actual, expected) in pipe
            .end
            .coordinates_feet
            .into_iter()
            .zip([15.0, 20.0, 30.0])
        {
            assert!((actual - expected).abs() < f64::EPSILON);
        }
        assert!(pipe.matches_bounds(&matching_bounds()));
        assert!((pipe.swept_radius_feet(&matching_bounds()).unwrap() - 0.1).abs() < 1.0e-12);
    }

    #[test]
    fn reads_a_single_schema_resolved_fitting_center_line() {
        let line = FittingCenterLineFields::parse(&fitting_center_line_body(), GLINE).unwrap();
        assert_eq!(line.owner_element_id, 417_660);
        for (actual, expected) in line
            .start
            .coordinates_feet
            .into_iter()
            .chain(line.end.coordinates_feet)
            .zip([10.0, 20.0, 30.0, 10.0, 20.0, 32.0])
        {
            assert!((actual - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn finds_orthonormal_family_instance_placement_candidates() {
        let candidates = FamilyInstancePlacementFields::candidates(&family_instance_body(), 20);
        assert_eq!(candidates.len(), 1);
        let placement = candidates[0];
        assert_eq!(placement.offset, 37);
        for (actual, expected) in placement
            .reference_direction
            .into_iter()
            .chain(placement.axis)
            .zip([0.0, 1.0, 0.0, 0.0, 0.0, 1.0])
        {
            assert!((actual - expected).abs() < f64::EPSILON);
        }
        assert!(
            GElementBounds {
                offset: 0,
                min: [9.0, 19.0, 29.0],
                max: [11.0, 21.0, 31.0],
            }
            .contains_point(placement.origin)
        );
    }

    #[test]
    fn reads_a_right_handed_ginstance_world_transform() {
        let bounds = GElementBounds {
            offset: 24,
            min: [9.0, 19.0, 29.0],
            max: [11.0, 21.0, 31.0],
        };
        let transform =
            GInstanceTransformFields::parse(&ginstance_body(), GINSTANCE, &bounds).unwrap();
        assert_eq!(transform.offset, 55);
        assert_eq!(transform.symbol_element_id, Some(417_391));
        assert!(bounds.contains_point(transform.origin));
        assert!((determinant(transform.basis) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reads_schema_checked_gelement_nodes_and_structural_bounds() {
        let graph = GElementGraphFields::parse(&gelement_graph_body(), |class_index| {
            matches!(class_index, 2_081 | GINSTANCE)
        })
        .unwrap();
        assert_eq!(graph.bounds.offset, 30);
        for (actual, expected) in graph
            .bounds
            .min
            .into_iter()
            .chain(graph.bounds.max)
            .zip([-1.0, -2.0, -3.0, 1.0, 2.0, 3.0])
        {
            assert!((actual - expected).abs() < f64::EPSILON);
        }
        assert_eq!(
            graph.top_level_nodes,
            [
                GElementNodeReference {
                    object_id: 3,
                    class_index: 2_081,
                },
                GElementNodeReference {
                    object_id: 4,
                    class_index: GINSTANCE,
                },
            ]
        );

        assert!(
            GElementGraphFields::parse(&gelement_graph_body(), |class_index| {
                class_index == GINSTANCE
            })
            .is_none()
        );
    }

    #[test]
    fn refuses_a_bounds_block_that_is_flat_on_one_axis_as_a_volume() {
        let volumetric = GElementBounds {
            offset: 0,
            min: [-1.0, -2.0, -3.0],
            max: [1.0, 2.0, 3.0],
        };
        assert!(volumetric.is_volumetric());
        assert!(
            !GElementBounds {
                max: [1.0, 2.0, -3.0],
                ..volumetric
            }
            .is_volumetric()
        );
    }

    #[test]
    fn verifies_symbol_bounds_after_rigid_instance_transform() {
        let transform = GInstanceTransformFields {
            offset: 0,
            basis: [[0.0, 0.0, -1.0], [0.0, -1.0, 0.0], [-1.0, 0.0, 0.0]],
            origin: RvtPoint3 {
                coordinates_feet: [10.0, 20.0, 30.0],
            },
            symbol_element_id: Some(417_391),
        };
        let local = GElementBounds {
            offset: 0,
            min: [-1.0, -2.0, -3.0],
            max: [1.0, 2.0, 3.0],
        };
        let world = GElementBounds {
            offset: 0,
            min: [7.0, 18.0, 29.0],
            max: [13.0, 22.0, 31.0],
        };
        assert!(world.matches_transformed(&local, &transform));

        let mismatched = GElementBounds {
            max: [13.1, 22.0, 31.0],
            ..world
        };
        assert!(!mismatched.matches_transformed(&local, &transform));
    }

    #[test]
    fn rejects_a_reflected_or_out_of_bounds_ginstance_transform() {
        let bounds = GElementBounds {
            offset: 24,
            min: [9.0, 19.0, 29.0],
            max: [11.0, 21.0, 31.0],
        };
        let mut reflected = ginstance_body();
        let third_basis = 55 + 6 * 8;
        reflected[third_basis..third_basis + 8].copy_from_slice(&(-1.0_f64).to_le_bytes());
        assert!(GInstanceTransformFields::parse(&reflected, GINSTANCE, &bounds).is_none());

        let outside = GElementBounds {
            max: [9.5, 21.0, 31.0],
            ..bounds
        };
        assert!(GInstanceTransformFields::parse(&ginstance_body(), GINSTANCE, &outside).is_none());
    }

    #[test]
    fn rejects_non_orthogonal_family_instance_axes() {
        let mut body = family_instance_body();
        let axis = 37 + 6 * 8;
        body[axis..axis + 8].copy_from_slice(&0.0_f64.to_le_bytes());
        body[axis + 8..axis + 16].copy_from_slice(&1.0_f64.to_le_bytes());
        assert!(FamilyInstancePlacementFields::candidates(&body, 20).is_empty());
    }

    #[test]
    fn rejects_ambiguous_or_non_linear_fitting_center_lines() {
        let mut ambiguous = fitting_center_line_body();
        ambiguous.extend(fitting_center_line_body());
        assert!(FittingCenterLineFields::parse(&ambiguous, GLINE).is_none());

        let mut non_linear = fitting_center_line_body();
        let marker = marker_offsets(&non_linear, &SERIALIZED_GLINE_MARKER)
            .next()
            .unwrap();
        non_linear[marker..marker + 8].fill(0);
        assert!(FittingCenterLineFields::parse(&non_linear, GLINE).is_none());
    }

    #[test]
    fn derives_an_outer_radius_that_differs_from_the_nominal_diameter() {
        let pipe = PipeLineGeometryFields::parse(&pipe_body(), CURVE_DRIVER).unwrap();
        let bounds = GElementBounds {
            offset: 24,
            min: [12.0, 19.88, 29.88],
            max: [15.0, 20.12, 30.12],
        };
        assert!((pipe.swept_radius_feet(&bounds).unwrap() - 0.12).abs() < 1.0e-12);
    }

    #[test]
    fn rejects_a_non_unit_direction_or_non_circular_section() {
        let mut bad_direction = pipe_body();
        let direction_x = bad_direction.len() - 3 * 8;
        bad_direction[direction_x..direction_x + 8].copy_from_slice(&2.0_f64.to_le_bytes());
        assert!(PipeLineGeometryFields::parse(&bad_direction, CURVE_DRIVER).is_none());

        let mut rectangle = pipe_body();
        let height = 17 + 6 + 8;
        rectangle[height..height + 8].copy_from_slice(&0.3_f64.to_le_bytes());
        assert!(PipeLineGeometryFields::parse(&rectangle, CURVE_DRIVER).is_none());
    }

    #[test]
    fn rejects_an_ambiguous_line() {
        let mut body = pipe_body();
        let line = body[body.len() - SERIALIZED_LINE_MARKER.len() - 8 * 8..].to_vec();
        body.extend(line);
        assert!(PipeLineGeometryFields::parse(&body, CURVE_DRIVER).is_none());
    }

    #[test]
    fn reads_only_an_exact_duplicated_bounds_block() {
        let values = [12.0_f64, 19.9, 29.9, 15.0, 20.1, 30.1];
        let mut body = vec![0x33; 24];
        for _ in 0..2 {
            for value in values {
                body.extend(value.to_le_bytes());
            }
        }
        assert_eq!(GElementBounds::parse(&body).unwrap(), matching_bounds());
        body[24] ^= 1;
        assert!(GElementBounds::parse(&body).is_none());
    }

    #[test]
    fn reads_near_duplicate_bounds_only_for_placement_validation() {
        let values = [12.0_f64, 19.9, 29.9, 15.0, 20.1, 30.1];
        let mut body = vec![0x33; 24];
        for value in values {
            body.extend(value.to_le_bytes());
        }
        for (index, value) in values.into_iter().enumerate() {
            body.extend((value + if index == 3 { 5.0e-9 } else { 0.0 }).to_le_bytes());
        }
        assert!(GElementBounds::parse(&body).is_none());
        let bounds = GElementBounds::parse_near_duplicate(&body).unwrap();
        assert!((bounds.max[0] - (15.0 + 5.0e-9)).abs() < f64::EPSILON);
    }

    #[test]
    fn checks_a_line_against_independent_element_bounds() {
        let bounds = matching_bounds();
        assert!(bounds.contains_line_segment(
            RvtPoint3 {
                coordinates_feet: [12.0, 20.0, 30.0],
            },
            RvtPoint3 {
                coordinates_feet: [15.0, 20.0, 30.0],
            },
        ));
        assert!(!bounds.contains_line_segment(
            RvtPoint3 {
                coordinates_feet: [11.0, 20.0, 30.0],
            },
            RvtPoint3 {
                coordinates_feet: [15.0, 20.0, 30.0],
            },
        ));
    }

    #[test]
    fn rejects_bounds_that_do_not_describe_the_swept_line() {
        let pipe = PipeLineGeometryFields::parse(&pipe_body(), CURVE_DRIVER).unwrap();
        let mut bounds = matching_bounds();
        bounds.max[2] += 1.0;
        assert!(!pipe.matches_bounds(&bounds));
    }

    #[test]
    fn checks_that_a_line_is_contained_by_independent_bounds() {
        let bounds = matching_bounds();
        assert!(bounds.contains_line_segment(
            RvtPoint3 {
                coordinates_feet: [12.0, 20.0, 30.0],
            },
            RvtPoint3 {
                coordinates_feet: [15.0, 20.0, 30.0],
            },
        ));
        assert!(!bounds.contains_line_segment(
            RvtPoint3 {
                coordinates_feet: [11.0, 20.0, 30.0],
            },
            RvtPoint3 {
                coordinates_feet: [15.0, 20.0, 30.0],
            },
        ));
    }
}
