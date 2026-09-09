#![cfg(feature = "agave-unstable-api")]

use {
    agave_snapshots::hardened_unpack::{SnapshotArchiveEntry, visit_snapshot_archive},
    std::io::{self, Read},
    tar::{Builder, EntryType, Header},
};

#[test]
fn public_visitor_accepts_streaming_zstd_without_filesystem() {
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
    let mut archive = Builder::new(encoder);
    for (name, bytes) in [
        ("version", b"1.2.0".as_slice()),
        ("accounts/001.2", b"account"),
    ] {
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        archive.append_data(&mut header, name, bytes).unwrap();
    }
    let compressed = archive.into_inner().unwrap().finish().unwrap();
    let decoder = zstd::stream::read::Decoder::new(compressed.as_slice()).unwrap();
    let mut seen = Vec::new();
    visit_snapshot_archive(
        decoder,
        |entry: SnapshotArchiveEntry<'_>, reader: &mut dyn Read| {
            assert_eq!(entry.kind, EntryType::Regular);
            assert_eq!(entry.actual_size, entry.apparent_size);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            assert_eq!(bytes.len() as u64, entry.apparent_size);
            seen.push((entry.path.to_path_buf(), bytes));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].0.to_str(), Some("accounts/001.2"));
    assert_eq!(seen[1].1, b"account");

    let decoder = zstd::stream::read::Decoder::new(compressed.as_slice()).unwrap();
    let error = visit_snapshot_archive(decoder, |_, _| Err(io::Error::other("sink closed").into()))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to unpack \"version\": IO error: sink closed")
    );
}
