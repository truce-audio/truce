//! Safe wrappers over the host's stream-based archive I/O (phase 5).
//!
//! ARA persists a plug-in's per-object state (here: each audio source's
//! analysis) into a host-managed archive addressed by byte position. The
//! plug-in assembles/parses the whole payload in memory and reads/writes
//! it in one call - simplest correct shape; true streaming can come
//! later. Authors never see this: `AraPlugin::store` / `restore` just
//! produce / consume a `Vec<u8>`.

use crate::abi::{
    ARAArchiveReaderHostRef, ARAArchiveWriterHostRef, ARAArchivingControllerHostRef,
    ARAArchivingControllerInterface, kARAFalse,
};

/// Writes the plug-in's archive payload through the host.
pub(crate) struct ArchiveWriter {
    host_ref: ARAArchivingControllerHostRef,
    writer_ref: ARAArchiveWriterHostRef,
    iface: *const ARAArchivingControllerInterface,
}

impl ArchiveWriter {
    pub(crate) fn new(
        host_ref: ARAArchivingControllerHostRef,
        writer_ref: ARAArchiveWriterHostRef,
        iface: *const ARAArchivingControllerInterface,
    ) -> Self {
        Self {
            host_ref,
            writer_ref,
            iface,
        }
    }

    /// Write the whole payload at position 0. Returns `false` on a host
    /// I/O failure (or a missing archiving controller).
    pub(crate) fn write_all(&self, bytes: &[u8]) -> bool {
        if self.iface.is_null() {
            return false;
        }
        // SAFETY: non-null archiving interface provided by the host.
        let Some(write) = (unsafe { &*self.iface }).write_bytes_to_archive else {
            return false;
        };
        if bytes.is_empty() {
            return true;
        }
        // SAFETY: `bytes` is a valid `bytes.len()`-byte buffer.
        let ok = unsafe {
            write(
                self.host_ref,
                self.writer_ref,
                0,
                bytes.len(),
                bytes.as_ptr(),
            )
        };
        ok != kARAFalse
    }
}

/// Reads the plug-in's archive payload back from the host.
pub(crate) struct ArchiveReader {
    host_ref: ARAArchivingControllerHostRef,
    reader_ref: ARAArchiveReaderHostRef,
    iface: *const ARAArchivingControllerInterface,
}

impl ArchiveReader {
    pub(crate) fn new(
        host_ref: ARAArchivingControllerHostRef,
        reader_ref: ARAArchiveReaderHostRef,
        iface: *const ARAArchivingControllerInterface,
    ) -> Self {
        Self {
            host_ref,
            reader_ref,
            iface,
        }
    }

    /// Read the whole archive into a `Vec`. `None` on a host I/O failure.
    pub(crate) fn read_all(&self) -> Option<Vec<u8>> {
        if self.iface.is_null() {
            return None;
        }
        // SAFETY: non-null archiving interface provided by the host.
        let iface = unsafe { &*self.iface };
        let get_size = iface.get_archive_size?;
        let read = iface.read_bytes_from_archive?;
        // SAFETY: valid host refs for this archive.
        let size = unsafe { get_size(self.host_ref, self.reader_ref) };
        if size == 0 {
            return Some(Vec::new());
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` is `size` bytes.
        let ok = unsafe { read(self.host_ref, self.reader_ref, 0, size, buf.as_mut_ptr()) };
        (ok != kARAFalse).then_some(buf)
    }
}
