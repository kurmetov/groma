use std::{io::Write, process::Command};

use flate2::{Compression, write::DeflateEncoder};
use tempfile::NamedTempFile;

fn fixture() -> NamedTempFile {
    fixture_with_schema(&schema_fixture())
}

fn ifc_fixture() -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(
        b"ISO-10303-21;\n\
          HEADER;\n\
          FILE_SCHEMA(('IFC4'));\n\
          ENDSEC;\n\
          DATA;\n\
          #1=IFCCARTESIANPOINT((0.,0.,0.));\n\
          #2=IFCDIRECTION((0.,0.,1.));\n\
          #3=IFCDIRECTION((1.,0.,0.));\n\
          #4=IFCAXIS2PLACEMENT3D(#1,#2,#3);\n\
          #5=IFCLOCALPLACEMENT($,#4);\n\
          #6=IFCCARTESIANPOINT((0.,0.));\n\
          #7=IFCDIRECTION((1.,0.));\n\
          #8=IFCAXIS2PLACEMENT2D(#6,#7);\n\
          #9=IFCRECTANGLEPROFILEDEF(.AREA.,$,#8,2.,1.);\n\
          #10=IFCDIRECTION((0.,0.,1.));\n\
          #11=IFCEXTRUDEDAREASOLID(#9,#4,#10,3.);\n\
          #12=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#11));\n\
          #13=IFCPRODUCTDEFINITIONSHAPE($,$,(#12));\n\
          #14=IFCWALL('wall-guid',$,'Fixture wall',$,$,#5,#13,$,$);\n\
          #15=IFCBUILDINGSTOREY('level-guid',$,'Ground floor',$,$,#5,$,$,$,0.);\n\
          #16=IFCRELCONTAINEDINSPATIALSTRUCTURE('rel-guid',$,$,$,(#14),#15);\n\
          #17=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);\n\
          #18=IFCUNITASSIGNMENT((#17));\n\
          #19=IFCPROJECT('project-guid',$,'Fixture project',$,$,$,$,(),#18);\n\
          ENDSEC;\n\
          END-ISO-10303-21;\n",
    )
    .unwrap();
    file.flush().unwrap();
    file
}

/// The same IFC as [`ifc_fixture`], written under a name of your choosing and
/// with its wall moved along X, so that two of them can be federated and
/// their extents made to overlap or not.
fn ifc_fixture_in(directory: &std::path::Path, name: &str, x: f64) -> std::path::PathBuf {
    let path = directory.join(name);
    let body = format!(
        "ISO-10303-21;\n\
         HEADER;\n\
         FILE_SCHEMA(('IFC4'));\n\
         ENDSEC;\n\
         DATA;\n\
         #1=IFCCARTESIANPOINT(({x}.,0.,0.));\n\
         #2=IFCDIRECTION((0.,0.,1.));\n\
         #3=IFCDIRECTION((1.,0.,0.));\n\
         #4=IFCAXIS2PLACEMENT3D(#1,#2,#3);\n\
         #5=IFCLOCALPLACEMENT($,#4);\n\
         #6=IFCCARTESIANPOINT((0.,0.));\n\
         #7=IFCDIRECTION((1.,0.));\n\
         #8=IFCAXIS2PLACEMENT2D(#6,#7);\n\
         #9=IFCRECTANGLEPROFILEDEF(.AREA.,$,#8,2.,1.);\n\
         #10=IFCDIRECTION((0.,0.,1.));\n\
         #11=IFCEXTRUDEDAREASOLID(#9,#4,#10,3.);\n\
         #12=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#11));\n\
         #13=IFCPRODUCTDEFINITIONSHAPE($,$,(#12));\n\
         #14=IFCWALL('wall-guid',$,'Fixture wall',$,$,#5,#13,$,$);\n\
         #15=IFCBUILDINGSTOREY('level-guid',$,'Ground floor',$,$,#5,$,$,$,0.);\n\
         #16=IFCRELCONTAINEDINSPATIALSTRUCTURE('rel-guid',$,$,$,(#14),#15);\n\
         #17=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);\n\
         #18=IFCUNITASSIGNMENT((#17));\n\
         #19=IFCPROJECT('project-guid',$,'Fixture project',$,$,$,$,(),#18);\n\
         ENDSEC;\n\
         END-ISO-10303-21;\n"
    );
    std::fs::write(&path, body).unwrap();
    path
}

fn fixture_with_schema(schema: &[u8]) -> NamedTempFile {
    fixture_with_schema_and_partition(schema, b"partition")
}

fn fixture_with_schema_and_partition(schema: &[u8], partition: &[u8]) -> NamedTempFile {
    fixture_with_streams(schema, partition, b"elements")
}

fn fixture_with_elem_table(elem_table: &[u8]) -> NamedTempFile {
    fixture_with_streams(&schema_fixture(), b"partition", elem_table)
}

fn fixture_with_elem_table_and_partition(elem_table: &[u8], partition: &[u8]) -> NamedTempFile {
    fixture_with_streams(&schema_fixture(), partition, elem_table)
}

fn fixture_with_streams(schema: &[u8], partition: &[u8], elem_table: &[u8]) -> NamedTempFile {
    fixture_with_release(schema, partition, elem_table, "2026")
}

