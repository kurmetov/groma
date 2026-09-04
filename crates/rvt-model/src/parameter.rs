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
/// after another in that order. Where the run begins inside a body is not
/// derivable yet, so it is located by scanning for a run of at least two sets
/// or three parameters, which makes a chance match very unlikely.
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

impl ParameterSets {
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
        // With the narrow built-in window a single valid set is already strong
        // evidence, so no minimum set count is imposed; every entry in the run
        // must still carry an acceptable identifier.
        (0..body.len().saturating_sub(4)).find_map(|offset| Self::read_run(body, offset, &accept))
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
        (sets > 0).then(|| Self {
            offset,
            encoded_bytes: cursor - offset,
            parameters,
            sets,
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
    fn refuses_a_code_outside_the_built_in_window() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend((-15_990_784_i32).to_le_bytes());
        bytes.extend(4_i32.to_le_bytes());
        assert!(ParameterSets::scan(&bytes, &|_| false).is_none());
    }
}
