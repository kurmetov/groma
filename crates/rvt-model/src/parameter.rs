use crate::member::RecordString;

/// Largest parameter count accepted from one stored set.
pub const MAX_PARAMETERS_PER_SET: u32 = 4096;
/// Range a built-in parameter code falls into. Built-in codes are negative;
/// anything else must be the identifier of a parameter element.
///
/// Measured over 163,260 recovered values: 99.6% land in `-1_200_000..-1_152`,
/// and the remaining 0.4% scatter down to -16M, which is what a chance match
/// looks like. The window is kept a little wider than the observed cluster and
/// far narrower than the `i32` range, so a stray word is unlikely to pass.
const BUILT_IN_PARAMETER_RANGE: std::ops::Range<i32> = -2_000_000..-1_000;

/// Prefix of measurable and non-measurable Forge spec identifiers.
pub const AUTODESK_SPEC_PREFIX: &str = "autodesk.spec.";

/// The Forge spec carried by a project/shared parameter definition.
///
/// A stored parameter value points to its definition by positive element ID.
/// The definition's `ParamDef` object in turn carries this type ID. The spec,
/// rather than a document display unit, determines the value's dimension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParameterSpec {
    pub offset: usize,
    pub type_id: String,
}

impl ParameterSpec {
    /// Find the single Forge spec identifier in a parameter-definition body.
    ///
    /// The string encoding is deterministic, but its owning `ParamDef`
    /// subclass is variable, so this remains a bounded scan. A body with more
    /// than one spec identifier is rejected rather than choosing one.
    #[must_use]
    pub fn scan(body: &[u8]) -> Option<Self> {
        let mut found = None;
        for offset in 0..body.len().saturating_sub(4) {
            let Some(value) = RecordString::parse_at_lenient(body, offset) else {
                continue;
            };
            if !value.value.starts_with(AUTODESK_SPEC_PREFIX) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(Self {
                offset,
                type_id: value.value,
            });
        }
        found
    }
}

/// A parameter value as stored, without unit conversion or interpretation.
#[derive(Clone, Debug, PartialEq)]
pub enum ParameterValue {
    Double(f64),
    Integer(i32),
    Text(String),
    /// An `ElementId` value: a reference to another element.
    Reference(i32),
}

/// One stored parameter: its identifier and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    /// Negative for a built-in parameter code, positive for a parameter
    /// element that carries the parameter's name.
    pub id: i32,
    pub value: ParameterValue,
}

impl Parameter {
    #[must_use]
    pub const fn is_built_in(&self) -> bool {
        self.id < 0
    }
}

/// The four typed parameter sets an element stores, in schema order.
///
/// `Element` declares `m_pParamValueSetDouble`, `m_pParamValueSetInt`,
/// `m_pParamValueSetAString` and `m_pParamValueSetElementId`. Each set is
/// `[count:u32]` followed by that many entries, and the sets are stored one
/// after another. The preferred reader binds the value kinds to dynamic set
/// class references before `m_id` and accepts only one matching run. The
/// older unbound scan remains available for unsupported schemas as diagnostic
/// output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParameterSets {
    /// Offset in the body where the run of sets starts.
    pub offset: usize,
    /// Bytes the run occupies.
    pub encoded_bytes: usize,
    pub parameters: Vec<Parameter>,
    /// How many of the four typed sets were present.
    pub sets: usize,
}

/// Dynamic class indexes of the four parameter-set objects declared by
/// `Element`. They are resolved from each file's `Formats/Latest` schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParameterSetClassIndexes {
    pub double: u16,
    pub integer: u16,
    pub text: u16,
    pub reference: u16,
}

impl ParameterSets {
    /// Read the one parameter run whose value kinds agree with the dynamic
    /// set classes referenced before the element's `m_id` field.
    ///
    /// `search_start` should be the end of the fixed `Element` tail and
    /// `pointer_end` the offset of `m_id`. No result is returned if there are
    /// no set-class references or if more than one matching run exists.
    #[must_use]
    pub fn scan_schema_bound(
        body: &[u8],
        search_start: usize,
        pointer_end: usize,
        classes: ParameterSetClassIndexes,
        accept_id: &impl Fn(i32) -> bool,
    ) -> Option<Self> {
        let kinds = referenced_kinds(body.get(..pointer_end)?, classes)?;
        let mut found = None;
        for offset in search_start..body.len().saturating_sub(3) {
            let Some(candidate) = Self::read_typed_run(body, offset, &kinds, accept_id) else {
                continue;
            };
            if found.is_some() {
                return None;
            }
            found = Some(candidate);
        }
        found
    }