/// The same fixture, with the release `BasicFileInfo` states chosen by the
/// caller. A release decides which identifier tables apply, and the tables
/// are the one part of a read that a file cannot supply for itself.
fn fixture_with_release(
    schema: &[u8],
    partition: &[u8],
    elem_table: &[u8],
    release: &str,
) -> NamedTempFile {
    let file = NamedTempFile::new().unwrap();
    let mut compound = cfb::create(file.path()).unwrap();
    compound.create_storage("/Formats").unwrap();
    compound.create_storage("/Global").unwrap();
    compound.create_storage("/Partitions").unwrap();

    let mut basic_info = 14_u32.to_le_bytes().to_vec();
    basic_info.extend([0xaa, 0xbb]);
    basic_info.extend([4, 0, 0, 0]);
    for unit in release.encode_utf16() {
        basic_info.extend(unit.to_le_bytes());
    }

    for (path, bytes) in [
        ("/BasicFileInfo", basic_info.as_slice()),
        ("/Formats/Latest", schema),
        ("/Global/ElemTable", elem_table),
        ("/Global/Latest", b"global"),
        ("/Partitions/69", partition),
    ] {
        compound
            .create_stream(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    compound.flush().unwrap();
    drop(compound);
    file
}

fn elem_table_fixture() -> Vec<u8> {
    let decoded = decoded_elem_table_fixture(3);
    let mut stored = vec![0; 8];
    stored.extend(truncated_gzip(&decoded));
    stored
}

/// A `Global/ElemTable` payload in the shape the declarations give it: the
/// object's own class index, the record count, and one 28-byte `ElemRec`
/// each - `m_id`, `m_originalElementId`, three `EpisodeId`s, `m_partitionId`
/// and `m_OwningElementId`.
fn decoded_elem_table_fixture(tail_bytes: usize) -> Vec<u8> {
    let record_count = 10_u32;
    let records = usize::try_from(record_count).unwrap();
    let mut decoded = vec![0; ELEM_TABLE_HEADER_BYTES + records * 28 + tail_bytes];
    decoded[..2].copy_from_slice(&1370_u16.to_le_bytes());
    decoded[2..6].copy_from_slice(&record_count.to_le_bytes());
    for index in 0..records {
        let offset = ELEM_TABLE_HEADER_BYTES + index * 28;
        let id = u32::try_from(index + 1).unwrap();
        decoded[offset..offset + 4].copy_from_slice(&id.to_le_bytes());
        decoded[offset + 4..offset + 8].copy_from_slice(&id.to_le_bytes());
        decoded[offset + 24..offset + 28].copy_from_slice(&(-1_i32).to_le_bytes());
    }

    decoded
}

/// The class-index tag and the record count that open the payload.
const ELEM_TABLE_HEADER_BYTES: usize = 6;

fn checksum_paged_elem_table_fixture() -> Vec<u8> {
    let mut decoded = decoded_elem_table_fixture(90_000);
    let tail_start = ELEM_TABLE_HEADER_BYTES + 10 * 28;
    let mut state = 0x6d2b_79f5_u32;
    for byte in &mut decoded[tail_start..] {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state.to_le_bytes()[0];
    }

    let mut stored = vec![0; 8];
    stored.extend(truncated_gzip(&decoded));
    assert!(stored.len() > rvt_container::REVIT_PAGE_PAYLOAD_BYTES);
    assert!(stored.len() < 2 * rvt_container::REVIT_PAGE_PAYLOAD_BYTES);
    stored.splice(
        rvt_container::REVIT_PAGE_PAYLOAD_BYTES..rvt_container::REVIT_PAGE_PAYLOAD_BYTES,
        [0xa5; rvt_container::REVIT_PAGE_CHECKSUM_BYTES],
    );
    stored
}

fn truncated_gzip(payload: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload).unwrap();
    let deflate = encoder.finish().unwrap();

    let mut stored = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    stored.extend(deflate);
    stored
}

fn checksum_paged_schema_fixture() -> Vec<u8> {
    let mut payload = 0_i16.to_le_bytes().to_vec();
    payload.extend(7_u16.to_le_bytes());
    payload.extend(b"Element");
    payload.extend(0_u16.to_le_bytes());
    payload.extend(1_i32.to_le_bytes());
    payload.extend(1_i32.to_le_bytes());
    payload.extend(90_000_i32.to_le_bytes());
    let mut state = 0x6d2b_79f5_u32;
    for _ in 0..90_000 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        payload.push(state.to_le_bytes()[0]);
    }
    payload.extend([0x05, 0]);
    payload.extend(0_i16.to_le_bytes());
    payload.extend(0_i32.to_le_bytes());
    payload.extend([0; 8]);

    let mut stored = truncated_gzip(&payload);
    assert!(stored.len() > rvt_container::REVIT_PAGE_PAYLOAD_BYTES);
    assert!(stored.len() < 2 * rvt_container::REVIT_PAGE_PAYLOAD_BYTES);
    stored.splice(
        rvt_container::REVIT_PAGE_PAYLOAD_BYTES..rvt_container::REVIT_PAGE_PAYLOAD_BYTES,
        [0xa5; rvt_container::REVIT_PAGE_CHECKSUM_BYTES],
    );
    stored
}

fn schema_fixture() -> Vec<u8> {
    let mut bytes = 0_i16.to_le_bytes().to_vec();
    bytes.extend(7_u16.to_le_bytes());
    bytes.extend(b"Element");
    bytes.extend(0_u16.to_le_bytes());
    bytes.extend(1_i32.to_le_bytes());
    bytes.extend(1_i32.to_le_bytes());
    bytes.extend(2_i32.to_le_bytes());
    bytes.extend(b"Id");
    bytes.extend([0x05, 0]);
    bytes.extend(0_i16.to_le_bytes());
    bytes.extend(0_i32.to_le_bytes());
    bytes.extend([0; 8]);
    bytes
}

#[test]
fn info_reports_container_inventory() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["info", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Container: CFB/OLE"));
    assert!(stdout.contains("Revit version: 2026"));
    assert!(stdout.contains("Identifier catalog: Revit 2026"));
    assert!(stdout.contains("Streams: 5"));
    assert!(stdout.contains("Partitions: 1"));
}

