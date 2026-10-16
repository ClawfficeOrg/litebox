// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Host directories over 9P2000.L ([`Share`]), mounted as their [`ShareMode`]
//! says. The runner's [`Transport`]s carry whole messages (e.g., a virtio-9p
//! device), where the 9P client expects a byte stream.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use litebox_broker_core::fs::backend::{
    Backend, CreationMetadata, DirHandle, FileHandle, HandleRef, Permissioned, SeekBehavior,
    WalkOutcome, WalkingDirHandle,
};
use litebox_broker_core::fs::composer::ComposerBuilder;
use litebox_broker_core::fs::errors::{
    ChmodError, ChownError, FileStatusError, MkdirError, OpenError, ReadDirError,
    ReadError as FsReadError, RmdirError, TruncateError, UnlinkError, WalkError,
    WriteError as FsWriteError,
};
use litebox_broker_core::fs::in_mem::{InMem, InitialNode};
use litebox_broker_core::fs::inode_allocator::InodeAllocator;
use litebox_broker_core::fs::nine_p::transport::{Read, ReadError, Write, WriteError};
use litebox_broker_core::fs::nine_p::{self, NineP};
use litebox_broker_core::fs::overlay::Overlay;
use litebox_broker_core::fs::{DirEntry, FileStatus, Mode, OFlags};
use litebox_broker_core::readiness::ReadinessSource;
use litebox_platform_vm_kernel::VmKernel;

/// Carries 9P messages to a server, one request at a time: whole messages,
/// unlike the byte stream of `nine_p::transport`, which the share adapts it
/// to.
pub trait Transport: Send {
    /// The largest message either way, at least 8 KiB; the client's `msize`.
    fn max_message_size(&self) -> usize;

    /// Sends `request`, one whole message, and waits for its response: its
    /// length, its bytes copied into `response` (truncated; unchecked as 9P).
    ///
    /// # Errors
    ///
    /// The transport failed, e.g., the server did not answer in time; it is
    /// not used again.
    fn round_trip(&mut self, request: &[u8], response: &mut [u8]) -> Result<usize, TransportError>;
}

/// A [`Transport`] failed; the share's operations fail with I/O errors from
/// then on.
#[derive(Clone, Copy, Debug)]
pub struct TransportError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareMode {
    ReadOnly,
    /// Writes reach the host directory, which must be one the guest may wipe
    /// out; the 9P server decides what is allowed.
    ReadWrite,
    /// The host directory, read-only, under an in-memory layer: writes never
    /// reach the host and are lost at shutdown.
    Overlay,
}

pub struct Share {
    /// Absolute, normalized, and unlike other mounts' paths (e.g., `/`,
    /// `/dev`), or building the broker's files panics. Not checked: must not
    /// be in `/dev`.
    pub path: String,
    pub mode: ShareMode,
    pub transport: Box<dyn Transport>,
}

/// The 9P client writes a whole request, then reads its response; a request
/// goes out once complete (its size field says so).
struct Stream {
    transport: Box<dyn Transport>,
    request: Vec<u8>,
    response: Vec<u8>,
    response_len: usize,
    read: usize,
}

impl Stream {
    fn new(transport: Box<dyn Transport>) -> Self {
        let response = alloc::vec![0; transport.max_message_size()];
        Self {
            transport,
            request: Vec::new(),
            response,
            response_len: 0,
            read: 0,
        }
    }

    fn max_message_size(&self) -> usize {
        self.transport.max_message_size()
    }

    /// From the request's size field, once written.
    fn request_len(&self) -> Option<usize> {
        let size = self.request.first_chunk::<4>()?;
        usize::try_from(u32::from_le_bytes(*size)).ok()
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, WriteError> {
        if self.request.is_empty() {
            // A new request: whatever is left of the last response is stale.
            self.response_len = 0;
            self.read = 0;
        }
        self.request.extend_from_slice(buf);
        let Some(len) = self.request_len() else {
            return Ok(buf.len());
        };
        if len < 4 || len > self.transport.max_message_size() || self.request.len() > len {
            self.request.clear();
            return Err(WriteError);
        }
        if self.request.len() == len {
            let result = self.transport.round_trip(&self.request, &mut self.response);
            self.request.clear();
            self.response_len = result
                .map_err(|TransportError| WriteError)?
                .min(self.response.len());
        }
        Ok(buf.len())
    }
}

/// Zero at the end of the response, which [`Read::read_exact`] fails.
impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        let rest = &self.response[self.read..self.response_len];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.read += n;
        Ok(n)
    }
}

