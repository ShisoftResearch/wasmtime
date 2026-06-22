#![allow(dead_code)]

use crate::prelude::*;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RegionId(pub(crate) u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegionBackendKind {
    DaxPmem,
    FileBacked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionAddressWindow {
    pub(crate) base: usize,
    pub(crate) reserved_len: usize,
    pub(crate) mapped_len: usize,
}

impl RegionAddressWindow {
    pub(crate) fn end(self) -> Result<usize> {
        self.base
            .checked_add(self.reserved_len)
            .context("region address window end overflow")
    }

    pub(crate) fn contains(self, addr: usize) -> Result<bool> {
        Ok(self.base <= addr && addr < self.end()?)
    }

    fn validate(self) -> Result<()> {
        ensure!(
            self.reserved_len > 0,
            "region reserved length must be nonzero"
        );
        ensure!(
            self.mapped_len <= self.reserved_len,
            "region mapped length exceeds reserved length"
        );
        let _ = self.end()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegionDescriptor {
    pub(crate) id: RegionId,
    pub(crate) backend: RegionBackendKind,
    pub(crate) path: PathBuf,
    pub(crate) window: RegionAddressWindow,
    pub(crate) numa_node: Option<i32>,
    pub(crate) cpu_set: Option<Vec<usize>>,
}

fn region_path_key(path: &Path, cwd: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "region path must be nonempty");

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegionSetDescriptor {
    regions: Vec<RegionDescriptor>,
    backend_kind: RegionBackendKind,
}

impl RegionSetDescriptor {
    pub(crate) fn new(mut regions: Vec<RegionDescriptor>) -> Result<Self> {
        ensure!(
            !regions.is_empty(),
            "region set requires at least one region"
        );
        regions.sort_by_key(|region| (region.window.base, region.id));

        let backend_kind = regions[0].backend;
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let cwd = std::env::current_dir()
            .context("failed to determine current directory for region path")?;
        for region in &mut regions {
            ensure!(
                region.backend == backend_kind,
                "all regions in one region set must use the same backend kind"
            );
            ensure!(ids.insert(region.id), "duplicate region id");
            let path_key = region_path_key(&region.path, &cwd)?;
            ensure!(paths.insert(path_key.clone()), "duplicate region path");
            region.path = path_key;
            if let Some(numa_node) = region.numa_node {
                ensure!(numa_node >= 0, "region NUMA node must be nonnegative");
            }
            if let Some(cpu_set) = region.cpu_set.as_mut() {
                ensure!(!cpu_set.is_empty(), "region CPU set must be nonempty");
                cpu_set.sort_unstable();
                cpu_set.dedup();
            }
            region.window.validate()?;
        }

        for pair in regions.windows(2) {
            let left = &pair[0];
            let right = &pair[1];
            ensure!(
                left.window.end()? <= right.window.base,
                "region address windows overlap"
            );
        }

        Ok(Self {
            regions,
            backend_kind,
        })
    }

    pub(crate) fn regions(&self) -> &[RegionDescriptor] {
        &self.regions
    }

    pub(crate) fn backend_kind(&self) -> RegionBackendKind {
        self.backend_kind
    }

    pub(crate) fn region_for_management_addr(&self, addr: usize) -> Option<&RegionDescriptor> {
        self.regions
            .iter()
            .find(|region| region.window.contains(addr).unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn region(id: u32, start: usize, len: usize, backend: RegionBackendKind) -> RegionDescriptor {
        RegionDescriptor {
            id: RegionId(id),
            backend,
            path: PathBuf::from(format!("/tmp/region-{id}")),
            window: RegionAddressWindow {
                base: start,
                reserved_len: len,
                mapped_len: len / 2,
            },
            numa_node: None,
            cpu_set: None,
        }
    }

    #[test]
    fn region_set_accepts_non_overlapping_homogeneous_regions() {
        let set = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x2000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap();
        assert_eq!(set.regions().len(), 2);
        assert_eq!(set.backend_kind(), RegionBackendKind::DaxPmem);
    }

    #[test]
    fn region_set_rejects_empty_region_list() {
        let err = RegionSetDescriptor::new(vec![]).unwrap_err();
        assert!(err.to_string().contains("at least one region"));
    }

    #[test]
    fn region_set_rejects_mixed_backend_kinds() {
        let err = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x2000_0000, 0x0100_0000, RegionBackendKind::FileBacked),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("same backend kind"));
    }

    #[test]
    fn region_set_rejects_overlapping_windows() {
        let err = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x1008_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("overlap"));
    }

    #[test]
    fn region_set_rejects_mapped_len_larger_than_reserved_len() {
        let mut bad = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        bad.window.mapped_len = bad.window.reserved_len + 1;
        let err = RegionSetDescriptor::new(vec![bad]).unwrap_err();
        assert!(err.to_string().contains("mapped length"));
    }

    #[test]
    fn region_set_rejects_zero_reserved_len() {
        let err =
            RegionSetDescriptor::new(vec![region(0, 0x1000_0000, 0, RegionBackendKind::DaxPmem)])
                .unwrap_err();
        assert!(err.to_string().contains("reserved length"));
    }

    #[test]
    fn region_set_rejects_window_end_overflow() {
        let err =
            RegionSetDescriptor::new(vec![region(0, usize::MAX, 1, RegionBackendKind::DaxPmem)])
                .unwrap_err();
        assert!(err.to_string().contains("overflow"));
    }

    #[test]
    fn region_set_rejects_duplicate_region_ids() {
        let err = RegionSetDescriptor::new(vec![
            region(7, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(7, 0x3000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("duplicate region id"));
    }

    #[test]
    fn region_set_rejects_duplicate_region_paths() {
        let first = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        let mut second = region(1, 0x3000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        second.path = first.path.clone();

        let err = RegionSetDescriptor::new(vec![first, second]).unwrap_err();
        assert!(err.to_string().contains("duplicate region path"));
    }

    #[test]
    fn region_set_rejects_empty_region_path() {
        let mut bad = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        bad.path = PathBuf::new();

        let err = RegionSetDescriptor::new(vec![bad]).unwrap_err();
        assert!(err.to_string().contains("region path"));
    }

    #[test]
    fn region_set_rejects_lexically_equivalent_region_paths() {
        let mut first = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        first.path = std::env::current_dir()
            .unwrap()
            .join("target/region-equivalent");
        let mut second = region(1, 0x3000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        second.path = PathBuf::from("target").join(".").join("region-equivalent");

        let err = RegionSetDescriptor::new(vec![first, second]).unwrap_err();
        assert!(err.to_string().contains("duplicate region path"));
    }

    #[test]
    fn region_set_stores_relative_region_path_as_absolute_normalized_path() {
        let mut descriptor = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        descriptor.path = PathBuf::from("target").join(".").join("region-stable-path");
        let expected = std::env::current_dir()
            .unwrap()
            .join("target/region-stable-path");

        let set = RegionSetDescriptor::new(vec![descriptor]).unwrap();
        assert_eq!(set.regions()[0].path, expected);
    }

    #[test]
    fn region_set_rejects_negative_numa_node() {
        let mut bad = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        bad.numa_node = Some(-1);

        let err = RegionSetDescriptor::new(vec![bad]).unwrap_err();
        assert!(err.to_string().contains("NUMA node"));
    }

    #[test]
    fn region_set_rejects_empty_cpu_set() {
        let mut bad = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        bad.cpu_set = Some(Vec::new());

        let err = RegionSetDescriptor::new(vec![bad]).unwrap_err();
        assert!(err.to_string().contains("CPU set"));
    }

    #[test]
    fn region_set_normalizes_cpu_set() {
        let mut descriptor = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        descriptor.cpu_set = Some(vec![3, 1, 3, 2]);

        let set = RegionSetDescriptor::new(vec![descriptor]).unwrap();
        assert_eq!(set.regions()[0].cpu_set.as_deref(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn region_set_finds_region_for_management_address() {
        let set = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x2000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap();
        assert_eq!(
            set.region_for_management_addr(0x2000_1234).unwrap().id,
            RegionId(1)
        );
        assert!(set.region_for_management_addr(0x3000_0000).is_none());
    }
}