#[test]
fn info_says_when_no_catalog_covers_the_release() {
    // 2024 is a real release whose enumerations have never been generated
    // here. The container still reads; what is missing is every name that
    // would have come from a table rather than from the file.
    let fixture = fixture_with_release(
        &schema_fixture(),
        b"partition",
        &elem_table_fixture(),
        "2024",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["info", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Revit version: 2024"));
    assert!(stdout.contains("Identifier catalog: none for this release"));
    assert!(stdout.contains("2023, 2026"));
}

#[test]
fn export_scene_says_when_no_catalog_covers_the_release() {
    let fixture = fixture_with_release(
        &schema_fixture(),
        &parameter_partition(),
        &elem_table_fixture(),
        "2024",
    );
    let target = NamedTempFile::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // The conversion goes through; what it says is which part of it was read
    // without a table, so nobody reads the thinner result as the whole model.
    assert!(stdout.contains("Scene written:"));
    assert!(stdout.contains("Revit 2024 wrote this file"));
    assert!(stdout.contains("identifier tables"));
}

#[test]
fn streams_lists_nested_paths() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["streams", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("8\tGlobal/ElemTable"));
    assert!(stdout.contains("9\tPartitions/69"));
}

#[test]
fn dump_stream_writes_only_raw_bytes_to_stdout() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "dump-stream",
            fixture.path().to_str().unwrap(),
            "Global/Latest",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"global");
    assert!(output.stderr.is_empty());
}

#[test]
fn schema_decodes_and_lists_classes() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["schema", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Framing: raw"));
    assert!(stdout.contains("Classes: 1"));
    assert!(stdout.contains("Properties: 1"));
    assert!(stdout.contains("12\tElement\tparent=-\tversion=1\tproperties=1"));
}