/// A [`Share`]'s directory. Unless [`ShareMode::ReadWrite`], writes fail
/// before reaching the 9P backend.
pub(crate) struct ShareFs {
    fs: NineP<VmKernel, Stream>,
    mode: ShareMode,
}

/// Linux's defaults for the user and tree a 9P mount attaches as; servers
/// such as QEMU's ignore them.
const USER: &str = "nobody";
const TREE: &str = "";

impl ShareFs {
    /// Attaches to the 9P server at the other end of `transport`.
    ///
    /// # Errors
    ///
    /// Version negotiation or the attach failed.
    fn new(
        transport: Box<dyn Transport>,
        mode: ShareMode,
        allocator: InodeAllocator,
    ) -> Result<Self, nine_p::Error> {
        let stream = Stream::new(transport);
        let msize = u32::try_from(stream.max_message_size()).unwrap_or(u32::MAX);
        let fs = NineP::new(stream, msize, USER, TREE, allocator)?;
        Ok(Self { fs, mode })
    }

    fn writable<E>(&self, error: E) -> Result<(), E> {
        if self.mode == ShareMode::ReadWrite {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[derive(Debug)]
pub(crate) enum MountError {
    Attach(nine_p::Error),
    Root,
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attach(error) => write!(f, "attaching: {error}"),
            Self::Root => f.write_str("looking at its root"),
        }
    }
}

/// An overlay reports its upper layer's root, so that copies the share
/// root's mode and owner. They are reported, not enforced: the resolver does
/// not check permissions on mount roots.
///
/// TODO: the 9P backend fails on host uids/gids beyond 16 bits.
pub(crate) fn mount(
    composer: ComposerBuilder,
    share: Share,
) -> Result<ComposerBuilder, MountError> {
    let Share {
        path,
        mode,
        transport,
    } = share;
    if mode != ShareMode::Overlay {
        return composer.try_mount(&path, |allocator| {
            ShareFs::new(transport, mode, allocator).map_err(MountError::Attach)
        });
    }
    composer.try_mount_nestable(&path, |allocators| {
        let lower = ShareFs::new(transport, mode, allocators.next()).map_err(MountError::Attach)?;
        let root = lower
            .owned_dir_at(lower.root(), OFlags::PATH)
            .map_err(|_| MountError::Root)?;
        let root = lower
            .status(HandleRef::Dir(&root))
            .map_err(|_| MountError::Root)?;
        let upper = InMem::<VmKernel>::new_initialized([(
            "/",
            InitialNode::Directory {
                mode: root.mode,
                owner: root.owner,
            },
        )]);
        Ok(Overlay::<VmKernel>::new(upper, lower, allocators.next()))
    })
}

const WRITE_FLAGS: OFlags = OFlags::CREAT
    .union(OFlags::TRUNC)
    .union(OFlags::WRONLY)
    .union(OFlags::RDWR);

// TODO: the 9P backend leaves permission checks to the server, and QEMU
// checks only its own host user. Reporting modes and owners
// (`PermissionCheck::ByResolver`) would let the resolver check the guest's
// user, at a `Tgetattr` per walked component.
impl Backend for ShareFs {
    fn root(&self) -> WalkingDirHandle<'_> {
        self.fs.root()
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        self.fs.walk_directories(from, components)
    }

    /// The 9P backend panics on directories opened for writing, so they are
    /// opened read-only, as `InMem` allows.
    fn owned_dir_at(
        &self,
        dir: WalkingDirHandle<'_>,
        flags: OFlags,
    ) -> Result<DirHandle, OpenError> {
        if flags.intersects(WRITE_FLAGS) {
            self.writable(OpenError::ReadOnlyFileSystem)?;
        }
        self.fs
            .owned_dir_at(dir, flags - OFlags::WRONLY - OFlags::RDWR)
    }

