//! Engine-neutral private-file access and rollback-journal lock state.

use dekopon_capability::StorageInterface;

use crate::{
    StorageHandle, StorageHostError,
    handle::{monotonic_ns, wall_ms},
    key::random_bytes,
};

/// Guest open intent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub create_new: bool,
    pub delete_on_close: bool,
}

/// Durability requested by a guest storage engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Durability {
    Data,
    DataAndMetadata,
    Full,
}

/// Curated rollback-journal lock level.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum LockLevel {
    #[default]
    None,
    Shared,
    Reserved,
    Pending,
    Exclusive,
}

/// Equality-only identity stable for one live logical file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStat {
    pub size: u64,
    pub identity: u64,
}

impl StorageHandle {
    pub fn vfs_open(&mut self, name: &str, options: OpenOptions) -> Result<u64, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        if (!options.read && !options.write)
            || (options.create && options.create_new)
            || ((options.create || options.create_new || options.delete_on_close) && !options.write)
        {
            return Err(StorageHostError::InvalidArgument);
        }
        if (options.write || options.create || options.create_new || options.delete_on_close)
            && self.access != dekopon_capability::StorageAccess::ReadWrite
        {
            return Err(StorageHostError::PermissionDenied);
        }
        if self.handles.len() as u64 >= self.limits.max_handles_per_invocation {
            return Err(StorageHostError::QuotaExceeded);
        }
        let token = self.ensure_entry(name)?;
        let exists = self.entries[&token].exists();
        if options.create_new && exists {
            return Err(StorageHostError::AlreadyExists);
        }
        if !exists && !options.create && !options.create_new {
            return Err(StorageHostError::NotFound);
        }
        let handle = self.next_handle;
        let next_handle = handle.checked_add(1).ok_or(StorageHostError::Arithmetic)?;
        self.ledger.acquire_handle()?;
        let opened = (|| {
            if !exists {
                self.charge_write(0)?;
                let identity = self.allocate_file_identity()?;
                let planned = self.reserve_candidate(&[(&token, Some(0))])?;
                self.write_direct(&token, Some(&[]), planned)?;
                let entry = self.entries.get_mut(&token).expect("resolved entry");
                entry.present(0);
                entry.identity = identity;
            }
            self.handles.insert(
                handle,
                crate::handle::HandleState {
                    token,
                    read: options.read,
                    write: options.write,
                    delete_on_close: options.delete_on_close,
                    lock: LockLevel::None,
                },
            );
            self.next_handle = next_handle;
            Ok(handle)
        })();
        if opened.is_err() {
            self.ledger.release_handle();
        }
        opened
    }

    pub fn vfs_close(&mut self, handle: u64) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        let call = self.note_call();
        let state = self.handles.remove(&handle);
        if state.is_some() {
            // A resource drop always releases native accounting, including after the host-call
            // budget becomes terminal. The budget error still wins over an invalid-handle result,
            // so repeated bad drops cannot mask or bypass the sticky ceiling.
            self.ledger.release_handle();
        }
        call?;
        let state = state.ok_or(StorageHostError::InvalidArgument)?;
        if state.delete_on_close {
            if self.access != dekopon_capability::StorageAccess::ReadWrite {
                return Err(StorageHostError::PermissionDenied);
            }
            self.pending_delete.insert(state.token.clone());
        }
        if self.pending_delete.contains(&state.token)
            && !self
                .handles
                .values()
                .any(|candidate| candidate.token == state.token)
        {
            let planned = self.reserve_candidate(&[(&state.token, None)])?;
            self.write_direct(&state.token, None, planned)?;
            self.entries
                .get_mut(&state.token)
                .ok_or(StorageHostError::Corrupt {
                    scope: "handle-entry",
                })?
                .absent();
            self.pending_delete.remove(&state.token);
        }
        Ok(())
    }

    pub fn vfs_stat(&mut self, name: &str) -> Result<Option<FileStat>, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        let token = self.ensure_entry(name)?;
        Ok(self.entries[&token].size().map(|size| FileStat {
            size,
            identity: self.entries[&token].identity,
        }))
    }

    pub fn vfs_read_at(
        &mut self,
        handle: u64,
        offset: u64,
        maximum: u32,
    ) -> Result<Vec<u8>, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        self.charge_read(u64::from(maximum))?;
        let state = self
            .handles
            .get(&handle)
            .ok_or(StorageHostError::InvalidArgument)?;
        if !state.read {
            return Err(StorageHostError::PermissionDenied);
        }
        let entry = self
            .entries
            .get(&state.token)
            .ok_or(StorageHostError::NotFound)?;
        let size = entry.size().ok_or(StorageHostError::NotFound)?;
        if offset >= size {
            return Ok(Vec::new());
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "TryFromIntError carries only out-of-range, which Arithmetic already states"
        )]
        let length = usize::try_from(u64::from(maximum).min(size - offset))
            .map_err(|_| StorageHostError::Arithmetic)?;
        self.namespace
            .data_directory
            .read_at(&state.token, offset, length)
    }

    pub fn vfs_write_at(
        &mut self,
        handle: u64,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        let state = self
            .handles
            .get(&handle)
            .ok_or(StorageHostError::InvalidArgument)?
            .clone();
        if !state.write {
            return Err(StorageHostError::PermissionDenied);
        }
        // Stat-derived length: a positional write never reads, allocates, or rewrites the bytes it
        // is not replacing, so the file's own size is the only thing this call needs to know.
        let current_length = self
            .entries
            .get(&state.token)
            .and_then(crate::handle::FileEntry::size)
            .ok_or(StorageHostError::NotFound)?;
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(StorageHostError::Arithmetic)?;
        // A sparse gap is logical growth the namespace pays for, and a rewrite in place still
        // charges the bytes it supplied.
        let logical_write = end.saturating_sub(current_length).max(bytes.len() as u64);
        self.charge_write(logical_write)?;
        let resulting_length = current_length.max(end);
        let planned = self.reserve_candidate(&[(&state.token, Some(resulting_length))])?;
        if bytes.is_empty() && end > current_length {
            self.truncate_direct(&state.token, end, planned)?;
        } else {
            self.write_range(&state.token, offset, bytes, planned)?;
        }
        self.entries
            .get_mut(&state.token)
            .expect("open entry")
            .present(resulting_length);
        Ok(())
    }

    pub fn vfs_size(&mut self, handle: u64) -> Result<u64, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        let state = self
            .handles
            .get(&handle)
            .ok_or(StorageHostError::InvalidArgument)?;
        self.entries
            .get(&state.token)
            .and_then(crate::handle::FileEntry::size)
            .ok_or(StorageHostError::NotFound)
    }

    pub fn vfs_truncate(&mut self, handle: u64, size: u64) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        let state = self
            .handles
            .get(&handle)
            .ok_or(StorageHostError::InvalidArgument)?
            .clone();
        if !state.write {
            return Err(StorageHostError::PermissionDenied);
        }
        let current_length = self
            .entries
            .get(&state.token)
            .and_then(crate::handle::FileEntry::size)
            .ok_or(StorageHostError::NotFound)?;
        // Growing truncate is logical growth; shrinking charges nothing but still reserves, so the
        // released bytes reach the ledger.
        let growth = size.saturating_sub(current_length);
        self.charge_write(growth)?;
        let planned = self.reserve_candidate(&[(&state.token, Some(size))])?;
        self.truncate_direct(&state.token, size, planned)?;
        self.entries
            .get_mut(&state.token)
            .expect("open entry")
            .present(size);
        Ok(())
    }

    pub fn vfs_sync(&mut self, handle: u64, _mode: Durability) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        if !self.handles.contains_key(&handle) {
            return Err(StorageHostError::InvalidArgument);
        }
        let token = &self.handles[&handle].token;
        let directory = &self.namespace.data_directory;
        directory
            .open_private(token, false)?
            .sync_all()
            .map_err(|source| directory.io_error(source))?;
        directory.sync()?;
        self.evidence.syncs = self
            .evidence
            .syncs
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        Ok(())
    }

    pub fn vfs_remove(&mut self, name: &str, _mode: Durability) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        self.charge_write(0)?;
        let token = self.ensure_entry(name)?;
        if self.handles.values().any(|handle| handle.token == token) {
            return Err(StorageHostError::Busy);
        }
        if !self.entries[&token].exists() {
            return Err(StorageHostError::NotFound);
        }
        let planned = self.reserve_candidate(&[(&token, None)])?;
        self.write_direct(&token, None, planned)?;
        self.entries
            .get_mut(&token)
            .expect("resolved entry")
            .absent();
        Ok(())
    }

    pub fn vfs_rename_atomic(
        &mut self,
        from: &str,
        to: &str,
        replace: bool,
        _mode: Durability,
    ) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        self.charge_write(0)?;
        let from_token = self.ensure_entry(from)?;
        let to_token = self.ensure_entry(to)?;
        if from_token == to_token {
            return Ok(());
        }
        if self
            .handles
            .values()
            .any(|handle| handle.token == from_token || handle.token == to_token)
        {
            return Err(StorageHostError::Busy);
        }
        // The directory entry moves, not the bytes: the source's stat-derived length is exactly
        // what the target ends up holding.
        let length = self.entries[&from_token]
            .size()
            .ok_or(StorageHostError::NotFound)?;
        if !replace && self.entries[&to_token].exists() {
            return Err(StorageHostError::AlreadyExists);
        }
        let identity = self.entries[&from_token].identity;
        let planned = self.reserve_candidate(&[(&from_token, None), (&to_token, Some(length))])?;
        let renamed = self.namespace.data_directory.rename_to(
            &from_token,
            &self.namespace.data_directory,
            &to_token,
        );
        self.after_mutation(renamed, planned)?;
        self.entries
            .get_mut(&from_token)
            .expect("renamed source")
            .absent();
        let target = self.entries.get_mut(&to_token).expect("renamed target");
        target.present(length);
        target.identity = identity;
        Ok(())
    }

    pub fn vfs_lock(&mut self, handle: u64, level: LockLevel) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        let current = self
            .handles
            .get(&handle)
            .ok_or(StorageHostError::InvalidArgument)?
            .lock;
        if level == current {
            return Ok(());
        }
        if next_lock(current) != Some(level) {
            return Err(StorageHostError::InvalidArgument);
        }
        let token = self.handles[&handle].token.clone();
        let others = self
            .handles
            .iter()
            .filter(|(id, state)| **id != handle && state.token == token);
        let conflict = match level {
            LockLevel::None => false,
            LockLevel::Shared => others
                .clone()
                .any(|(_, state)| state.lock >= LockLevel::Pending),
            LockLevel::Reserved | LockLevel::Pending => others
                .clone()
                .any(|(_, state)| state.lock >= LockLevel::Reserved),
            LockLevel::Exclusive => others
                .clone()
                .any(|(_, state)| state.lock != LockLevel::None),
        };
        if conflict {
            return Err(StorageHostError::Busy);
        }
        self.handles.get_mut(&handle).expect("existing handle").lock = level;
        Ok(())
    }

    pub fn vfs_unlock(&mut self, handle: u64, to: LockLevel) -> Result<(), StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        let state = self
            .handles
            .get_mut(&handle)
            .ok_or(StorageHostError::InvalidArgument)?;
        if to > state.lock {
            return Err(StorageHostError::InvalidArgument);
        }
        state.lock = to;
        Ok(())
    }

    pub fn vfs_check_reserved_lock(&mut self, handle: u64) -> Result<bool, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        let token = self
            .handles
            .get(&handle)
            .ok_or(StorageHostError::InvalidArgument)?
            .token
            .clone();
        Ok(self
            .handles
            .values()
            .any(|state| state.token == token && state.lock >= LockLevel::Reserved))
    }

    pub fn vfs_random_bytes(&mut self, length: u32) -> Result<Vec<u8>, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        self.charge_entropy(u64::from(length))?;
        random_bytes(length as usize)
    }

    pub fn vfs_monotonic_time_ns(&mut self) -> Result<u64, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        monotonic_ns()
    }

    pub fn vfs_wall_time_ms(&mut self) -> Result<u64, StorageHostError> {
        self.require_vfs()?;
        self.note_call()?;
        wall_ms()
    }

    fn require_vfs(&self) -> Result<(), StorageHostError> {
        if self.interface == StorageInterface::DurableFiles {
            Ok(())
        } else {
            Err(StorageHostError::PermissionDenied)
        }
    }
}

fn next_lock(level: LockLevel) -> Option<LockLevel> {
    match level {
        LockLevel::None => Some(LockLevel::Shared),
        LockLevel::Shared => Some(LockLevel::Reserved),
        LockLevel::Reserved => Some(LockLevel::Pending),
        LockLevel::Pending => Some(LockLevel::Exclusive),
        LockLevel::Exclusive => None,
    }
}
