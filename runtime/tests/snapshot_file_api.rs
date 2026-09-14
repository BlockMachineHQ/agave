//! This caller uses no dev-context field visibility.
use solana_runtime::serde_snapshot::decode_snapshot_file;
use std::io::{BufReader, Cursor};

#[test]
fn public_decoder_exposes_image_metadata_without_dev_context() {
    let result = decode_snapshot_file(&mut BufReader::new(Cursor::new(Vec::<u8>::new())));
    assert!(result.is_err());
    // Type-check the production metadata surface even though the invalid input
    // above deliberately constructs no snapshot or AccountsDb.
    if let Ok(decoded) = result {
        let _ = (
            decoded.slot(),
            decoded.bank_hash(),
            decoded.accounts_lt_hash(),
            decoded.capitalization(),
            decoded.accounts_data_len(),
            decoded.accounts_slot(),
            decoded.storage_entries().count(),
        );
    }
}
