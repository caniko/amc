//! Durable discovery hints, never admission or residency proofs. A campaign
//! advances through host PIDs and streams VMAs/pagemap in bounded windows.
//! Reopening a cursor always reidentifies the live target; the broker separately
//! rechecks placement, swapped PTEs and native backing before every batch.
use crate::{host::Identity, page_return, recovery::RecoveryPolicy};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::FileExt,
    path::Path,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCursor {
    pub target: Identity,
    /// A change resets the hint; the mm-bound descriptors remain the proof.
    pub layout: String,
    pub maps_offset: u64,
    pub address: u64,
}

impl ProcessCursor {
    pub fn revalidate(&mut self, target: Identity, layout: String) {
        if self.target != target || self.layout != layout {
            *self = Self {
                target,
                layout,
                maps_offset: 0,
                address: 0,
            };
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub boot_id: String,
    pub groups: Vec<String>,
    pub after_pid: i32,
    pub active: Option<ProcessCursor>,
}

impl crate::store::Snapshot for Discovery {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.groups.len() <= 64 && self.after_pid >= 0,
            "invalid discovery frontier"
        );
        for group in &self.groups {
            crate::native::cgroup_directory(group)?;
        }
        if let Some(cursor) = &self.active {
            crate::native::cgroup_directory(&cursor.target.cgroup)?;
            ensure!(
                cursor.target.pid > 0
                    && cursor.target.start_ticks > 0
                    && cursor.target.inode > 0
                    && cursor.layout.len() <= 256
                    && cursor.maps_offset <= i64::MAX as u64,
                "invalid process discovery hint"
            );
        }
        Ok(())
    }
    fn boot_id(&self) -> &str {
        &self.boot_id
    }
    fn fresh(boot: &str) -> Self {
        Self {
            boot_id: boot.into(),
            groups: vec![],
            after_pid: 0,
            active: None,
        }
    }
}

impl Discovery {
    pub fn select(&mut self, policy: &RecoveryPolicy) {
        if self.groups != policy.page_cgroups {
            self.groups = policy.page_cgroups.clone();
            self.after_pid = 0;
            self.active = None;
        }
    }
    pub fn finish_process(&mut self) {
        if let Some(cursor) = self.active.take() {
            self.after_pid = cursor.target.pid;
        }
    }
    pub fn wrap(&mut self) {
        self.after_pid = 0;
        self.active = None;
    }
}

/// Keep only the next 512 host PIDs, rather than truncating a subtree's prefix.
/// Scanning host /proc also avoids cgroup-walk cutoffs and discovers selected
/// processes even when a hierarchy contains more than 8192 empty descendants.
pub fn candidates(after: i32) -> Result<Vec<i32>> {
    candidates_at(Path::new("/proc"), after)
}

fn candidates_at(root: &Path, after: i32) -> Result<Vec<i32>> {
    let mut pids = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid > after {
            pids.insert(pid);
            if pids.len() > 512 {
                pids.pop_last();
            }
        }
    }
    Ok(pids.into_iter().collect())
}

pub fn selected(pid: i32, policy: &RecoveryPolicy) -> Result<bool> {
    let group = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let group = group
        .lines()
        .find_map(|s| s.strip_prefix("0::"))
        .context("missing native placement")?;
    Ok(policy
        .page_cgroups
        .iter()
        .any(|prefix| group == prefix || group.starts_with(&format!("{prefix}/"))))
}

pub fn layout(pid: i32) -> Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .context("invalid native process stat")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    // startcode/endcode/startstack and data/argument/environment bounds change
    // on ordinary exec. Even same-layout exec cannot turn a hint into authority:
    // the complete sweep wraps and native descriptors/settlement are rechecked.
    let indices = [23, 24, 25, 42, 43, 44, 45, 46, 47, 48];
    indices
        .into_iter()
        .map(|i| fields.get(i).copied().context("short native process stat"))
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join(" "))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Scan {
    Range { address: u64, bytes: u64 },
    More,
    Complete,
}

