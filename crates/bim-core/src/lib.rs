#![forbid(unsafe_code)]

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BimModel {
    pub elements: Vec<BimElement>,
    pub levels: Vec<BimLevel>,
    pub relations: Vec<BimRelation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimElement {
    pub id: BimElementId,
    pub name: Option<String>,
    pub category: Option<BimCategory>,
    pub properties: Vec<BimProperty>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BimElementId(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimCategory {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimProperty {
    pub name: String,
    pub value: BimPropertyValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BimPropertyValue {
    Bool(bool),
    Integer(i64),
    Number(f64),
    Text(String),
    Bytes(Vec<u8>),
    Unknown(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimRelation {
    pub kind: String,
    pub source: BimElementId,
    pub target: BimElementId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimLevel {
    pub id: BimElementId,
    pub name: Option<String>,
    pub elevation: Option<f64>,
}