    /// Scan a record body for the run of parameter sets.
    ///
    /// `accept_id` decides whether a positive identifier belongs to a known
    /// parameter element; negative built-in codes are accepted on their own.
    #[must_use]
    pub fn scan(body: &[u8], accept_id: &impl Fn(i32) -> bool) -> Option<Self> {
        let accept = |id: i32| {
            if id < 0 {
                BUILT_IN_PARAMETER_RANGE.contains(&id)
            } else {
                accept_id(id)
            }
        };
        Self::scan_accepting(body, &accept)
    }

    /// Scan using a caller-supplied allow-list for every identifier, including
    /// negative built-ins. This is preferred when a release-specific public
    /// catalog is available.
    #[must_use]
    pub fn scan_verified(body: &[u8], accept_id: &impl Fn(i32) -> bool) -> Option<Self> {
        Self::scan_accepting(body, accept_id)
    }

    fn scan_accepting(body: &[u8], accept_id: &impl Fn(i32) -> bool) -> Option<Self> {
        (0..body.len().saturating_sub(4)).find_map(|offset| Self::read_run(body, offset, accept_id))
    }

    /// Read the four sets in schema order starting at `offset`.
    fn read_run(body: &[u8], offset: usize, accept: &impl Fn(i32) -> bool) -> Option<Self> {
        let mut cursor = offset;
        let mut parameters = Vec::new();
        let mut sets = 0_usize;

        for kind in [Kind::Double, Kind::Integer, Kind::Text, Kind::Reference] {
            let Some((next, mut read)) = read_set(body, cursor, kind, accept) else {
                continue;
            };
            sets += 1;
            parameters.append(&mut read);
            cursor = next;
        }
        // One built-in-only set is strong when the caller checked every code
        // against its release catalog. Positive IDs occur throughout record
        // bodies, so a lone project/shared parameter needs independent
        // evidence: another typed set or at least three entries.
        let built_in_only = !parameters.is_empty() && parameters.iter().all(Parameter::is_built_in);
        (sets >= 2 || parameters.len() >= 3 || built_in_only).then(|| Self {
            offset,
            encoded_bytes: cursor - offset,
            parameters,
            sets,
        })
    }

