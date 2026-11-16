//! CPU topology: L3 domains, SMT cores, CPU Set ids, default profiles.
//! All data comes from the Windows API. Do not assume a core numbering.

use std::ptr::null_mut;
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemCpuSetInformation, RelationCache,
    RelationProcessorCore,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};

use crate::config::Profile;

#[derive(Clone, Debug, PartialEq)]
pub struct L3 {
    pub size: u32,
    pub mask: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Topology {
    /// All logical processors available to this system (single group).
    pub all_mask: u64,
    pub l3: Vec<L3>,
    pub physical_cores: u32,
    /// (logical processor index, CPU Set id)
    pub cpu_sets: Vec<(u8, u32)>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Calls `f` with the raw bytes of each record (records have different sizes).
/// Record header: Relationship u32 @0, Size u32 @4, payload @8.
fn for_each_lpi_record(relation: i32, mut f: impl FnMut(&[u8])) {
    unsafe {
        let mut len = 0u32;
        GetLogicalProcessorInformationEx(relation, null_mut(), &mut len);
        if len == 0 {
            return;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetLogicalProcessorInformationEx(relation, buf.as_mut_ptr() as *mut _, &mut len) == 0 {
            return;
        }
        let bytes = std::slice::from_raw_parts(buf.as_ptr() as *const u8, len as usize);
        let mut off = 0usize;
        while off + 8 <= bytes.len() {
            let size = u32_at(bytes, off + 4) as usize;
            if size == 0 || off + size > bytes.len() {
                break;
            }
            f(&bytes[off..off + size]);
            off += size;
        }
    }
}

pub fn read() -> Topology {
    let mut t = Topology::default();
    unsafe {
        let (mut process, mut system) = (0usize, 0usize);
        GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system);
        t.all_mask = system as u64;
    }

    // CACHE_RELATIONSHIP @8: Level u8 @8, CacheSize u32 @12, GroupMask.Mask @40.
    for_each_lpi_record(RelationCache, |r| {
        if r.len() >= 48 && r[8] == 3 {
            let l3 = L3 { size: u32_at(r, 12), mask: u64_at(r, 40) };
            if !t.l3.contains(&l3) {
                t.l3.push(l3);
            }
        }
    });
    t.l3.sort_by_key(|l| l.mask.trailing_zeros());

    // PROCESSOR_RELATIONSHIP @8: one record per physical core.
    for_each_lpi_record(RelationProcessorCore, |_| t.physical_cores += 1);

    // SYSTEM_CPU_SET_INFORMATION: Size u32 @0, Type u32 @4, Id u32 @8,
    // Group u16 @12, LogicalProcessorIndex u8 @14.
    unsafe {
        let mut len = 0u32;
        GetSystemCpuSetInformation(null_mut(), 0, &mut len, null_mut(), 0);
        if len > 0 {
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetSystemCpuSetInformation(buf.as_mut_ptr() as *mut _, len, &mut len, null_mut(), 0)
                != 0
            {
                let bytes = std::slice::from_raw_parts(buf.as_ptr() as *const u8, len as usize);
                let mut off = 0usize;
                while off + 16 <= bytes.len() {
                    let size = u32_at(bytes, off) as usize;
                    if size == 0 || off + size > bytes.len() {
                        break;
                    }
                    if u32_at(bytes, off + 4) == 0 && u16_at(bytes, off + 12) == 0 {
                        t.cpu_sets.push((bytes[off + 14], u32_at(bytes, off + 8)));
                    }
                    off += size;
                }
            }
        }
    }
    t
}

impl Topology {
    /// CPU Set ids for the logical processors in `mask`, sorted.
    pub fn cpu_set_ids(&self, mask: u64) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .cpu_sets
            .iter()
            .filter(|(idx, _)| *idx < 64 && mask & (1u64 << idx) != 0)
            .map(|&(_, id)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// "Cache" and "Frequency" if there are two L3 domains of different size
    /// (the larger L3 is the V-Cache CCD). "All cores" always.
    pub fn default_profiles(&self) -> Vec<Profile> {
        let mut out = Vec::new();
        if self.l3.len() == 2 && self.l3[0].size != self.l3[1].size {
            let (big, small) = if self.l3[0].size > self.l3[1].size {
                (&self.l3[0], &self.l3[1])
            } else {
                (&self.l3[1], &self.l3[0])
            };
            out.push(Profile { name: "Cache".into(), cpus: format_mask(big.mask) });
            out.push(Profile { name: "Frequency".into(), cpus: format_mask(small.mask) });
        }
        out.push(Profile { name: "All cores".into(), cpus: format_mask(self.all_mask) });
        out
    }

    pub fn describe(&self) -> String {
        let l3: Vec<String> = self
            .l3
            .iter()
            .map(|l| format!("{} MB on cpus {}", l.size / (1024 * 1024), format_mask(l.mask)))
            .collect();
        format!(
            "topology: {} logical / {} physical, cpus {}, L3: [{}]",
            self.all_mask.count_ones(),
            self.physical_cores,
            format_mask(self.all_mask),
            l3.join("; ")
        )
    }
}

/// "0-15,20" -> bit mask. Indices must be below 64.
pub fn parse_cpus(s: &str) -> Result<u64, String> {
    let mut mask = 0u64;
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let a: u32 = a.parse().map_err(|_| format!("bad cpu index '{part}'"))?;
        let b: u32 = b.parse().map_err(|_| format!("bad cpu index '{part}'"))?;
        if a > b || b > 63 {
            return Err(format!("bad cpu range '{part}'"));
        }
        for i in a..=b {
            mask |= 1u64 << i;
        }
    }
    if mask == 0 {
        return Err("empty cpu list".into());
    }
    Ok(mask)
}

/// Bit mask -> "0-15,20".
pub fn format_mask(mask: u64) -> String {
    let mut parts = Vec::new();
    let mut i = 0u32;
    while i < 64 {
        if mask & (1u64 << i) != 0 {
            let start = i;
            while i + 1 < 64 && mask & (1u64 << (i + 1)) != 0 {
                i += 1;
            }
            parts.push(if start == i { format!("{start}") } else { format!("{start}-{i}") });
        }
        i += 1;
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpus_round_trip() {
        assert_eq!(parse_cpus("0-15").unwrap(), 0xFFFF);
        assert_eq!(parse_cpus("16-31").unwrap(), 0xFFFF_0000);
        assert_eq!(parse_cpus(" 0, 2-3 ,63").unwrap(), 0x8000_0000_0000_000D);
        assert_eq!(format_mask(0xFFFF_0000), "16-31");
        assert_eq!(format_mask(0x8000_0000_0000_000D), "0,2-3,63");
        assert!(parse_cpus("").is_err());
        assert!(parse_cpus("5-2").is_err());
        assert!(parse_cpus("0-64").is_err());
        assert!(parse_cpus("0x0F").is_err());
    }

    #[test]
    fn default_profiles_dual_ccd() {
        let t = Topology {
            all_mask: 0xFFFF_FFFF,
            l3: vec![L3 { size: 96 << 20, mask: 0xFFFF }, L3 { size: 32 << 20, mask: 0xFFFF_0000 }],
            physical_cores: 16,
            cpu_sets: vec![],
        };
        let p = t.default_profiles();
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].name.as_str(), p[0].cpus.as_str()), ("Cache", "0-15"));
        assert_eq!((p[1].name.as_str(), p[1].cpus.as_str()), ("Frequency", "16-31"));
        assert_eq!((p[2].name.as_str(), p[2].cpus.as_str()), ("All cores", "0-31"));
    }

    #[test]
    fn default_profiles_single_l3() {
        let t = Topology {
            all_mask: 0xFF,
            l3: vec![L3 { size: 32 << 20, mask: 0xFF }],
            physical_cores: 4,
            cpu_sets: vec![],
        };
        assert_eq!(t.default_profiles().len(), 1);
    }
}