#[test]
fn schema_recovers_checksum_paged_storage_after_strict_failure() {
    let fixture = fixture_with_schema(&checksum_paged_schema_fixture());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["schema", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Framing: truncated gzip at byte 0"));
    assert!(stdout.contains("Checksum-page trailers stripped: true"));
    assert!(stdout.contains("Classes: 1"));
}

#[test]
fn partitions_inventories_validated_members() {
    let mut partition = vec![0; 44];
    partition.extend(truncated_gzip(b"first"));
    partition.extend(truncated_gzip(b"second payload"));
    let fixture = fixture_with_schema_and_partition(&schema_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["partitions", fixture.path().to_str().unwrap(), "--members"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Partitions/69\tstored="));
    assert!(stdout.contains("members=2"));
    assert!(stdout.contains("member=0\tstored_offset=44\tlogical_offset=44"));
    assert!(stdout.contains("Validated gzip members: 2"));
    assert!(stdout.contains("Decoded bytes: 19"));
    assert!(stdout.contains("Candidate failures: 0"));
}

#[test]
fn elem_table_reports_layout_without_listing_ids_by_default() {
    let fixture = fixture_with_elem_table(&elem_table_fixture());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["elem-table", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Framing: truncated gzip at byte 8"));
    assert!(stdout.contains("Declared records: 10"));
    assert!(stdout.contains("Object class index: 1370"));
    assert!(stdout.contains("Parsed records: 10"));
    assert!(stdout.contains("Unique element IDs: 10"));
    assert!(stdout.contains("Preserved trailing bytes: 3"));
    assert!(!stdout.contains("Record inventory:"));
}

#[test]
fn elem_table_cleans_known_checksum_pages_before_inflation() {
    let fixture = fixture_with_elem_table(&checksum_paged_elem_table_fixture());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["elem-table", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Checksum-page trailers stripped: true"));
    assert!(stdout.contains("Declared records: 10"));
    assert!(stdout.contains("Parsed records: 10"));
    assert!(stdout.contains("Preserved trailing bytes: 90000"));
}

#[test]
fn partition_id_probe_reports_aggregate_overlap_without_ids() {
    let mut payload = 1_u32.to_le_bytes().to_vec();
    payload.extend(b"member payload");
    let partition = truncated_gzip(&payload);
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["partition-id-probe", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Candidate element IDs: 10"));
    assert!(stdout.contains("Validated members: 1"));
    assert!(stdout.contains("Members whose first u32 is a candidate ID: 1"));
    assert!(stdout.contains("Distinct overlapping IDs: 1"));
    assert!(stdout.contains("Member hit rate: 100.000%"));
    assert!(!stdout.contains("primary="));
}

#[test]
fn schema_prefix_probe_counts_candidates_during_streaming_decode() {
    let mut payload = [12_u16.to_le_bytes(), 0_u16.to_le_bytes()].concat();
    payload.extend(b"member payload");
    let partition = truncated_gzip(&payload);
    let fixture = fixture_with_schema_and_partition(&schema_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "schema-prefix-probe",
            fixture.path().to_str().unwrap(),
            "--class",
            "Element",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Class: Element"));
    assert!(stdout.contains("Schema index: 12"));
    assert!(stdout.contains("Members with candidates: 1"));
    assert!(stdout.contains("Candidate prefixes: 1"));
}

#[test]
fn flags_probe_reports_the_width_check_over_a_walked_class() {
    let mut payload = [12_u16.to_le_bytes(), 0_u16.to_le_bytes()].concat();
    payload.extend(b"member payload");
    let partition = truncated_gzip(&payload);
    let fixture = fixture_with_schema_and_partition(&schema_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "flags-probe",
            fixture.path().to_str().unwrap(),
            "--class",
            "Element",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Class: Element"));
    assert!(stdout.contains("Records walked:"));
    assert!(stdout.contains("Node GInfo objects met:"));
    // The check reports the disagreement count even when it is zero, so a
    // regression shows up as a number rather than as a missing line.
    assert!(stdout.contains("where the walk read a different width:"));
}

/// Two markers 32 bytes apart, each followed by an ID the fixture's
/// `Global/ElemTable` also declares.
fn marker_payload() -> Vec<u8> {
    let mut payload = vec![0x7f; 80];
    for (offset, id) in [(8_usize, 3_u32), (40, 4)] {
        payload[offset..offset + 2].copy_from_slice(&12_u16.to_le_bytes());
        payload[offset + 2..offset + 4].copy_from_slice(&0_u16.to_le_bytes());
        payload[offset + 4..offset + 8].copy_from_slice(&id.to_le_bytes());
    }
    payload
}

#[test]
fn marker_envelopes_report_record_boundary_evidence() {
    let partition = truncated_gzip(&marker_payload());
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "marker-envelopes",
            fixture.path().to_str().unwrap(),
            "--class",
            "Element",
            "--leading",
            "8",
            "--trailing",
            "8",
            "--dump",
            "1",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Class: Element"));
    assert!(stdout.contains("Schema index: 12"));
    assert!(stdout.contains("Element-ID reference: Global/ElemTable (10 IDs)"));
    assert!(stdout.contains("Marker candidates: 2"));
    assert!(stdout.contains("Captured envelopes: 2"));
    assert!(stdout.contains("Partitions stopped by envelope budget: 0"));
    assert!(stdout.contains("Envelopes with 8 leading bytes: 2 (distinct patterns: 1)"));
    assert!(stdout.contains("leading=7f7f7f7f7f7f7f7f	count=2	share=100.000%"));
    assert!(stdout.contains("gap=32	count=1	share=100.000%"));
    assert!(stdout.contains("Post-marker u32 samples: 2 (distinct values: 2)"));
    assert!(stdout.contains("Samples whose u32 is a candidate ID: 2 (100.000%)"));
    assert!(stdout.contains("marker=0c000000"));
    assert_eq!(stdout.matches("decoded_offset=").count(), 1);
}

#[test]
fn marker_envelopes_report_the_capture_budget_without_losing_candidates() {
    let partition = truncated_gzip(&marker_payload());
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "marker-envelopes",
            fixture.path().to_str().unwrap(),
            "--class",
            "Element",
            "--max-envelopes",
            "1",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Marker candidates: 2"));
    assert!(stdout.contains("Captured envelopes: 1"));
    assert!(stdout.contains("Partitions stopped by envelope budget: 1"));
    assert!(stdout.contains("Consecutive same-member candidate gaps: 0"));
}

/// Two 16-byte-header records whose bodies tile the member exactly.
fn member_record_payload() -> Vec<u8> {
    let mut payload = Vec::new();
    for (index, body) in [(1_u32, 24_usize), (2, 8)] {
        let mut header = vec![0_u8; 16];
        header[..4].copy_from_slice(&index.to_le_bytes());
        header[8..12].copy_from_slice(&u32::try_from(body).unwrap().to_le_bytes());
        payload.extend(header);
        payload.extend(vec![0x5a; body]);
    }
    payload
}

/// A descriptor followed by its member, as partition streams store them.
fn framed_partition(
    payload: &[u8],
    record_count: u32,
    body_bytes: u32,
    format_tag: u32,
) -> Vec<u8> {
    let member = truncated_gzip(payload);
    let mut descriptor = vec![0_u8; 40];
    descriptor[14..16].copy_from_slice(&0x0e4e_u16.to_le_bytes());
    descriptor[20..24].copy_from_slice(&record_count.to_le_bytes());
    descriptor[24..28].copy_from_slice(&(u32::try_from(member.len()).unwrap() + 16).to_le_bytes());
    descriptor[28..32].copy_from_slice(&body_bytes.to_le_bytes());
    descriptor[32..36].copy_from_slice(&format_tag.to_le_bytes());

    let mut stored = descriptor;
    stored.extend(member);
    stored
}

#[test]
fn member_framing_validates_descriptors_and_the_record_array() {
    let payload = member_record_payload();
    let partition = framed_partition(&payload, 2, 32, 102);
    let fixture = fixture_with_schema_and_partition(&schema_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "member-framing",
            fixture.path().to_str().unwrap(),
            "--dump",
            "1",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Members with a descriptor: 1"));
    assert!(stdout.contains("Stored span == compressed + 16: 1 (100.000%)"));
    assert!(stdout.contains("Format tags: 102=1"));
    assert!(stdout.contains("Members walked without an error: 1 (100.000%)"));
    assert!(stdout.contains("Members ending inside a record: 0"));
    assert!(stdout.contains("Record count == descriptor count: 1 (100.000%)"));
    assert!(stdout.contains("Body bytes == descriptor body bytes: 1 (100.000%)"));
    assert!(stdout.contains("Records recovered: 2"));
    assert!(stdout.contains("record\toffset=0\theader=16\tbody=24"));
}

#[test]
fn member_framing_carries_a_record_into_the_next_member() {
    // The first member ends four bytes into the second record's body; the
    // second member holds that tail and nothing else.
    let mut first = member_record_payload();
    let tail = first.split_off(first.len() - 4);
    let mut partition = framed_partition(&first, 2, 32, 102);
    partition.extend(framed_partition(&tail, 0, 0, 102));
    let fixture = fixture_with_schema_and_partition(&schema_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["member-framing", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Members with a known format tag: 2"));
    assert!(stdout.contains("Members walked without an error: 2 (100.000%)"));
    assert!(stdout.contains("Members ending inside a record: 1"));
    assert!(
        stdout
            .contains("Members resuming after a carried tail: 1 (ending on a record boundary: 1)")
    );
    assert!(stdout.contains("Carries dropped at a walk failure: 0"));
    assert!(stdout.contains("Record count == descriptor count: 2 (100.000%)"));
    assert!(stdout.contains("Body bytes == descriptor body bytes: 2 (100.000%)"));
    assert!(stdout.contains("Records recovered: 2"));
}

#[test]
fn member_framing_reports_a_header_cut_by_the_payload_end() {
    let mut payload = member_record_payload();
    // Leave eight bytes of a final record header with no room for the rest.
    payload.extend([0; 8]);
    let partition = framed_partition(&payload, 2, 32, 102);
    let fixture = fixture_with_schema_and_partition(&schema_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["member-framing", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Members with a known format tag: 1"));
    assert!(stdout.contains("Members walked without an error: 0 (0.000%)"));
    assert!(stdout.contains("Records recovered: 0"));
    assert!(stdout.contains("failure\tcount=1\treason=record header at 64 is truncated"));
}

#[test]
fn dump_member_writes_the_inflated_payload() {
    let payload = member_record_payload();
    let partition = framed_partition(&payload, 2, 32, 102);
    let fixture = fixture_with_schema_and_partition(&schema_fixture(), &partition);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "dump-member",
            fixture.path().to_str().unwrap(),
            "Partitions/69",
            "40",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, payload);
}

/// One record with an explicit identifier, class index, and body length.
fn record(wide: bool, id: u32, class_index: u16, body: usize) -> Vec<u8> {
    let header_bytes = if wide { 16 } else { 12 };
    let length_offset = if wide { 8 } else { 4 };
    let trailing = if wide { 12 } else { 8 };
    let mut header = vec![0_u8; header_bytes];
    header[..4].copy_from_slice(&id.to_le_bytes());
    header[length_offset..length_offset + 4]
        .copy_from_slice(&u32::try_from(body).unwrap().to_le_bytes());
    header[trailing..trailing + 2].copy_from_slice(&class_index.to_le_bytes());

    let mut bytes = header;
    bytes.extend(vec![0x5a; body]);
    bytes
}

/// A header member (tag 101) and an element member (tag 102) for ids 1 and 2.
fn object_partition() -> Vec<u8> {
    let mut narrow = record(false, 1, 12, 8);
    narrow.extend(record(false, 2, 12, 4));
    let mut wide = record(true, 1, 12, 16);
    wide.extend(record(true, 2, 12, 8));

    let mut partition = framed_partition(&narrow, 2, 12, 101);
    partition.extend(framed_partition(&wide, 2, 24, 102));
    partition
}

#[test]
fn records_resolve_identifiers_and_classes() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["records", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Records: 4"));
    assert!(stdout.contains("Narrow (12-byte) / wide (16-byte) headers: 2 / 2"));
    assert!(stdout.contains("Distinct values: 2"));
    assert!(stdout.contains("Records whose lead is a candidate ID: 4 (100.000%)"));
    assert!(stdout.contains("Format tag 101: 2 records, resolved 2 (100.000%)"));
    assert!(stdout.contains("Format tag 102: 2 records, resolved 2 (100.000%)"));
    assert!(stdout.contains("12\tElement\tcount=2"));
}

#[test]
fn inspect_reports_the_object_inventory() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["inspect", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Objects discovered: 2"));
    assert!(stdout.contains("Records: 4"));
    assert!(stdout.contains("Records with a resolved class: 4 (100.000%)"));
    assert!(stdout.contains("Identifiers also in Global/ElemTable: 2 (20.000% of the table)"));
    assert!(stdout.contains("Element classes (format tag 102): 1"));
    assert!(stdout.contains("12\tElement\tcount=2"));
}

#[test]
fn inspect_can_skip_the_partition_walk() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "inspect",
            fixture.path().to_str().unwrap(),
            "--streams-only",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Partitions/*: 1 stream(s)"));
    assert!(!stdout.contains("Objects discovered"));
}

#[test]
fn element_lists_every_record_for_one_identifier() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "element",
            fixture.path().to_str().unwrap(),
            "2",
            "--bytes",
            "4",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Element 2:"));
    assert!(stdout.contains("format=101\tclass=12 Element\tbody=4"));
    assert!(stdout.contains("format=102\tclass=12 Element\tbody=8"));
    assert!(stdout.contains("body=5a5a5a5a"));
    assert!(stdout.contains("Records: 2"));
}

#[test]
fn element_reports_an_identifier_that_is_absent() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["element", fixture.path().to_str().unwrap(), "9999"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Records: 0"));
    assert!(stdout.contains("No record carries this identifier."));
}

#[test]
fn schema_lists_the_properties_of_one_class() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "schema",
            fixture.path().to_str().unwrap(),
            "--class",
            "Element",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Class 12: Element"));
    assert!(stdout.contains("Properties: 1"));
    assert!(stdout.contains("0\tId\ttype=Integer32Alternate\twidth=4"));
    assert!(!stdout.contains("Class inventory:"));
}

#[test]
fn schema_rejects_an_unknown_class_name() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "schema",
            fixture.path().to_str().unwrap(),
            "--class",
            "NoSuchClass",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("schema class not found: NoSuchClass"));
}

#[test]
fn bodies_print_record_payloads_of_one_class() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "bodies",
            fixture.path().to_str().unwrap(),
            "--class",
            "Element",
            "--tag",
            "102",
            "--bytes",
            "4",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Bodies of class 12 Element:"));
    assert!(stdout.contains("id=1\ttag=102\tlen=16\tcompanion=0\tbody=5a5a5a5a"));
    assert!(stdout.contains("id=2\ttag=102\tlen=8"));
    assert!(stdout.contains("Records printed: 2"));
}

/// An integer set and a text set, as an element stores its parameters.
fn parameter_sets(built_in: i32, value: i32, text: &str) -> Vec<u8> {
    let mut bytes = 1_u32.to_le_bytes().to_vec();
    bytes.extend(built_in.to_le_bytes());
    bytes.extend(value.to_le_bytes());
    bytes.extend(1_u32.to_le_bytes());
    bytes.extend((-1_001_203_i32).to_le_bytes());
    bytes.extend(encoded_utf16(text));
    bytes
}

fn encoded_utf16(value: &str) -> Vec<u8> {
    let units = value.encode_utf16().collect::<Vec<_>>();
    let mut bytes = u32::try_from(units.len()).unwrap().to_le_bytes().to_vec();
    for unit in units {
        bytes.extend(unit.to_le_bytes());
    }
    bytes
}

/// A tag-102 record whose body carries the `Element` tail after its `m_id`,
/// followed by a length-prefixed UTF-16 name.
fn element_body(id: u32, level: i32, name: &str) -> Vec<u8> {
    let mut body = vec![0_u8; 4];
    body.extend(1_u32.to_le_bytes());
    body.extend(id.to_le_bytes());
    body.extend(level.to_le_bytes());
    for _ in 0..5 {
        body.extend((-1_i32).to_le_bytes());
    }
    body.extend((-4_i32).to_le_bytes());
    body.extend([0, 0, 0]);

    let units = name.encode_utf16().collect::<Vec<_>>();
    body.extend(u32::try_from(units.len()).unwrap().to_le_bytes());
    for unit in units {
        body.extend(unit.to_le_bytes());
    }
    body
}

#[test]
fn export_json_emits_one_object_per_element() {
    let body = element_body(1, 77, "01 Этаж");
    let mut wide = record(true, 1, 12, body.len());
    let header_len = wide.len() - body.len();
    wide.truncate(header_len);
    wide.extend(&body);
    let partition = framed_partition(&wide, 1, u32::try_from(body.len()).unwrap(), 102);
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &partition);

    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["export-json", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout.lines().next().unwrap();
    assert!(line.contains("\"id\":1"));
    assert!(line.contains("\"class_index\":12"));
    assert!(line.contains("\"class\":\"Element\""));
    assert!(line.contains("\"level_id\":77"));
    assert!(line.contains("\"name\":\"01 Этаж\""));
    assert!(line.contains("\"name_source\":\"offset\""));
    assert!(line.contains("\"design_option_id\":-4"));
    assert!(line.contains("\"records\":1"));
    assert!(
        line.contains("\"source\":{\"partition\":\"Partitions/69\",\"member\":0,\"offset\":0}")
    );
    assert!(!line.contains("family_id"));
    assert_eq!(stdout.lines().count(), 1);
}

#[test]
fn export_json_full_indexes_the_model_before_the_elements() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["export-json", fixture.path().to_str().unwrap(), "--full"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut lines = stdout.lines();
    // The model line indexes what follows: one line per element, and a class
    // histogram naming the sections those lines fall into.
    let model = lines.next().unwrap();
    assert!(model.starts_with("{\"kind\":\"model\""), "{model}");
    assert!(model.contains("\"elements\":2"), "{model}");
    assert!(model.contains("\"records\":4"), "{model}");
    assert!(model.contains("\"with_class\":2"), "{model}");
    assert!(
        model.contains("\"bodies\":{\"elements\":0,\"records\":0"),
        "{model}"
    );
    assert!(
        model.contains("\"units\":{\"length\":\"meters\""),
        "{model}"
    );
    assert!(
        model.contains("{\"index\":12,\"name\":\"Element\",\"elements\":2}"),
        "{model}"
    );
    assert_eq!(lines.count(), 2);

    // Without the flag the model line is not written at all, so an existing
    // reader of the element lines sees the same file it did before.
    let plain = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["export-json", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(!plain.contains("\"kind\":\"model\""), "{plain}");
}

#[test]
fn export_json_writes_to_a_file_and_honours_the_limit() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let target = NamedTempFile::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-json",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
            "--limit",
            "1",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Elements written: 1 of 2"));
    let written = std::fs::read_to_string(target.path()).unwrap();
    assert_eq!(written.lines().count(), 1);
    assert!(written.contains("\"id\":1"));
}

#[test]
fn export_ifc_writes_an_ifc4_spatial_model() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let target = NamedTempFile::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-ifc",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
            "--model-namespace",
            "00112233-4455-6677-8899-aabbccddeeff",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Model namespace: 00112233-4455-6677-8899-aabbccddeeff"));
    assert!(stdout.contains("Building storeys: 0"));
    assert!(stdout.contains("Elements: 0"));
    let written = std::fs::read_to_string(target.path()).unwrap();
    assert!(written.starts_with("ISO-10303-21;\nHEADER;"));
    assert!(written.contains("FILE_SCHEMA(('IFC4'))"));
    assert!(written.contains("=IFCPROJECT("));
    assert!(written.contains("=IFCSITE("));
    assert!(written.contains("=IFCBUILDING("));
    assert!(written.ends_with("END-ISO-10303-21;\n"));
}

/// The setup a run used can be written out and read back, and a flag beats
/// what the file says - which is what makes a saved setup reusable with one
/// thing changed.
#[test]
fn export_ifc_saves_and_reuses_an_export_setup() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let target = NamedTempFile::new().unwrap();
    let setup = NamedTempFile::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-ifc",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
            "--length-unit",
            "millimetre",
            "--no-types",
            "--write-settings",
            setup.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Length unit: millimetre")
    );

    let written = std::fs::read_to_string(setup.path()).unwrap();
    assert!(
        written.contains("\"length-unit\": \"millimetre\""),
        "{written}"
    );
    assert!(written.contains("\"types\": false"), "{written}");

    // Read back, the same setup produces the same file - and the flag beside
    // it overrides the one setting it names.
    let again = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-ifc",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
            "--settings",
            setup.path().to_str().unwrap(),
            "--length-unit",
            "metre",
        ])
        .output()
        .unwrap();
    assert!(again.status.success());
    assert!(
        String::from_utf8(again.stdout)
            .unwrap()
            .contains("Length unit: metre")
    );
    let ifc = std::fs::read_to_string(target.path()).unwrap();
    assert!(
        ifc.contains("=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.)"),
        "the flag should win"
    );
}

/// A settings file naming a mapping table that cannot be applied fails before
/// the model is read, and says which line is wrong.
#[test]
fn export_ifc_refuses_a_mapping_table_it_cannot_apply() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let target = NamedTempFile::new().unwrap();
    let mut mapping = NamedTempFile::new().unwrap();
    writeln!(mapping, "OST_Walls\t\tIfcWall\t").unwrap();
    writeln!(mapping, "OST_Ceilings\t\tIfcCeiling\t").unwrap();
    mapping.flush().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-ifc",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
            "--class-mapping",
            mapping.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains("IfcCeiling"), "{stderr}");
}

#[test]
fn export_ifc_refuses_to_overwrite_the_source_file() {
    let fixture = fixture();
    let before = std::fs::read(fixture.path()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-ifc",
            fixture.path().to_str().unwrap(),
            "--output",
            fixture.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("output must not overwrite the source file")
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
}

#[test]
fn names_report_where_a_class_keeps_its_string() {
    let mut wide = Vec::new();
    for (index, name) in [(1_u32, "01 Этаж"), (2, "02 Этаж")] {
        let body = element_body(index, 77, name);
        let mut header = record(true, index, 12, body.len());
        header.truncate(16);
        wide.extend(header);
        wide.extend(body);
    }
    let body_bytes = u32::try_from(wide.len() - 32).unwrap();
    let partition = framed_partition(&wide, 2, body_bytes, 102);
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &partition);

    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["names", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("12\tElement\tbodies=2\toffset=+0\tagreement=100%"));
    assert!(stdout.contains("01 Этаж"));
    assert!(stdout.contains("Classes with a settled string offset: 1"));
}

/// Two elements: one parameter definition named "Этаж", and one element that
/// stores parameters referencing it.
fn parameter_partition() -> Vec<u8> {
    let mut wide = Vec::new();
    for (id, name, extra) in [
        (7_u32, "Этаж", Vec::new()),
        (8, "Труба", parameter_sets(-1_114_242, 0, "153")),
    ] {
        let mut body = element_body(id, 77, name);
        body.extend(extra);
        let mut header = record(true, id, 12, body.len());
        header.truncate(16);
        wide.extend(header);
        wide.extend(body);
    }
    let body_bytes = u32::try_from(wide.len() - 32).unwrap();
    framed_partition(&wide, 2, body_bytes, 102)
}

#[test]
fn parameters_report_values_and_their_identifiers() {
    let fixture =
        fixture_with_elem_table_and_partition(&elem_table_fixture(), &parameter_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["parameters", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Known parameter elements: 0"));
    assert!(stdout.contains("param=-1114242\tint=0"));
    assert!(stdout.contains("param=-1001203\ttext=\\\"153\\\""));
    assert!(stdout.contains("Parameter values recovered: 2"));
}

#[test]
fn export_json_carries_parameter_values() {
    let fixture =
        fixture_with_elem_table_and_partition(&elem_table_fixture(), &parameter_partition());
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(["export-json", fixture.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout
        .lines()
        .find(|line| line.contains("\"id\":8"))
        .unwrap();
    assert!(line.contains("\"parameters\":["));
    // The fixture states release 2026, and 2026 is catalogued, so both codes
    // reach the export under the names Autodesk publishes for them rather
    // than under the `param_<code>` fallback an uncatalogued release leaves.
    assert!(line.contains(
        "{\"id\":-1114242,\"name\":\"Use Annotation Scale\",\
         \"built_in\":\"RBS_FAMILY_CONTENT_ANNOTATION_DISPLAY\",\"int\":0}"
    ));
    assert!(line.contains(
        "{\"id\":-1001203,\"name\":\"Mark\",\"built_in\":\"ALL_MODEL_MARK\",\"text\":\"153\"}"
    ));
}

#[test]
fn export_scene_writes_a_framed_scene_a_reader_can_locate() {
    let fixture = fixture_with_elem_table_and_partition(&elem_table_fixture(), &object_partition());
    let target = NamedTempFile::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Scene written:"), "{stdout}");
    assert!(
        stdout.contains("Elements: 0 (0 carry geometry)"),
        "{stdout}"
    );

    let written = std::fs::read(target.path()).unwrap();
    assert!(written.starts_with(b"RIVETSCN"));
    assert!(written.ends_with(b"RIVETEND"));

    // A reader takes the last 24 bytes and follows them to the manifest. That
    // is the whole contract for finding anything in the file, so the test
    // walks it rather than trusting the writer's own report.
    let trailer = written.len() - 24;
    let offset = u64::from_le_bytes(written[trailer..trailer + 8].try_into().unwrap());
    let stored = u32::from_le_bytes(written[trailer + 8..trailer + 12].try_into().unwrap());
    let length = u32::from_le_bytes(written[trailer + 12..trailer + 16].try_into().unwrap());
    let offset = usize::try_from(offset).unwrap();
    let manifest = inflate(&written[offset..offset + stored as usize]);
    assert_eq!(manifest.len(), length as usize);
    let manifest = String::from_utf8(manifest).unwrap();
    assert!(
        manifest.contains("\"format\":\"rivet-scene\""),
        "{manifest}"
    );
    assert!(manifest.contains("\"unit\":\"metre\""), "{manifest}");
    assert!(manifest.contains("\"kind\":\"rvt\""), "{manifest}");
    assert!(
        manifest.contains("\"application\":\"Autodesk Revit\""),
        "{manifest}"
    );
}

#[test]
fn export_scene_reads_ifc_geometry_levels_and_source_classes() {
    let fixture = ifc_fixture();
    let target = NamedTempFile::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            fixture.path().to_str().unwrap(),
            "--output",
            target.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Entities read: 19"), "{stdout}");
    assert!(stdout.contains("Building storeys: 1"), "{stdout}");
    assert!(
        stdout.contains("Elements: 1 (1 carry geometry)"),
        "{stdout}"
    );

    let written = std::fs::read(target.path()).unwrap();
    let trailer = written.len() - 24;
    let offset = u64::from_le_bytes(written[trailer..trailer + 8].try_into().unwrap());
    let stored = u32::from_le_bytes(written[trailer + 8..trailer + 12].try_into().unwrap());
    let offset = usize::try_from(offset).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&inflate(&written[offset..offset + stored as usize])).unwrap();
    assert_eq!(manifest["source"]["kind"], "ifc");
    assert_eq!(manifest["counts"]["withGeometry"], 1);
    assert_eq!(manifest["levels"][0]["name"], "Ground floor");
    assert_eq!(manifest["ifcClasses"][0]["name"], "IFCWALL");
}

#[test]
fn export_scene_refuses_an_ifc_above_the_safe_parsing_limit_before_reading_it() {
    let fixture = ifc_fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            fixture.path().to_str().unwrap(),
            "--max-ifc-bytes",
            "8",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("safe parsing limit"), "{stderr}");
    // The refusal says what reading it was expected to cost, because "too
    // large" without a number is not something an operator can act on.
    assert!(stderr.contains("expected to need"), "{stderr}");
}

/// The two ceilings were one flag, and that flag's default describes a single
/// gzip member - which is how a 1.8 GiB IFC came to be refused by a number
/// that has nothing to say about it. An IFC is bounded by `--max-ifc-bytes`,
/// and by this host's memory where that is not given.
#[test]
fn the_rvt_member_ceiling_no_longer_bounds_an_ifc_source() {
    let fixture = ifc_fixture();
    let scene = tempfile::Builder::new().suffix(".rvs").tempfile().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            fixture.path().to_str().unwrap(),
            "--output",
            scene.path().to_str().unwrap(),
            "--max-member-bytes",
            "8",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn export_scene_refuses_to_overwrite_the_source_file() {
    let fixture = fixture();
    let before = std::fs::read(fixture.path()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            fixture.path().to_str().unwrap(),
            "--output",
            fixture.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("output must not overwrite the source file")
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
}

#[test]
fn export_scene_rejects_its_options_before_reading_the_file() {
    // The source names a file that does not exist. Reporting the option and
    // not the missing file is what proves the check runs first - on a real
    // model the decode it precedes costs minutes.
    for (flag, value, message) in [
        ("--compression", "12", "compression must be between 0 and 9"),
        (
            "--chord-tolerance-mm",
            "0",
            "chord tolerance must be a positive number of millimetres",
        ),
        (
            "--chunk-triangles",
            "0",
            "a chunk must hold at least one triangle",
        ),
        (
            "--property-block",
            "0",
            "a property block must hold at least one element",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
            .args(["export-scene", "no/such/model.rvt", flag, value])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(message), "{flag}: {stderr}");
    }
}

fn inflate(bytes: &[u8]) -> Vec<u8> {
    let mut decoder = flate2::write::DeflateDecoder::new(Vec::new());
    decoder.write_all(bytes).unwrap();
    decoder.finish().unwrap()
}

/// Two files that each number their elements from scratch. Both name a wall
/// `wall-guid` and a storey `level-guid`, which is exactly the collision a
/// federated model has to survive.
#[test]
fn export_ifc_federates_several_sources_and_keeps_their_identifiers_apart() {
    let directory = tempfile::tempdir().unwrap();
    let architecture = ifc_fixture_in(directory.path(), "architecture.ifc", 0.0);
    let structure = ifc_fixture_in(directory.path(), "structure.ifc", 1.0);
    let target = directory.path().join("federated.ifc");
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-ifc",
            architecture.to_str().unwrap(),
            structure.to_str().unwrap(),
            "--output",
            target.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Federated documents: 2"), "{stdout}");
    assert!(
        stdout.contains("architecture: 1 elements from architecture.ifc"),
        "{stdout}"
    );
    assert!(
        stdout.contains("structure: 1 elements from structure.ifc"),
        "{stdout}"
    );
    // The wall and the storey each occurred twice, and qualification is what
    // kept them four things rather than two.
    assert!(
        stdout.contains("kept apart by qualification: 1"),
        "{stdout}"
    );
    assert!(stdout.contains("Building storeys: 2"), "{stdout}");
    assert!(stdout.contains("Elements: 2"), "{stdout}");
    // The two walls overlap, so nothing is said about their origins.
    assert!(!stdout.contains("different origins"), "{stdout}");

    let written = std::fs::read_to_string(&target).unwrap();
    assert!(written.starts_with("ISO-10303-21;\nHEADER;"));
    assert_eq!(written.matches("IFCWALL(").count(), 2, "{written}");
    assert_eq!(written.matches("IFCBUILDINGSTOREY(").count(), 2);
}

#[test]
fn export_scene_reports_documents_that_are_stated_about_different_origins() {
    let directory = tempfile::tempdir().unwrap();
    let here = ifc_fixture_in(directory.path(), "here.ifc", 0.0);
    let far = ifc_fixture_in(directory.path(), "far.ifc", 100_000.0);
    let target = directory.path().join("federated.rvs");
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            here.to_str().unwrap(),
            far.to_str().unwrap(),
            "--output",
            target.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Reported, and the conversion still finished: nothing here knows the
    // transform that would reconcile two origins, so nothing is moved.
    assert!(stdout.contains("different origins"), "{stdout}");
    assert!(stdout.contains("nothing was moved"), "{stdout}");
    assert!(
        stdout.contains("Elements: 2 (2 carry geometry)"),
        "{stdout}"
    );

    // The scene names both files and says which one each element came from,
    // so a viewer can show one and hide the other.
    let written = std::fs::read(&target).unwrap();
    let trailer = written.len() - 24;
    let offset = u64::from_le_bytes(written[trailer..trailer + 8].try_into().unwrap());
    let stored = u32::from_le_bytes(written[trailer + 8..trailer + 12].try_into().unwrap());
    let offset = usize::try_from(offset).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&inflate(&written[offset..offset + stored as usize])).unwrap();
    let documents = manifest["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 2, "{manifest}");
    let named: Vec<&str> = documents
        .iter()
        .map(|document| document["name"].as_str().unwrap())
        .collect();
    assert!(
        named.contains(&"here.ifc") && named.contains(&"far.ifc"),
        "{named:?}"
    );
    assert_eq!(documents[0]["elements"], 1);
    // Every element indexes a distinct document, and both are still read as
    // IFC so their source class survives.
    let per_element = manifest["elements"]["documents"].as_array().unwrap();
    assert_eq!(per_element.len(), 2);
    assert_ne!(per_element[0], per_element[1]);
    assert_eq!(manifest["ifcClasses"][0]["name"], "IFCWALL");
    // A federated set of IFCs is still an IFC set, not a mixed one.
    assert_eq!(manifest["source"]["kind"], "ifc");
}

#[test]
fn an_export_of_several_sources_will_not_guess_an_output_name() {
    let directory = tempfile::tempdir().unwrap();
    let first = ifc_fixture_in(directory.path(), "first.ifc", 0.0);
    let second = ifc_fixture_in(directory.path(), "second.ifc", 1.0);
    let output = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args([
            "export-scene",
            first.to_str().unwrap(),
            second.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("2 source files have no one name to derive it from")
    );
}
