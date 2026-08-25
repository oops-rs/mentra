//! [`LeaseStore`] on advisory OS file locks (`flock`/`LockFileEx`).
//!
//! A lease is an exclusive lock on `leases/<key>.lock`, held by the store
//! for as long as the lease is: acquisition is `try_lock_exclusive`, release
//! drops the handle, and a process that dies releases every lock it held —
//! which is exactly the lifetime a resume lease wants, so the TTL the trait
//! carries has nothing left to time out and is ignored. Cross-process
//! exclusion is real: a second store on the same root, in this process or
//! another, is refused while the lock is held.
//!
//! Lock files are never unlinked. Deleting one while another process waits
//! on the same inode is the classic re-create race that hands two holders
//! the "same" lease on different inodes; an empty leftover file per key is
//! the cheap price of never entering it.

use std::{fs::File, fs::OpenOptions, time::Duration};

use fs2::FileExt as _;

use super::{
    super::store::LeaseStore, FileRuntimeStore, RuntimeError, fs_util, lock_unpoisoned, store_error,
};

/// A lease this store currently holds: dropping the handle releases the OS
/// lock.
pub(super) struct HeldLease {
    owner: String,
    _file: File,
}

impl LeaseStore for FileRuntimeStore {
    fn acquire_lease(&self, key: &str, owner: &str, _ttl: Duration) -> Result<bool, RuntimeError> {
        let mut held = lock_unpoisoned(&self.held_leases);
        // Matches the SQLite store: an existing lease refuses every
        // acquirer, its current owner included.
        if held.contains_key(key) {
            return Ok(false);
        }

        let dir = self.leases_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| store_error(&format!("create '{}'", dir.display()), error))?;
        let path = dir.join(format!("{}.lock", fs_util::encode_component(key)));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .map_err(|error| store_error(&format!("open '{}'", path.display()), error))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                held.insert(
                    key.to_string(),
                    HeldLease {
                        owner: owner.to_string(),
                        _file: file,
                    },
                );
                Ok(true)
            }
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                Ok(false)
            }
            Err(error) => Err(store_error(&format!("lock '{}'", path.display()), error)),
        }
    }

    fn release_lease(&self, key: &str, owner: &str) -> Result<(), RuntimeError> {
        let mut held = lock_unpoisoned(&self.held_leases);
        if held.get(key).is_some_and(|lease| lease.owner == owner) {
            held.remove(key);
        }
        Ok(())
    }
}
