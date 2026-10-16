// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The broker core's providers: hardware randomness, timers, `/dev` (standard
//! streams from the runner's [`StdioProvider`], `null`, `urandom`),
//! the root ([`Root`]), in-memory directories, host directories
//! ([`Share`]s) over 9P, and sockets.

use crate::Root;
use crate::share::{self, Share};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use litebox_broker_core::{
    BrokerCore, BrokerCoreLimits, CallerCredential, DestinationPortRange, DestinationRule,
    Ipv4Cidr, ObjectRights, PolicyEngine, SocketPolicy,
    fs::{
        FileService, Mode, OFlags, UserInfo,
        backend::{Backend, HandleRef},
        composer::{Composer, ComposerBuilder},
        devices::Devices,
        in_mem::{InMem, InitialNode},
        inode_allocator::InodeAllocator,
        overlay::Overlay,
        resolver::Resolver,
        tar_ro::{EMPTY_TAR_FILE, TarRo},
    },
    random::{RandomProvider, RandomProviderError},
    socket::SocketProvider,
    stdio::StdioProvider,
    timer::TimerProvider,
};
use litebox_broker_protocol::socket::{Ipv4Address, Port};
use litebox_platform_vm_kernel::VmKernel;

/// The HAL's hardware CSPRNG as the broker's randomness.
struct HardwareRandom(litebox_hal::rng::Hardware);

impl RandomProvider for HardwareRandom {
    fn fill(&self, output: &mut [u8]) -> Result<(), RandomProviderError> {
        self.0.fill(output).map_err(|_| RandomProviderError)
    }
}

/// No sticky bit: modes here have none.
const RWX_ALL: Mode = Mode::RWXU.union(Mode::RWXG).union(Mode::RWXO);

const WRITABLE_BY_ALL: InitialNode = InitialNode::Directory {
    mode: RWX_ALL,
    owner: UserInfo::ROOT,
};

/// With the mount's allocator, so on a device of its own, unlike
/// `InMem::new_initialized`'s.
fn tmpfs(allocator: InodeAllocator) -> InMem<VmKernel> {
    let fs = InMem::<VmKernel>::new(allocator);
    let root = fs
        .owned_dir_at(fs.root(), OFlags::PATH)
        .expect("an in-memory root opens");
    fs.chmod(HandleRef::Dir(&root), RWX_ALL)
        .expect("an in-memory root's mode changes");
    fs
}

/// A writable root is an overlay: the image layers, merged, under an
/// in-memory layer, with `/tmp`, that takes writes. A read-only root is its one
/// layer, mounted directly.
///
/// TODO(broker core, as in userland): the resolver skips permission checks on
/// backend roots (`/`, mount roots), and authorizes an open after the backend
/// opens it, so a denied `O_TRUNC` still copies up an empty file.
///
/// # Panics
///
/// If read-only with more than one layer.
fn mount_root(composer: ComposerBuilder, root: Root) -> ComposerBuilder {
    let Root {
        mut layers,
        writable,
    } = root;
    if layers.is_empty() {
        layers.push(EMPTY_TAR_FILE);
    }
    if !writable {
        let [image] = layers[..] else {
            panic!("a read-only root takes one layer");
        };
        return composer.mount("/", |allocator| TarRo::new(image.into(), allocator));
    }
    composer.mount_nestable("/", |allocators| {
        let upper = InMem::<VmKernel>::new_initialized([("/tmp".to_owned(), WRITABLE_BY_ALL)]);
        // An overlay's lowers go highest first.
        let lowers = layers
            .iter()
            .rev()
            .map(|layer| Box::new(TarRo::new((*layer).into(), allocators.next())) as _)
            .collect();
        Overlay::<VmKernel>::with_boxed_lowers(upper, lowers, allocators.next())
    })
}

/// # Panics
///
/// If a share cannot be mounted, as `--mount` fails userland's broker, or a
/// mount path is invalid or taken.
fn file_service(
    root: Root,
    tmpfs_paths: Vec<String>,
    shares: Vec<Share>,
    stdio: Arc<dyn StdioProvider>,
    random: Arc<dyn RandomProvider>,
) -> Arc<dyn FileService> {
    let mut composer = mount_root(Composer::builder(), root)
        .mount("/dev", |allocator| Devices::new(allocator, stdio, random));
    for path in tmpfs_paths {
        composer = composer.mount(&path, tmpfs);
    }
    for share in shares {
        let path = share.path.clone();
        composer = share::mount(composer, share)
            .unwrap_or_else(|error| panic!("mounting a share at {path}: {error}"));
    }
    let devices = composer
        .build()
        .unwrap_or_else(|error| panic!("broker file service: {error:?}"));
    Arc::new(Resolver::<VmKernel, _>::new(devices))
}

/// Sockets may reach the guest network, and, if `external`, any address:
/// what is reachable is up to the network the runner's link is on.
fn socket_policy(external: bool) -> SocketPolicy {
    let policy = SocketPolicy::guest_network();
    if !external {
        return policy;
    }
    let anywhere = [DestinationRule::new(
        CallerCredential::HostGuaranteed,
        Ipv4Cidr::new(Ipv4Address([0, 0, 0, 0]), 0).expect("a canonical network"),
        DestinationPortRange::new(Port(1), Port(u16::MAX)).expect("an ordered range"),
    )];
    policy
        .with_tcp_destination_rules(&anywhere)
        .and_then(|policy| policy.with_udp_destination_rules(&anywhere))
        .expect("one rule fits")
}

/// The broker core's limits, which the network stack also applies.
pub(crate) const LIMITS: BrokerCoreLimits = BrokerCoreLimits::DEFAULT;

/// A seed for the network stack.
///
/// # Panics
///
/// Without a hardware CSPRNG.
pub(crate) fn seed() -> u64 {
    let mut seed = [0; 8];
    litebox_hal::rng::hardware()
        .fill(&mut seed)
        .unwrap_or_else(|_| panic!("hardware CSPRNG failed"));
    u64::from_ne_bytes(seed)
}

/// Runner processes, which the kernel vouches for, may open and use files and
/// sockets (see [`socket_policy`]).
///
/// # Panics
///
/// Without a hardware CSPRNG, on a second call, if a share cannot be
/// mounted, if a mount path is invalid or taken, or if a read-only root has
/// more than one layer.
pub(crate) fn core(
    root: Root,
    tmpfs: Vec<String>,
    shares: Vec<Share>,
    stdio: Arc<dyn StdioProvider>,
    timers: Arc<dyn TimerProvider>,
    sockets: Arc<dyn SocketProvider>,
    external: bool,
) -> BrokerCore {
    let random: Arc<dyn RandomProvider> = Arc::new(HardwareRandom(litebox_hal::rng::hardware()));
    BrokerCore::new_with_limits(
        PolicyEngine::with_host_guaranteed_rights(ObjectRights::WAIT | ObjectRights::WRITE)
            .with_socket_policy(socket_policy(external)),
        LIMITS,
        sockets,
        Arc::clone(&random),
        timers,
        file_service(root, tmpfs, shares, stdio, random),
    )
    .unwrap_or_else(|error| panic!("broker core: {error:?}"))
}
