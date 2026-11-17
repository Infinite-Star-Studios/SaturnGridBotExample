//! The wallet key file: JSON `{"wif": "<WIF>"}`, readable by its owner only
//! (mode 0600, the same format as Saturn's P2P bot kit). Keys are read only
//! from this file: never from the command line, an environment variable or the config.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use zeroize::{Zeroize, Zeroizing};

use crate::phantasma::keys::KeyPair;

const MAX_KEY_FILE: u64 = 4096;

pub fn load(path: &Path) -> Result<KeyPair, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("cannot open key file {}: {e}", path.display()))?;
    let meta = f.metadata().map_err(|e| format!("cannot read key file {}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() > MAX_KEY_FILE {
        return Err(format!("{} is not a key file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "key file {} can be read by other users (mode {mode:o}). Run: chmod 600 {}",
                path.display(),
                path.display()
            ));
        }
    }
    let mut buf = Zeroizing::new(Vec::with_capacity(256));
    f.read_to_end(&mut buf).map_err(|e| format!("cannot read key file {}: {e}", path.display()))?;
    let mut v: serde_json::Value =
        serde_json::from_slice(&buf).map_err(|_| format!("key file {} is not JSON like {{\"wif\": \"…\"}}", path.display()))?;
    let wif = Zeroizing::new(v.get("wif").and_then(|w| w.as_str()).unwrap_or_default().to_string());
    if let Some(serde_json::Value::String(s)) = v.get_mut("wif") {
        s.zeroize();
    }
    if wif.is_empty() {
        return Err(format!("key file {} has no \"wif\"", path.display()));
    }
    let kp = KeyPair::from_wif(&wif).map_err(|e| format!("key file {}: {e}", path.display()))?;
    if !kp.address().is_user() {
        return Err(format!("key file {} does not hold a user key", path.display()));
    }
    Ok(kp)
}

/// Make a new random key and write it to `path` (never overwrites; mode 0600).
pub fn create(path: &Path) -> Result<KeyPair, String> {
    let mut sk = Zeroizing::new([0u8; 32]);
    getrandom::fill(sk.as_mut()).map_err(|e| format!("no secure randomness available: {e}"))?;
    let kp = KeyPair::from_private_key(&sk);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} already exists; refusing to overwrite a key", path.display())
        } else {
            format!("cannot create {}: {e}", path.display())
        }
    })?;
    let body = Zeroizing::new(format!("{{\"wif\": \"{}\"}}\n", kp.to_wif().as_str()));
    f.write_all(body.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(kp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_load() {
        let dir = std::env::temp_dir().join(format!("saturn-grid-key-{}", std::process::id()));
        let path = dir.join("k.json");
        let made = create(&path).unwrap();
        assert!(create(&path).is_err(), "never overwrites");
        let loaded = load(&path).unwrap();
        assert_eq!(made.address(), loaded.address());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load(&path).unwrap_err().contains("chmod 600"));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