/// The maps byte offset is only a restart hint. Churn at that position can
/// invalidate it; reset without faulting and revisit on this or a later sweep.
/// A found range stays at its first page until the caller proves settlement.
pub fn scan(cursor: &mut ProcessCursor, pagemap: &File, bound: u64) -> Result<Scan> {
    let maps = File::open(format!("/proc/{}/maps", cursor.target.pid))?;
    scan_at(cursor, maps, pagemap, bound, 8192, 8_388_608)
}

fn scan_at(
    cursor: &mut ProcessCursor,
    maps: File,
    pagemap: &File,
    bound: u64,
    map_budget: usize,
    page_budget: u64,
) -> Result<Scan> {
    let page = page_return::page_size()?;
    ensure!(
        bound >= page && bound <= 16 * 1024 * 1024,
        "invalid discovery batch"
    );
    let mut maps = BufReader::new(maps);
    maps.seek(SeekFrom::Start(cursor.maps_offset))?;
    let mut entries = [0u8; 4096];
    let mut scanned = 0;
    for _ in 0..map_budget {
        let offset = maps.stream_position()?;
        let mut line = String::new();
        let count = (&mut maps).take(16385).read_line(&mut line)?;
        ensure!(count <= 16384, "native mapping exceeds line bound");
        if count == 0 {
            return Ok(Scan::Complete);
        }
        let mut fields = line.split_whitespace();
        let range = fields
            .next()
            .and_then(|s| s.split_once('-'))
            .and_then(|(a, b)| {
                Some((
                    u64::from_str_radix(a, 16).ok()?,
                    u64::from_str_radix(b, 16).ok()?,
                ))
            });
        let Some((first, last)) =
            range.filter(|(a, b)| a < b && a.is_multiple_of(page) && b.is_multiple_of(page))
        else {
            cursor.maps_offset = 0;
            cursor.address = 0;
            return Ok(Scan::More);
        };
        let readable = fields
            .next()
            .is_some_and(|p| p.starts_with('r') && p.ends_with('p'));
        let mut address =
            if cursor.maps_offset == offset && (first..=last).contains(&cursor.address) {
                cursor.address
            } else {
                first
            };
        if readable {
            while address < last {
                if scanned == page_budget {
                    cursor.maps_offset = offset;
                    cursor.address = address;
                    return Ok(Scan::More);
                }
                let count = ((last - address) / page)
                    .min((entries.len() / 8) as u64)
                    .min(page_budget - scanned) as usize;
                ensure!(count > 0, "empty native mapping scan");
                pagemap.read_exact_at(
                    &mut entries[..count * 8],
                    (address / page)
                        .checked_mul(8)
                        .context("pagemap overflow")?,
                )?;
                scanned += count as u64;
                let swapped = |i: usize| {
                    u64::from_ne_bytes(
                        entries[i * 8..i * 8 + 8]
                            .try_into()
                            .expect("eight-byte pagemap entry"),
                    ) & ((1u64 << 63) | (1u64 << 62))
                        == 1u64 << 62
                };
                if let Some(first) = (0..count).find(|i| swapped(*i)) {
                    let pages = (first..count)
                        .take((bound / page) as usize)
                        .take_while(|i| swapped(*i))
                        .count();
                    cursor.maps_offset = offset;
                    cursor.address = address + first as u64 * page;
                    return Ok(Scan::Range {
                        address: cursor.address,
                        bytes: pages as u64 * page,
                    });
                }
                address += count as u64 * page;
            }
        }
        cursor.maps_offset = maps.stream_position()?;
        cursor.address = 0;
    }
    Ok(Scan::More)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn cursor() -> ProcessCursor {
        ProcessCursor {
            target: Identity {
                uid: 0,
                pid: 42,
                start_ticks: 7,
                inode: 1,
                cgroup: "/target".into(),
            },
            layout: "layout".into(),
            maps_offset: 0,
            address: 0,
        }
    }
    #[test]
    fn pid_reuse_exec_and_native_migration_reset_discovery_hints() {
        let mut hint = cursor();
        hint.maps_offset = 42;
        hint.address = 4096;
        hint.revalidate(hint.target.clone(), hint.layout.clone());
        assert_eq!(hint.maps_offset, 42);
        for (identity, layout) in [
            (
                Identity {
                    start_ticks: 8,
                    ..hint.target.clone()
                },
                hint.layout.clone(),
            ),
            (hint.target.clone(), "new-layout".into()),
            (
                Identity {
                    cgroup: "/moved".into(),
                    inode: 2,
                    ..hint.target.clone()
                },
                hint.layout.clone(),
            ),
        ] {
            hint.maps_offset = 42;
            hint.address = 4096;
            hint.revalidate(identity, layout);
            assert_eq!(hint.maps_offset, 0);
            assert_eq!(hint.address, 0);
        }
    }
    #[test]
    fn process_frontier_reaches_a_target_beyond_512_and_survives_restart() {
        let root = std::env::temp_dir().join(format!(
            "amc-discovery-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        for pid in 1..=600 {
            fs::create_dir(root.join(pid.to_string())).unwrap();
        }
        let first = candidates_at(&root, 0).unwrap();
        assert_eq!(first.len(), 512);
        let mut discovery = Discovery {
            boot_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            groups: vec!["/target".into()],
            after_pid: 512,
            active: None,
        };
        let (store, _) =
            crate::store::Store::open_discovery(&root.join("state"), &discovery.boot_id).unwrap();
        store.save_discovery(&discovery).unwrap();
        drop(store);
        let (_, restored) =
            crate::store::Store::open_discovery(&root.join("state"), &discovery.boot_id).unwrap();
        assert_eq!(
            candidates_at(&root, restored.after_pid).unwrap(),
            (513..=600).collect::<Vec<_>>()
        );
        discovery.active = Some(cursor());
        discovery.finish_process();
        assert_eq!(discovery.after_pid, 42);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn streaming_frontier_reaches_beyond_both_mapping_and_page_cutoffs() {
        let root = std::env::temp_dir().join(format!(
            "amc-vma-discovery-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let page = page_return::page_size().unwrap();
        let first = 8192 * page;
        let end = first + page * 8_388_610;
        let mut maps = File::create(root.join("maps")).unwrap();
        for i in 0..8192 {
            writeln!(maps, "{:x}-{:x} ---p 0 00:00 0", i * page, (i + 1) * page).unwrap();
        }
        writeln!(maps, "{first:x}-{end:x} rw-p 0 00:00 0").unwrap();
        drop(maps);
        let map = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("pagemap"))
            .unwrap();
        map.set_len((8192 + 8_388_610) * 8).unwrap();
        map.write_all_at(&(1u64 << 62).to_ne_bytes(), (8192 + 8_388_609) * 8)
            .unwrap();
        let mut cursor = cursor();
        let scan_window = |cursor: &mut ProcessCursor| {
            scan_at(
                cursor,
                File::open(root.join("maps")).unwrap(),
                &map,
                page,
                8192,
                8_388_608,
            )
            .unwrap()
        };
        assert_eq!(scan_window(&mut cursor), Scan::More);
        assert!(cursor.maps_offset > 0);
        let mut cursor: ProcessCursor =
            serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
        assert_eq!(scan_window(&mut cursor), Scan::More);
        assert_eq!(cursor.address, first + page * 8_388_608);
        let found = Scan::Range {
            address: first + page * 8_388_609,
            bytes: page,
        };
        assert_eq!(scan_window(&mut cursor), found);
        // Interrupted/denied batches replay the same range, not a later page.
        assert_eq!(scan_window(&mut cursor), found);
        cursor.address += page;
        assert_eq!(scan_window(&mut cursor), Scan::Complete);
        fs::remove_dir_all(root).unwrap();
    }
}
