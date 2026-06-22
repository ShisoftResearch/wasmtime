use crate::prelude::*;
use alloc::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CpuSet {
    cpus: Vec<usize>,
}

impl CpuSet {
    pub(crate) fn new(mut cpus: Vec<usize>) -> Result<Self> {
        ensure!(!cpus.is_empty(), "CPU set must be nonempty");
        cpus.sort_unstable();
        cpus.dedup();
        Ok(Self { cpus })
    }

    pub(crate) fn parse(spec: &str) -> Result<Self> {
        Self::new(parse_cpu_list(spec)?)
    }

    pub(crate) fn as_slice(&self) -> &[usize] {
        &self.cpus
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct NumaNode(i32);

impl NumaNode {
    pub(crate) fn new(node: i32) -> Result<Self> {
        ensure!(node >= 0, "NUMA node must be nonnegative");
        Ok(Self(node))
    }

    pub(crate) fn id(self) -> i32 {
        self.0
    }
}

pub(crate) fn parse_cpu_list(spec: &str) -> Result<Vec<usize>> {
    let spec = spec.trim();
    ensure!(!spec.is_empty(), "CPU list must be nonempty");

    let mut cpus = BTreeSet::new();
    for item in spec.split(',') {
        let item = item.trim();
        ensure!(!item.is_empty(), "invalid CPU list entry");

        if item.contains('-') {
            let mut parts = item.split('-');
            let start = parts.next().unwrap();
            let end = parts.next().context("invalid CPU range")?;
            ensure!(parts.next().is_none(), "invalid CPU range `{item}`");

            let start = start
                .parse::<usize>()
                .with_context(|| format!("invalid CPU index `{start}`"))?;
            let end = end
                .parse::<usize>()
                .with_context(|| format!("invalid CPU index `{end}`"))?;
            ensure!(start <= end, "invalid CPU range `{item}`");

            for cpu in start..=end {
                cpus.insert(cpu);
            }
        } else {
            let cpu = item
                .parse::<usize>()
                .with_context(|| format!("invalid CPU index `{item}`"))?;
            cpus.insert(cpu);
        }
    }

    Ok(cpus.into_iter().collect())
}

#[cfg(target_os = "linux")]
pub(crate) fn current_cpu() -> Result<usize> {
    let cpu = unsafe { libc::sched_getcpu() };
    if cpu < 0 {
        return Err(std::io::Error::last_os_error()).context("failed to get current CPU");
    }
    usize::try_from(cpu).context("current CPU index overflow")
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn current_cpu() -> Result<usize> {
    bail!("current_cpu is unsupported on this platform")
}

#[cfg(target_os = "linux")]
pub(crate) fn node_for_cpu(cpu: usize) -> Result<Option<i32>> {
    const SYSFS_NODE_ROOT: &str = "/sys/devices/system/node";

    let entries = match std::fs::read_dir(SYSFS_NODE_ROOT) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {SYSFS_NODE_ROOT}"));
        }
    };

    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {SYSFS_NODE_ROOT} entry"))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(raw_node) = file_name.strip_prefix("node") else {
            continue;
        };
        let node = raw_node
            .parse::<i32>()
            .with_context(|| format!("invalid NUMA node directory `{file_name}`"))?;
        nodes.push((NumaNode::new(node)?.id(), entry.path().join("cpulist")));
    }

    nodes.sort_unstable_by_key(|(node, _)| *node);

    for (node, cpulist_path) in nodes {
        let cpulist = match std::fs::read_to_string(&cpulist_path) {
            Ok(cpulist) => cpulist,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", cpulist_path.display()));
            }
        };
        let cpu_list = parse_cpu_list(cpulist.trim())
            .with_context(|| format!("failed to parse {}", cpulist_path.display()))?;
        if cpu_list.binary_search(&cpu).is_ok() {
            return Ok(Some(node));
        }
    }

    Ok(None)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn node_for_cpu(_cpu: usize) -> Result<Option<i32>> {
    bail!("node_for_cpu is unsupported on this platform")
}

#[cfg(target_os = "linux")]
pub(crate) fn cpus_for_node(node: NumaNode) -> Result<Option<CpuSet>> {
    let cpulist_path = format!("/sys/devices/system/node/node{}/cpulist", node.id());
    let cpulist = match std::fs::read_to_string(&cpulist_path) {
        Ok(cpulist) => cpulist,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {cpulist_path}"));
        }
    };
    CpuSet::parse(cpulist.trim())
        .map(Some)
        .with_context(|| format!("failed to parse {cpulist_path}"))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cpus_for_node(_node: NumaNode) -> Result<Option<CpuSet>> {
    bail!("cpus_for_node is unsupported on this platform")
}

#[cfg(target_os = "linux")]
pub(crate) fn pin_current_thread(cpus: &[usize]) -> Result<()> {
    let cpus = CpuSet::new(cpus.to_vec())?;
    let mut affinity = unsafe { core::mem::zeroed::<libc::cpu_set_t>() };
    unsafe { libc::CPU_ZERO(&mut affinity) };

    let cpu_set_size = usize::try_from(libc::CPU_SETSIZE).unwrap();
    for &cpu in cpus.as_slice() {
        ensure!(
            cpu < cpu_set_size,
            "CPU index {cpu} exceeds CPU_SETSIZE {cpu_set_size}"
        );
        unsafe { libc::CPU_SET(cpu, &mut affinity) };
    }

    let rc =
        unsafe { libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &affinity) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to pin current thread");
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pin_current_thread(_cpus: &[usize]) -> Result<()> {
    bail!("pin_current_thread is unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::parse_cpu_list;

    #[test]
    fn parse_cpu_list_expands_ranges() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11").unwrap(),
            vec![0, 1, 2, 3, 8, 10, 11]
        );
    }

    #[test]
    fn parse_cpu_list_rejects_reversed_range() {
        let error = format!("{}", parse_cpu_list("4-2").unwrap_err());
        assert!(
            error.contains("invalid CPU range"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_cpu_list_sorts_and_dedups() {
        assert_eq!(parse_cpu_list("3,1,2,1").unwrap(), vec![1, 2, 3]);
    }
}