    fn read_typed_run(
        body: &[u8],
        offset: usize,
        kinds: &[Kind],
        accept: &impl Fn(i32) -> bool,
    ) -> Option<Self> {
        let mut cursor = offset;
        let mut parameters = Vec::new();
        for kind in kinds {
            let (next, mut read) = read_set(body, cursor, *kind, accept)?;
            parameters.append(&mut read);
            cursor = next;
        }
        Some(Self {
            offset,
            encoded_bytes: cursor - offset,
            parameters,
            sets: kinds.len(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Double,
    Integer,
    Text,
    Reference,
}

fn referenced_kinds(prefix: &[u8], classes: ParameterSetClassIndexes) -> Option<Vec<Kind>> {
    let mut found = [
        (Kind::Double, classes.double, None),
        (Kind::Integer, classes.integer, None),
        (Kind::Text, classes.text, None),
        (Kind::Reference, classes.reference, None),
    ];
    for offset in 0..prefix.len().saturating_sub(3) {
        if prefix.get(offset..offset + 2) != Some(&[0xff, 0xff]) {
            continue;
        }
        let index = u16::from_le_bytes([prefix[offset + 2], prefix[offset + 3]]);
        for (_, wanted, position) in &mut found {
            if index == *wanted {
                if position.is_some() {
                    return None;
                }
                *position = Some(offset);
            }
        }
    }
    let mut kinds = found
        .into_iter()
        .filter_map(|(kind, _, position)| Some((position?, kind)))
        .collect::<Vec<_>>();
    if kinds.is_empty() {
        return None;
    }
    kinds.sort_unstable_by_key(|(position, _)| *position);
    Some(kinds.into_iter().map(|(_, kind)| kind).collect())
}

/// Read one `[count:u32]`-prefixed set, returning the offset after it.
fn read_set(
    body: &[u8],
    offset: usize,
    kind: Kind,
    accept: &impl Fn(i32) -> bool,
) -> Option<(usize, Vec<Parameter>)> {
    let prefix = body.get(offset..offset + 4)?;
    let count = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    if count == 0 || count > MAX_PARAMETERS_PER_SET {
        return None;
    }

    let mut cursor = offset + 4;
    let mut parameters = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (next, parameter) = read_parameter(body, cursor, kind)?;
        if !accept(parameter.id) {
            return None;
        }
        parameters.push(parameter);
        cursor = next;
    }
    Some((cursor, parameters))
}

fn read_parameter(body: &[u8], offset: usize, kind: Kind) -> Option<(usize, Parameter)> {
    let read_i32 = |at: usize| -> Option<i32> {
        let bytes = body.get(at..at + 4)?;
        Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    match kind {
        Kind::Double => {
            let bytes = body.get(offset..offset + 8)?;
            let value = f64::from_le_bytes(bytes.try_into().ok()?);
            if !value.is_finite() {
                return None;
            }
            let id = read_i32(offset + 8)?;
            Some((
                offset + 12,
                Parameter {
                    id,
                    value: ParameterValue::Double(value),
                },
            ))
        }
        Kind::Integer => {
            let id = read_i32(offset)?;
            let value = read_i32(offset + 4)?;
            Some((
                offset + 8,
                Parameter {
                    id,
                    value: ParameterValue::Integer(value),
                },
            ))
        }
        Kind::Reference => {
            let id = read_i32(offset)?;
            let value = read_i32(offset + 4)?;
            Some((
                offset + 8,
                Parameter {
                    id,
                    value: ParameterValue::Reference(value),
                },
            ))
        }
        Kind::Text => {
            let id = read_i32(offset)?;
            let text = RecordString::parse_at_lenient(body, offset + 4)?;
            Some((
                offset + 4 + text.encoded_bytes,
                Parameter {
                    id,
                    value: ParameterValue::Text(text.value),
                },
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_bytes(value: &str) -> Vec<u8> {
        let units = value.encode_utf16().collect::<Vec<_>>();
        let mut bytes = u32::try_from(units.len()).unwrap().to_le_bytes().to_vec();
        for unit in units {
            bytes.extend(unit.to_le_bytes());
        }
        bytes
    }

    /// One integer set and one text set, as an element stores them.
    fn sets_fixture() -> Vec<u8> {
        let mut bytes = vec![0xaa_u8; 6];
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend((-1_114_242_i32).to_le_bytes());
        bytes.extend(0_i32.to_le_bytes());
        bytes.extend(2_u32.to_le_bytes());
        bytes.extend((-1_001_203_i32).to_le_bytes());
        bytes.extend(text_bytes("153"));
        bytes.extend(221_298_i32.to_le_bytes());
        bytes.extend(text_bytes("3"));
        bytes
    }

    #[test]
    fn reads_an_integer_set_followed_by_a_text_set() {
        let bytes = sets_fixture();
        let found = ParameterSets::scan(&bytes, &|id| id == 221_298).unwrap();

        assert_eq!(found.offset, 6);
        assert_eq!(found.sets, 2);
        assert_eq!(found.parameters.len(), 3);
        assert_eq!(
            found.parameters[0],
            Parameter {
                id: -1_114_242,
                value: ParameterValue::Integer(0)
            }
        );
        assert_eq!(
            found.parameters[1],
            Parameter {
                id: -1_001_203,
                value: ParameterValue::Text("153".to_owned())
            }
        );
        assert_eq!(
            found.parameters[2],
            Parameter {
                id: 221_298,
                value: ParameterValue::Text("3".to_owned())
            }
        );
        assert!(found.parameters[0].is_built_in());
        assert!(!found.parameters[2].is_built_in());
    }

    #[test]
    fn reads_a_double_set() {
        let mut bytes = 3_u32.to_le_bytes().to_vec();
        bytes.extend(3.937_f64.to_le_bytes());
        bytes.extend((-1_002_000_i32).to_le_bytes());
        bytes.extend(0.5_f64.to_le_bytes());
        bytes.extend((-1_002_001_i32).to_le_bytes());
        bytes.extend(12.0_f64.to_le_bytes());
        bytes.extend((-1_002_002_i32).to_le_bytes());

        let found = ParameterSets::scan(&bytes, &|_| false).unwrap();
        assert_eq!(found.sets, 1);
        assert_eq!(found.parameters.len(), 3);
        assert_eq!(found.parameters[0].value, ParameterValue::Double(3.937));
        assert_eq!(found.encoded_bytes, bytes.len());
    }

    #[test]
    fn schema_bound_scan_uses_the_referenced_value_kinds() {
        let classes = ParameterSetClassIndexes {
            double: 0x1122,
            integer: 0x2233,
            text: 0x3344,
            reference: 0x4455,
        };
        let mut bytes = vec![0xff, 0xff, 0x22, 0x11];
        bytes.extend([0xff, 0xff, 0xff, 0xff, 0x44, 0x33]);
        let pointer_end = bytes.len();
        bytes.extend([0xaa, 0xbb]);
        let offset = bytes.len();
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(0.009_f64.to_le_bytes());
        bytes.extend(7_i32.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(8_i32.to_le_bytes());
        bytes.extend(text_bytes("3"));

        let found = ParameterSets::scan_schema_bound(&bytes, offset, pointer_end, classes, &|id| {
            matches!(id, 7 | 8)
        })
        .unwrap();
        assert_eq!(found.offset, offset);
        assert_eq!(found.sets, 2);
        assert_eq!(found.parameters[0].value, ParameterValue::Double(0.009));
        assert_eq!(
            found.parameters[1].value,
            ParameterValue::Text("3".to_owned())
        );

        assert!(ParameterSets::scan_schema_bound(&bytes, offset, 0, classes, &|_| true).is_none());
    }

    #[test]
    fn schema_bound_scan_rejects_an_ambiguous_run() {
        let classes = ParameterSetClassIndexes {
            double: 1,
            integer: 2,
            text: 3,
            reference: 4,
        };
        let mut bytes = vec![0xff, 0xff, 2, 0];
        let pointer_end = bytes.len();
        let run = [
            1_u32.to_le_bytes(),
            7_i32.to_le_bytes(),
            1_i32.to_le_bytes(),
        ]
        .concat();
        bytes.extend(&run);
        bytes.extend(&run);
        assert!(
            ParameterSets::scan_schema_bound(&bytes, pointer_end, pointer_end, classes, &|id| id
                == 7)
            .is_none()
        );
    }

    #[test]
    fn rejects_an_unknown_positive_identifier() {
        let mut bytes = 2_u32.to_le_bytes().to_vec();
        bytes.extend(7_i32.to_le_bytes());
        bytes.extend(1_i32.to_le_bytes());
        bytes.extend(8_i32.to_le_bytes());
        bytes.extend(1_i32.to_le_bytes());
        assert!(ParameterSets::scan(&bytes, &|_| false).is_none());
    }

    #[test]
    fn accepts_a_single_set_with_one_built_in_parameter() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend((-1_002_000_i32).to_le_bytes());
        bytes.extend(4_i32.to_le_bytes());
        let found = ParameterSets::scan(&bytes, &|_| false).unwrap();
        assert_eq!(found.parameters.len(), 1);
        assert_eq!(found.parameters[0].value, ParameterValue::Integer(4));
    }

    #[test]
    fn verified_scan_rejects_a_built_in_code_missing_from_the_catalog() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend((-65_536_i32).to_le_bytes());
        bytes.extend(4_i32.to_le_bytes());
        assert!(ParameterSets::scan_verified(&bytes, &|_| false).is_none());
    }

    #[test]
    fn rejects_one_unaccompanied_positive_parameter() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend(3.937_f64.to_le_bytes());
        bytes.extend(221_298_i32.to_le_bytes());
        assert!(ParameterSets::scan(&bytes, &|id| id == 221_298).is_none());
    }

    #[test]
    fn refuses_a_code_outside_the_built_in_window() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend((-15_990_784_i32).to_le_bytes());
        bytes.extend(4_i32.to_le_bytes());
        assert!(ParameterSets::scan(&bytes, &|_| false).is_none());
    }

    #[test]
    fn reads_the_spec_from_a_parameter_definition() {
        let mut bytes = vec![0xff; 17];
        let offset = bytes.len();
        bytes.extend(text_bytes(
            "autodesk.spec.aec.structural:massPerUnitLength-1.0.0",
        ));
        bytes.extend([0; 5]);

        let found = ParameterSpec::scan(&bytes).unwrap();
        assert_eq!(found.offset, offset);
        assert_eq!(
            found.type_id,
            "autodesk.spec.aec.structural:massPerUnitLength-1.0.0"
        );
    }

    #[test]
    fn rejects_an_ambiguous_parameter_spec() {
        let mut bytes = text_bytes("autodesk.spec.aec:length-2.0.0");
        bytes.extend(text_bytes("autodesk.spec.aec:area-2.0.0"));
        assert!(ParameterSpec::scan(&bytes).is_none());
    }
}