    fn walking_dir_at<'a>(&'a self, dir: &DirHandle) -> Option<WalkingDirHandle<'a>> {
        self.fs.walking_dir_at(dir)
    }

    /// An exclusive creation of an existing file must succeed, read-only, for
    /// the resolver to report `EEXIST` rather than `EROFS`, as `TarRo` does.
    fn open_file_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
        flags: OFlags,
    ) -> Result<Permissioned<FileHandle>, OpenError> {
        if self.mode == ShareMode::ReadWrite {
            return self.fs.open_file_at(dir, name, flags);
        }
        let exclusive = flags.contains(OFlags::CREAT | OFlags::EXCL);
        if !exclusive && flags.intersects(WRITE_FLAGS) {
            return Err(OpenError::ReadOnlyFileSystem);
        }
        self.fs
            .open_file_at(dir, name, flags - WRITE_FLAGS - OFlags::EXCL)
    }

    /// The overlay lists its lower directories through `O_PATH` handles,
    /// which the 9P backend cannot read, so they are reopened. The resolver
    /// lists only opened directories, which must stay readable after removal,
    /// so those are not. `lookup_at` defaults to this.
    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        if self.mode != ShareMode::Overlay {
            return self.fs.list_dir_at(handle);
        }
        let walking = self.fs.walking_dir_at(&handle).ok_or(ReadDirError::Io)?;
        let opened = self
            .fs
            .owned_dir_at(walking, OFlags::RDONLY)
            .map_err(|_| ReadDirError::Io)?;
        self.fs.list_dir_at(opened)
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, FsReadError> {
        self.fs.read(h, buf, offset)
    }

    fn get_static_backing_data(&self, h: &FileHandle) -> Option<&'static [u8]> {
        self.fs.get_static_backing_data(h)
    }

    fn write(&self, h: &FileHandle, buf: &[u8], offset: usize) -> Result<usize, FsWriteError> {
        self.writable(FsWriteError::NotForWriting)?;
        self.fs.write(h, buf, offset)
    }

    fn truncate(&self, h: &FileHandle, length: usize) -> Result<(), TruncateError> {
        self.writable(TruncateError::NotForWriting)?;
        self.fs.truncate(h, length)
    }

    fn seek_behavior(&self, h: &FileHandle) -> SeekBehavior {
        self.fs.seek_behavior(h)
    }

    fn status(&self, h: HandleRef<'_>) -> Result<FileStatus, FileStatusError> {
        self.fs.status(h)
    }

    fn is_terminal(&self, h: &FileHandle) -> bool {
        self.fs.is_terminal(h)
    }

    fn readiness_source(&self, h: &FileHandle) -> Option<Arc<dyn ReadinessSource>> {
        self.fs.readiness_source(h)
    }

    fn create_file_at(
        &self,
        dir: DirHandle,
        name: &str,
        metadata: CreationMetadata,
    ) -> Result<FileHandle, OpenError> {
        self.writable(OpenError::ReadOnlyFileSystem)?;
        self.fs.create_file_at(dir, name, metadata)
    }

    fn mkdir_at(
        &self,
        dir: DirHandle,
        name: &str,
        metadata: CreationMetadata,
    ) -> Result<DirHandle, MkdirError> {
        self.writable(MkdirError::ReadOnlyFileSystem)?;
        self.fs.mkdir_at(dir, name, metadata)
    }

    fn unlink_at(&self, dir: DirHandle, name: &str) -> Result<(), UnlinkError> {
        self.writable(UnlinkError::ReadOnlyFileSystem)?;
        self.fs.unlink_at(dir, name)
    }

    fn rmdir_at(&self, dir: DirHandle, name: &str) -> Result<(), RmdirError> {
        self.writable(RmdirError::ReadOnlyFileSystem)?;
        self.fs.rmdir_at(dir, name)
    }

    fn chmod(&self, h: HandleRef<'_>, mode: Mode) -> Result<(), ChmodError> {
        self.writable(ChmodError::ReadOnlyFileSystem)?;
        self.fs.chmod(h, mode)
    }

    fn chown(
        &self,
        h: HandleRef<'_>,
        user: Option<u16>,
        group: Option<u16>,
    ) -> Result<(), ChownError> {
        self.writable(ChownError::ReadOnlyFileSystem)?;
        self.fs.chown(h, user, group)
    }
}
