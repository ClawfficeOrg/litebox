// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The first boot module: a tar of named files. Services and clients take the
//! files they need; the broker, see [`Payload::container`].

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use litebox_bootloader::handoff::BootInfo;
use litebox_broker_vm_kernel::Root;
use litebox_platform_vm_kernel::KERNEL_OFFSET;
use serde::Deserialize;

pub struct Payload {
    files: BTreeMap<&'static str, &'static [u8]>,
}

impl Payload {
    /// # Errors
    ///
    /// No boot module, or not a tar.
    pub fn read(info: &BootInfo) -> Result<Self, &'static str> {
        let module = info
            .modules
            .first()
            .ok_or("no payload module (pass -initrd)")?;
        let len = usize::try_from(module.end - module.start).unwrap();
        // Safety: boot modules are excluded from the heap and stay mapped at
        // `PA + KERNEL_OFFSET` for the kernel's lifetime.
        let data: &'static [u8] = unsafe {
            core::slice::from_raw_parts((module.start.as_u64() + KERNEL_OFFSET) as *const u8, len)
        };
        let archive = tar_no_std::TarArchiveRef::new(data).map_err(|_| "payload is not a tar")?;
        let mut files = BTreeMap::new();
        for entry in archive.entries() {
            let filename = entry.filename();
            let Ok(name) = filename.as_str() else {
                continue;
            };
            // `tar_no_std` names do not outlive the entry.
            let name: &'static str = alloc::boxed::Box::leak(name.trim_start_matches("./").into());
            files.insert(name, entry.data());
        }
        Ok(Self { files })
    }

    /// The root and `tmpfs` directories, from `config.json`, an OCI runtime
    /// spec subset (unknown fields ignored):
    /// - `root.path`: a tar in the payload (default `rootfs.tar`);
    ///   `root.readonly`.
    /// - `mounts` of `type` `tmpfs`; `options` ignored. Other mounts, and
    ///   destinations [`crate::devices::mount_path`] rejects, are skipped, as
    ///   default configurations have some (e.g., `proc`, `/dev/shm`).
    /// - Not OCI: `root.layers`, tars lowest first (one, if `root.readonly`),
    ///   overriding `root.path` until images come as OCI image layouts.
    ///
    /// # Panics
    ///
    /// If `config.json` is malformed or names a tar not in the payload.
    pub fn container(&self) -> (Root, Vec<String>) {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Config {
            root: ConfigRoot,
            mounts: Vec<Mount>,
        }
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct ConfigRoot {
            path: Option<String>,
            readonly: bool,
            layers: Option<Vec<String>>,
        }
        #[derive(Deserialize)]
        struct Mount {
            destination: String,
            #[serde(rename = "type", default)]
            kind: Option<String>,
        }

        let config: Config = self
            .file("config.json")
            .map_or_else(Default::default, |json| {
                serde_json::from_slice(json).expect("malformed config.json")
            });
        let tar = |name: &String| {
            self.file(name)
                .unwrap_or_else(|| panic!("config.json: no {name} in the payload"))
        };
        let layers = match (config.root.layers, config.root.path) {
            (Some(names), _) => names.iter().map(tar).collect(),
            (None, Some(name)) => alloc::vec![tar(&name)],
            (None, None) => self.file("rootfs.tar").into_iter().collect(),
        };
        let mut tmpfs = Vec::new();
        for mount in config.mounts {
            if mount.kind.as_deref() != Some("tmpfs") {
                litebox_util_log::warn!(
                    destination:% = mount.destination,
                    kind:? = mount.kind;
                    "config.json: skipping a mount of an unsupported type"
                );
            } else if !crate::devices::mount_path(&mount.destination) {
                litebox_util_log::warn!(
                    destination:% = mount.destination;
                    "config.json: skipping a tmpfs where the broker cannot mount"
                );
            } else {
                tmpfs.push(mount.destination);
            }
        }
        let root = Root {
            layers,
            writable: !config.root.readonly,
        };
        (root, tmpfs)
    }

    pub fn file(&self, name: &str) -> Option<&'static [u8]> {
        self.files.get(name).copied()
    }

    /// Files whose names start with `prefix`, as (the rest of the name, data).
    pub fn files_under<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = (&'static str, &'static [u8])> + 'a {
        self.files
            .iter()
            .filter_map(move |(name, data)| Some((name.strip_prefix(prefix)?, *data)))
    }
}
