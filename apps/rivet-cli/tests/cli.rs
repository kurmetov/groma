use std::{io::Write, process::Command};

use flate2::{Compression, write::DeflateEncoder};
use tempfile::NamedTempFile;

fn fixture() -> NamedTempFile {
    fixture_with_schema(&schema_fixture())
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
    let file = NamedTempFile::new().unwrap();
    let mut compound = cfb::create(file.path()).unwrap();
    compound.create_storage("/Formats").unwrap();
    compound.create_storage("/Global").unwrap();
    compound.create_storage("/Partitions").unwrap();

    let mut basic_info = 14_u32.to_le_bytes().to_vec();
    basic_info.extend([0xaa, 0xbb]);
    basic_info.extend([4, 0, 0, 0]);
    for unit in "2026".encode_utf16() {
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

fn decoded_elem_table_fixture(tail_bytes: usize) -> Vec<u8> {
    let record_count = 10_u16;
    let mut decoded = vec![0; 0x1e + usize::from(record_count) * 28 + tail_bytes];
    decoded[..2].copy_from_slice(&9_u16.to_le_bytes());
    decoded[2..4].copy_from_slice(&record_count.to_le_bytes());
    for index in 0..usize::from(record_count) {
        let offset = 0x1e + index * 28;
        if index < 8 {
            decoded[offset..offset + 4].fill(0xff);
        }
        let id = u32::try_from(index + 1).unwrap();
        decoded[offset + 4..offset + 8].copy_from_slice(&id.to_le_bytes());
        decoded[offset + 8..offset + 12].copy_from_slice(&id.to_le_bytes());
    }

    decoded
}

fn checksum_paged_elem_table_fixture() -> Vec<u8> {
    let mut decoded = decoded_elem_table_fixture(90_000);
    let tail_start = 0x1e + 10 * 28;
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
    assert!(stdout.contains("Streams: 5"));
    assert!(stdout.contains("Partitions: 1"));
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
    assert!(stdout.contains("Record framing: explicit-4-byte-marker"));
    assert!(stdout.contains("Records matching marker: 8"));
    assert!(stdout.contains("Parsed records: 10"));
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
    assert!(line.contains("{\"id\":-1114242,\"int\":0}"));
    assert!(line.contains("{\"id\":-1001203,\"text\":\"153\"}"));
}
