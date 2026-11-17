//! The bot's saved state: the grid slots, the transaction in flight and the
//! stats. Written atomically (temp file, fsync, rename) after every change,
//! and guarded by an exclusive lock so two bots never trade one grid.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::grid::{Side, Slot, i128_str, u128_str};

pub const STATE_VERSION: u32 = 1;

/// A signed swap that was (or is about to be) broadcast. Saved BEFORE sending,
/// so a restart always resolves it on chain before trading again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub hash: String,
    pub slot: usize,
    pub side: Side,
    #[serde(with = "u128_str")]
    pub amount_in: u128,
    #[serde(with = "u128_str")]
    pub min_out: u128,
    /// Unix seconds; the chain refuses the transaction after this.
    pub expiration: u32,
    /// The signed transaction (public data: it is what goes on chain), to re-broadcast if needed.
    pub signed_hex: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub buys: u64,
    pub sells: u64,
    pub round_trips: u64,
    pub failed: u64,
    /// Realized profit in quote (raw); includes the first sale of inventory brought in.
    #[serde(with = "i128_str")]
    pub realized_pnl_quote: i128,
    /// Profit taken out of the grid (when not compounding), quote raw.
    #[serde(with = "u128_str")]
    pub banked_quote: u128,
    /// Gas paid, KCAL raw.
    #[serde(with = "u128_str")]
    pub gas_kcal: u128,
    pub last_trade_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub paper: bool,
    pub network: String,
    pub pool_id: u64,
    pub base: String,
    pub quote: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    /// A digest of the grid settings; a changed grid is never resumed silently.
    pub fingerprint: String,
    pub created_at: i64,
    pub slots: Vec<Slot>,
    pub pending: Option<Pending>,
    pub stats: Stats,
}

/// Holds the lock file open (and locked) for the life of the process.
pub struct StateFile {
    path: PathBuf,
    _lock: File,
}

impl StateFile {
    /// Lock `path` for this process. Fails if another bot holds it.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let lock_path = with_suffix(path, ".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
        lock.try_lock()
            .map_err(|_| format!("{} is locked: another saturn-grid is running on this state file", path.display()))?;
        Ok(StateFile { path: path.to_path_buf(), _lock: lock })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<State>, String> {
        load(&self.path)
    }

    /// Atomic replace: write a temp file, fsync it, rename it over the old one.
    pub fn save(&self, s: &State) -> Result<(), String> {
        let tmp = with_suffix(&self.path, ".tmp");
        let json = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
        let mut f = File::create(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        f.write_all(&json)
            .and_then(|_| f.write_all(b"\n"))
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        drop(f);
        fs::rename(&tmp, &self.path).map_err(|e| format!("cannot replace {}: {e}", self.path.display()))?;
        #[cfg(unix)]
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }
}

/// Read a state file without locking it (for `status`).
pub fn load(path: &Path) -> Result<Option<State>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let s: State =
                serde_json::from_slice(&bytes).map_err(|e| format!("{} is not a valid state file: {e}", path.display()))?;
            if s.version != STATE_VERSION {
                return Err(format!("{} has state version {}, expected {STATE_VERSION}", path.display(), s.version));
            }
            Ok(Some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Holding;

    #[test]
    fn save_load_and_lock() {
        let dir = std::env::temp_dir().join(format!("saturn-grid-test-{}", std::process::id()));
        let path = dir.join("state.json");
        let sf = StateFile::open(&path).unwrap();
        assert!(StateFile::open(&path).is_err(), "a second bot is refused");
        let s = State {
            version: STATE_VERSION,
            paper: true,
            network: "devnet".into(),
            pool_id: 150,
            base: "SOUL".into(),
            quote: "KCAL".into(),
            base_decimals: 8,
            quote_decimals: 10,
            fingerprint: "x".into(),
            created_at: 1,
            slots: vec![Slot {
                buy_at: u128::MAX,
                sell_at: 2,
                holding: Holding::Quote,
                amount: 3,
                cost_quote: 0,
                round_trips: 0,
            }],
            pending: None,
            stats: Stats { realized_pnl_quote: -5, ..Stats::default() },
        };
        sf.save(&s).unwrap();
        let back = sf.load().unwrap().unwrap();
        assert_eq!(back.slots, s.slots, "u128 survives JSON exactly");
        assert_eq!(back.stats.realized_pnl_quote, -5);
        drop(sf);
        assert!(StateFile::open(&path).is_ok(), "the lock is released with the process's handle");
        let _ = fs::remove_dir_all(&dir);
    }
}
