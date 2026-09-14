//! Bank-local account storage for the external backend experiment.
//!
//! The genesis view must start empty. Snapshot views attach a verified image. Views
//! must preserve exact parent visibility, including tombstones, after root/drop.
use {
    crate::accounts_db::AccountsDb,
    crate::accounts_hash::AccountsLtHash,
    solana_account::AccountSharedData,
    solana_clock::{BankId, Slot},
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::{fmt::Debug, ops::Deref, sync::Arc},
};

pub type BackendResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BankIdentity {
    pub slot: Slot,
    pub bank_id: BankId,
}

/// Results of independently verifying the complete, merged account image.
/// `bank_hash` is the serialized Bank hash, NOT the snapshot archive hash
/// (the latter is the checksum of `accounts_lt_hash`). The provider must bind
/// these values to the actual image it exposes, including its modification slots.
#[derive(Clone, Debug)]
pub struct VerifiedSnapshotImage {
    pub slot: Slot,
    pub bank_hash: Hash,
    pub accounts_lt_hash: AccountsLtHash,
    pub capitalization: u64,
    pub accounts_data_len: u64,
}

/// Storage operations only: native Bank still owns account selection, locks,
/// sysvars, stakes, fees and hashing. Errors are terminal because native callers
/// cannot roll back the protocol side effects preceding a store.
///
/// This is a trusted storage-provider/native-framework contract, like direct
/// AccountsDb access, not a sandbox boundary. Providers and callers retaining raw
/// handles MUST use native Bank protocol methods for account mutation and freeze,
/// and BankForks for root selection/publication. Arbitrary clients must not call
/// `store`, `seal`, `apply_root`, `mark_root`, or other lifecycle methods directly:
/// doing so bypasses native hashing, locks, protocol side effects and root ordering.
/// These methods are callbacks for the native framework; isolated provider tests
/// may exercise the contract directly. Read-only consumers should use Bank's
/// frozen external account pin instead of obtaining a raw provider handle.
pub trait ExternalAccountBackend: Debug + Send + Sync {
    fn identity(&self) -> BankIdentity;
    fn parent_identity(&self) -> Option<BankIdentity>;
    /// Called once before native genesis initialization; refuse nonempty views.
    fn initialize_empty_genesis(&self) -> BackendResult<()>;
    /// Attach exactly this verified image to a fresh `(slot, BankId 0)` view
    /// without a parent. Refuse a reused view or a different image. This callback
    /// begins native consumption of the view; earlier decode/input failures do not
    /// consume it. Wrappers requiring one-shot attempts must poison earlier errors
    /// independently. Native restore reconstructs feature/reward caches with an
    /// already frozen Bank whose store paths reject writes. `seal` is explicit
    /// after verification; it is not completion of a new restore-write hash phase.
    /// No root receipt is implied: BankForks still performs the root barrier.
    /// A restore that fails after this callback begins consumes the view; it must
    /// not be retried or published.
    fn initialize_verified_snapshot(&self, _image: &VerifiedSnapshotImage) -> BackendResult<()> {
        Err("snapshot attachment is not supported by this backend".into())
    }
    /// Return None only for absence/tombstones, never for an I/O failure.
    fn load(&self, key: &Pubkey) -> BackendResult<Option<(AccountSharedData, Slot)>>;
    /// Preserve input order, including repeated keys and zero-lamport tombstones.
    fn store(&self, accounts: &[(Pubkey, AccountSharedData)]) -> BackendResult<()>;
    fn fork_child(&self, child: BankIdentity) -> BackendResult<Arc<dyn ExternalAccountBackend>>;
    /// For executed Banks, called after native deferred writes and hash completion.
    /// For snapshot attachment, called after verification of the already frozen
    /// reconstructed Bank, without re-running freeze or a restore-write phase.
    fn seal(&self) -> BackendResult<()>;
    /// Pre-publication barrier on the selected target's backend. `path` is nonempty,
    /// oldest-to-newest, ending at `self.identity()`: exact frozen native Bank
    /// identities linked by actual parents, including any retained rooted prefix.
    /// The first entry may have a historical parent detached by an earlier squash.
    /// Verify sealed views and exact ancestry, apply only the not-yet-rooted suffix
    /// monotonically, and retain receipts for every subsequent `mark_root` in the
    /// path (including repeated marks). Success is not a clean checkpoint. Failure
    /// is terminal, including after partial application; never return success early.
    fn apply_root(&self, path: &[BankIdentity]) -> BackendResult<()>;
    /// Logical mark only. The owner must complete its pre-publication root barrier
    /// first; implementations must refuse a mark without the required receipt.
    fn mark_root(&self) -> BackendResult<()>;
    /// Invalidate the named non-root bank identities without reclaiming memory
    /// still owned by pinned views. Must distinguish same-slot replacements.
    fn remove_unrooted(&self, banks: &[BankIdentity]) -> BackendResult<()>;
    /// Bank lifetime ended; existing readers/descendant views must remain valid.
    fn release_bank(&self) -> BackendResult<()>;
}

/// Native APIs are infallible. Aborting, rather than unwinding, also ensures a
/// worker fault cannot be swallowed by a pool or leave a partially mutated Bank
/// eligible for reuse.
pub fn terminal_backend_result<T>(result: BackendResult<T>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            eprintln!("terminal external account backend failure: {error}");
            std::process::abort();
        }
    }
}

/// Retains the native access surface while explicitly refusing physical native
/// maintenance/scans/snapshot access on an external Bank. No AccountsDb is
/// allocated for that variant.
#[derive(Debug)]
pub struct NativeAccountsDb(pub(crate) Option<Arc<AccountsDb>>);

impl Deref for NativeAccountsDb {
    type Target = Arc<AccountsDb>;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect(
            "unsupported native AccountsDb operation on external account backend (including scans, snapshots and maintenance)",
        )
    }
}
