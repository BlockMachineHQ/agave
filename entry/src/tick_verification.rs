//! Storage-independent tick validation shared with ledger replay.
use {
    crate::entry::{Entry, EntrySlice},
    log::{info, warn},
    solana_clock::Slot,
};

/// Resolved bank and migration inputs. No consensus mode is inferred here.
#[derive(Clone, Copy, Debug)]
pub struct TickVerificationParams {
    pub slot: Slot,
    pub tick_height: u64,
    pub max_tick_height: u64,
    pub hashes_per_tick: Option<u64>,
    pub alpenglow_ticks: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickVerificationError {
    TooManyTicks,
    TooFewTicks,
    TrailingEntry,
    InvalidLastTick,
    InvalidTickHashCount,
}

/// Verify a segment in native ledger error order. Structural failures do not
/// touch the hash counter; hash verification retains its partial-failure effects.
pub fn verify_ticks(
    params: TickVerificationParams,
    entries: &[Entry],
    slot_full: bool,
    tick_hash_count: &mut u64,
) -> Result<(), TickVerificationError> {
    let next_bank_tick_height = params.tick_height + entries.tick_count();
    let max_bank_tick_height = params.max_tick_height;
    let slot = params.slot;
    if next_bank_tick_height > max_bank_tick_height {
        warn!("Too many entry ticks found in slot: {slot}");
        return Err(TickVerificationError::TooManyTicks);
    }
    if next_bank_tick_height < max_bank_tick_height && slot_full {
        info!("Too few entry ticks found in slot: {slot}");
        return Err(TickVerificationError::TooFewTicks);
    }
    if next_bank_tick_height == max_bank_tick_height {
        let has_trailing_entry = entries.last().map(|e| !e.is_tick()).unwrap_or_default();
        if has_trailing_entry {
            warn!("Slot: {slot} did not end with a tick entry");
            return Err(TickVerificationError::TrailingEntry);
        }
        if !slot_full {
            warn!("Slot: {slot} was not marked full");
            return Err(TickVerificationError::InvalidLastTick);
        }
    }
    if params.alpenglow_ticks {
        if entries.iter().any(|entry| entry.num_hashes != 1) {
            warn!("Alpenglow entry with invalid num_hashes found in slot: {slot}");
            return Err(TickVerificationError::InvalidTickHashCount);
        }
        return Ok(());
    }
    let hashes_per_tick = params.hashes_per_tick.unwrap_or(0);
    if !entries.verify_tick_hash_count(tick_hash_count, hashes_per_tick) {
        warn!("Tick with invalid number of hashes found in slot: {slot}");
        return Err(TickVerificationError::InvalidTickHashCount);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::entry::create_ticks, solana_hash::Hash,
        solana_transaction::versioned::VersionedTransaction,
    };

    fn params() -> TickVerificationParams {
        TickVerificationParams {
            slot: 2,
            tick_height: 4,
            max_tick_height: 6,
            hashes_per_tick: Some(4),
            alpenglow_ticks: false,
        }
    }

    #[test]
    fn actual_height_and_skipped_slots() {
        let entries = create_ticks(2, 4, Hash::default());
        assert_eq!(verify_ticks(params(), &entries, true, &mut 0), Ok(()));
        let incomplete = TickVerificationParams {
            tick_height: 2,
            ..params()
        };
        assert_eq!(
            verify_ticks(incomplete, &entries, true, &mut 0),
            Err(TickVerificationError::TooFewTicks)
        );
        let skipped = TickVerificationParams {
            slot: 5,
            max_tick_height: 12,
            ..params()
        };
        assert_eq!(
            verify_ticks(skipped, &create_ticks(8, 4, Hash::default()), true, &mut 0),
            Ok(())
        );
    }

    #[test]
    fn structural_error_order_precedes_hash_counter_effects() {
        let mut entries = create_ticks(2, 3, Hash::default());
        entries.push(Entry {
            num_hashes: 9,
            transactions: vec![VersionedTransaction::default()],
            ..Entry::default()
        });
        for (height, full, expected) in [
            (5, true, TickVerificationError::TooManyTicks),
            (3, true, TickVerificationError::TooFewTicks),
            (4, false, TickVerificationError::TrailingEntry),
            (4, true, TickVerificationError::TrailingEntry),
        ] {
            let mut count = 7;
            assert_eq!(
                verify_ticks(
                    TickVerificationParams {
                        tick_height: height,
                        ..params()
                    },
                    &entries,
                    full,
                    &mut count
                ),
                Err(expected)
            );
            assert_eq!(count, 7);
        }
        entries.pop();
        let mut count = 7;
        assert_eq!(
            verify_ticks(params(), &entries, false, &mut count),
            Err(TickVerificationError::InvalidLastTick)
        );
        assert_eq!(count, 7);
        assert_eq!(
            verify_ticks(params(), &entries, true, &mut count),
            Err(TickVerificationError::InvalidTickHashCount)
        );
        assert_eq!(count, 10);
    }

    #[test]
    fn segmented_hash_counter_and_resolved_modes() {
        let tx = Entry {
            num_hashes: 3,
            transactions: vec![VersionedTransaction::default()],
            ..Entry::default()
        };
        let mut count = 0;
        assert_eq!(verify_ticks(params(), &[tx], false, &mut count), Ok(()));
        assert_eq!(count, 3);
        let mut ticks = create_ticks(1, 1, Hash::default());
        ticks.extend(create_ticks(1, 4, ticks[0].hash));
        assert_eq!(verify_ticks(params(), &ticks, true, &mut count), Ok(()));
        assert_eq!(count, 0);
        let low_power = TickVerificationParams {
            hashes_per_tick: None,
            ..params()
        };
        count = 17;
        assert_eq!(verify_ticks(low_power, &ticks, true, &mut count), Ok(()));
        assert_eq!(count, 17);
        let alpenglow = TickVerificationParams {
            tick_height: 5,
            alpenglow_ticks: true,
            ..params()
        };
        assert_eq!(
            verify_ticks(alpenglow, &ticks[..1], true, &mut count),
            Ok(())
        );
        assert_eq!(count, 17);
        assert_eq!(
            verify_ticks(alpenglow, &ticks[1..], true, &mut count),
            Err(TickVerificationError::InvalidTickHashCount)
        );
        assert_eq!(count, 17);
    }
}
