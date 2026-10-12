//! Durable discovery hints, never admission or residency proofs. A campaign
//! advances through host PIDs and streams VMAs/pagemap in bounded windows.
//! Reopening a cursor always reidentifies the live target; the broker separately
//! rechecks placement, swapped PTEs and native backing before every batch.
use crate::{host::Identity, page_return, recovery::RecoveryPolicy};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{FileExt, MetadataExt},
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
    /// Exact maps-stream digest. Old snapshots must rescan their entire prefix.
    #[serde(default)]
    pub maps_fingerprint: Option<[u8; 32]>,
}

impl ProcessCursor {
    pub fn revalidate(&mut self, target: Identity, layout: String) {
        if self.target != target || self.layout != layout {
            *self = Self {
                target,
                layout,
                maps_offset: 0,
                address: 0,
                maps_fingerprint: None,
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

/// This campaign must revisit the complete PID frontier before declaring
/// success. An inherited cursor says nothing about the old prefix's residency.
pub struct Sweep {
    began_at_start: bool,
}

impl Sweep {
    pub fn new(discovery: &Discovery) -> Self {
        Self {
            began_at_start: discovery.after_pid == 0 && discovery.active.is_none(),
        }
    }
    pub fn wrap(&mut self, before: [u8; 32], after: [u8; 32]) -> bool {
        std::mem::replace(&mut self.began_at_start, true) && before == after
    }
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
    let group = placement_at(Path::new("/proc"), pid)?;
    Ok(selected_group(&group, policy))
}

fn placement_at(proc: &Path, pid: i32) -> Result<String> {
    let group = fs::read_to_string(proc.join(pid.to_string()).join("cgroup"))?;
    let group = group
        .lines()
        .find_map(|s| s.strip_prefix("0::"))
        .context("missing native placement")?;
    crate::native::cgroup_directory(group)?;
    Ok(group.into())
}

fn selected_group(group: &str, policy: &RecoveryPolicy) -> bool {
    policy
        .page_cgroups
        .iter()
        .any(|prefix| group == prefix || group.starts_with(&format!("{prefix}/")))
}

pub fn layout(pid: i32) -> Result<String> {
    Ok(process_signature_at(Path::new("/proc"), pid)?.1)
}

fn process_signature_at(proc: &Path, pid: i32) -> Result<(u64, String)> {
    let stat = fs::read_to_string(proc.join(pid.to_string()).join("stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .context("invalid native process stat")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    let start = fields
        .get(19)
        .context("missing process lifetime")?
        .parse::<u64>()?;
    ensure!(start > 0, "invalid process lifetime");
    // startcode/endcode/startstack and data/argument/environment bounds change
    // on ordinary exec. Even same-layout exec cannot turn a hint into authority:
    // the complete sweep wraps and native descriptors/settlement are rechecked.
    let indices = [23, 24, 25, 42, 43, 44, 45, 46, 47, 48];
    indices
        .into_iter()
        .map(|i| fields.get(i).copied().context("short native process stat"))
        .collect::<Result<Vec<_>>>()
        .map(|v| (start, v.join(" ")))
}

/// A bounded-memory, ordered digest of all selected native process lifetimes,
/// placements, mappings and PTE return state. Destination counters omit migrated
/// debt; PID identity alone cannot certify an address space already scanned.
pub fn frontier(policy: &RecoveryPolicy) -> Result<[u8; 32]> {
    frontier_at(Path::new("/proc"), Path::new("/sys/fs/cgroup"), policy)
}

fn frontier_at(proc: &Path, groups: &Path, policy: &RecoveryPolicy) -> Result<[u8; 32]> {
    let deadline = std::time::Instant::now() + page_return::INVENTORY_TIMEOUT;
    let mut hash = Sha256::new();
    let mut after = 0;
    loop {
        ensure!(
            std::time::Instant::now() < deadline,
            "selected PID frontier exceeded time bound"
        );
        let pids = candidates_at(proc, after)?;
        if pids.is_empty() {
            return Ok(hash.finalize().into());
        }
        for pid in pids {
            ensure!(
                std::time::Instant::now() < deadline,
                "selected PID frontier exceeded time bound"
            );
            after = pid;
            let entry = (|| -> Result<_> {
                let group = placement_at(proc, pid)?;
                if !selected_group(&group, policy) {
                    return Ok(None);
                }
                let signature = process_signature_at(proc, pid)?;
                let uid = fs::metadata(proc.join(pid.to_string()))?.uid();
                let inode = fs::metadata(groups.join(group.trim_start_matches('/')))?.ino();
                let mut maps = File::open(proc.join(pid.to_string()).join("maps"))?;
                let mappings = mapping_fingerprint(&mut maps)?;
                let mut source = maps.try_clone()?;
                let pagemap = File::open(proc.join(pid.to_string()).join("pagemap"))?;
                let mut state = Sha256::new();
                state.update(mappings);
                let mut cursor = ProcessCursor {
                    target: Identity {
                        pid,
                        start_ticks: signature.0,
                        uid,
                        inode,
                        cgroup: group.clone(),
                    },
                    layout: signature.1.clone(),
                    maps_offset: 0,
                    address: 0,
                    maps_fingerprint: Some(mappings),
                };
                ensure!(
                    scan_stream(
                        &mut cursor,
                        maps,
                        &pagemap,
                        page_return::page_size()?,
                        (usize::MAX, u64::MAX),
                        Some((&mut state, deadline))
                    )? == Scan::Complete,
                    "selected address-space frontier is incomplete"
                );
                ensure!(
                    placement_at(proc, pid)? == group
                        && process_signature_at(proc, pid)? == signature
                        && mapping_fingerprint(&mut source)? == mappings,
                    "selected process changed during frontier observation"
                );
                Ok(Some((group, signature, uid, inode, state.finalize())))
            })();
            match entry {
                Ok(Some((group, (start, layout), uid, inode, state))) => {
                    hash.update(pid.to_le_bytes());
                    hash.update(start.to_le_bytes());
                    hash.update(uid.to_le_bytes());
                    hash.update(inode.to_le_bytes());
                    hash.update(state);
                    for text in [group, layout] {
                        hash.update((text.len() as u64).to_le_bytes());
                        hash.update(text.as_bytes());
                    }
                }
                Ok(None) => (),
                Err(_) if !proc.join(pid.to_string()).exists() => (),
                Err(error) => return Err(error),
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Scan {
    Range { address: u64, bytes: u64 },
    More,
    Complete,
}

/// Byte offsets apply only to the exact mapping stream that produced them.
/// mmap/munmap changes reset the entire process frontier before any resume.
/// A found range stays at its first page until the caller proves settlement.
pub fn scan(cursor: &mut ProcessCursor, pagemap: &File, bound: u64) -> Result<Scan> {
    let maps = File::open(format!("/proc/{}/maps", cursor.target.pid))?;
    scan_at(cursor, maps, pagemap, bound, 8192, 8_388_608)
}

fn mapping_fingerprint(maps: &mut File) -> Result<[u8; 32]> {
    let deadline = std::time::Instant::now() + page_return::INVENTORY_TIMEOUT;
    maps.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        ensure!(
            std::time::Instant::now() < deadline,
            "native mapping fingerprint exceeded time bound"
        );
        let count = maps.read(&mut buffer)?;
        if count == 0 {
            return Ok(hash.finalize().into());
        }
        hash.update(&buffer[..count]);
    }
}

fn scan_at(
    cursor: &mut ProcessCursor,
    mut maps: File,
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
    let fingerprint = mapping_fingerprint(&mut maps)?;
    if cursor.maps_fingerprint != Some(fingerprint) {
        cursor.maps_offset = 0;
        cursor.address = 0;
        cursor.maps_fingerprint = Some(fingerprint);
    }
    let mut source = maps.try_clone()?;
    let result = scan_stream(
        cursor,
        maps,
        pagemap,
        bound,
        (map_budget, page_budget),
        None,
    )?;
    // The bounded window reads the live stream without retaining it in RAM.
    // No range, resume or EOF is trusted if mmap/munmap changed that stream.
    if mapping_fingerprint(&mut source)? != fingerprint {
        cursor.maps_offset = 0;
        cursor.address = 0;
        cursor.maps_fingerprint = None;
        return Ok(Scan::More);
    }
    Ok(result)
}

fn scan_stream(
    cursor: &mut ProcessCursor,
    maps: File,
    pagemap: &File,
    bound: u64,
    budget: (usize, u64),
    mut completion: Option<(&mut Sha256, std::time::Instant)>,
) -> Result<Scan> {
    let (map_budget, page_budget) = budget;
    let page = page_return::page_size()?;
    let mut maps = BufReader::new(maps);
    maps.seek(SeekFrom::Start(cursor.maps_offset))?;
    let mut entries = [0u8; 4096];
    let mut scanned = 0;
    for _ in 0..map_budget {
        ensure!(
            completion
                .as_ref()
                .is_none_or(|(_, deadline)| std::time::Instant::now() < *deadline),
            "selected address-space frontier exceeded time bound"
        );
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
        let permissions = fields
            .next()
            .context("missing native mapping permissions")?;
        // x86's fixed kernel vsyscall page is not a swappable user VMA and
        // has no pagemap entry. Do not generalize this exception to user VMAs.
        if cfg!(target_arch = "x86_64")
            && first == 0xffffffffff600000
            && last == 0xffffffffff601000
            && permissions == "--xp"
            && fields.nth(3) == Some("[vsyscall]")
        {
            cursor.maps_offset = maps.stream_position()?;
            cursor.address = 0;
            continue;
        }
        let readable = permissions.starts_with('r') && permissions.ends_with('p');
        let mut address =
            if cursor.maps_offset == offset && (first..=last).contains(&cursor.address) {
                cursor.address
            } else {
                first
            };
        while address < last {
            ensure!(
                completion
                    .as_ref()
                    .is_none_or(|(_, deadline)| std::time::Instant::now() < *deadline),
                "selected address-space frontier exceeded time bound"
            );
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
            if let Some((hash, _)) = completion.as_mut() {
                // Ignore PFNs/soft-dirty churn. Only residency and swap state
                // affect whether this address space still owes page return.
                for entry in entries[..count * 8].chunks_exact(8) {
                    hash.update([(u64::from_ne_bytes(
                        entry.try_into().expect("eight-byte pagemap entry"),
                    ) >> 62) as u8]);
                }
                address += count as u64 * page;
                continue;
            }
            let swapped = |i: usize| {
                u64::from_ne_bytes(
                    entries[i * 8..i * 8 + 8]
                        .try_into()
                        .expect("eight-byte pagemap entry"),
                ) & ((1u64 << 63) | (1u64 << 62))
                    == 1u64 << 62
            };
            if let Some(first) = (0..count).find(|i| swapped(*i)) {
                ensure!(readable, "swapped PTE in an unsupported native mapping");
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
            maps_fingerprint: None,
        }
    }
    #[test]
    fn selected_process_entering_behind_pid_frontier_prevents_completion() {
        let root = std::env::temp_dir().join(format!(
            "amc-pid-migration-{}",
            crate::store::fresh_id().unwrap()
        ));
        let proc = root.join("proc");
        let groups = root.join("groups");
        fs::create_dir_all(groups.join("selected")).unwrap();
        let policy = RecoveryPolicy {
            cgroup: "/helper.service".into(),
            helper_bytes: 1048576,
            minimum_bytes: 1,
            targets: vec![],
            page_cgroups: vec!["/selected".into()],
            batch_bytes: 4096,
        };
        for (pid, group) in [(42, "/outside"), (600, "/selected")] {
            fs::create_dir_all(proc.join(pid.to_string())).unwrap();
            fs::write(
                proc.join(pid.to_string()).join("cgroup"),
                format!("0::{group}\n"),
            )
            .unwrap();
            let mut fields = vec!["0"; 49];
            fields[0] = "S";
            fields[19] = "7";
            fs::write(
                proc.join(pid.to_string()).join("stat"),
                format!("{pid} (target) {}\n", fields.join(" ")),
            )
            .unwrap();
            fs::write(
                proc.join(pid.to_string()).join("maps"),
                "0-1000 rw-p 0 00:00 0\n",
            )
            .unwrap();
            fs::write(
                proc.join(pid.to_string()).join("pagemap"),
                (1u64 << 63).to_ne_bytes(),
            )
            .unwrap();
        }
        let mut discovery = Discovery {
            boot_id: "boot".into(),
            groups: policy.page_cgroups.clone(),
            after_pid: 0,
            active: None,
        };
        let mut sweep = Sweep::new(&discovery);
        let before = frontier_at(&proc, &groups, &policy).unwrap();
        // PID 42 was outside the selection when its position was passed. Its
        // original memcg can retain all swap charges after this migration.
        discovery.after_pid = 600;
        assert!(
            candidates_at(&proc, discovery.after_pid)
                .unwrap()
                .is_empty()
        );
        fs::write(proc.join("42/cgroup"), "0::/selected\n").unwrap();
        let after = frontier_at(&proc, &groups, &policy).unwrap();
        assert_ne!(before, after);
        assert!(
            !sweep.wrap(before, after),
            "zero destination counters must not accept a process missed behind the PID sweep"
        );
        discovery.wrap();
        assert_eq!(
            candidates_at(&proc, discovery.after_pid).unwrap(),
            [42, 600]
        );
        assert!(sweep.wrap(after, frontier_at(&proc, &groups, &policy).unwrap()));
        let stable = frontier_at(&proc, &groups, &policy).unwrap();
        // PID/layout/placement are unchanged when an earlier mapping becomes
        // swapped with debt still charged outside the selected destination.
        fs::write(proc.join("42/pagemap"), (1u64 << 62).to_ne_bytes()).unwrap();
        assert!(
            !sweep.wrap(stable, frontier_at(&proc, &groups, &policy).unwrap()),
            "a newly swapped PTE behind the scan must prevent successful completion"
        );
        let swapped = frontier_at(&proc, &groups, &policy).unwrap();
        fs::write(proc.join("42/maps"), "0-1000 rw-p 0 00:00 0 /new-mapping\n").unwrap();
        assert!(
            !sweep.wrap(swapped, frontier_at(&proc, &groups, &policy).unwrap()),
            "VMA replacement behind the completed PID must also invalidate completion"
        );
        let old = fs::read_to_string(proc.join("42/stat")).unwrap();
        fs::write(proc.join("42/stat"), old.replacen(" 7 ", " 8 ", 1)).unwrap();
        assert!(
            !sweep.wrap(after, frontier_at(&proc, &groups, &policy).unwrap()),
            "PID reuse must also restart the full sweep"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mapping_stream_above_snapshot_cap_resumes_to_swapped_tail() {
        let root = std::env::temp_dir().join(format!(
            "amc-large-maps-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let page = page_return::page_size().unwrap();
        let padding = "p".repeat(8192);
        let mut maps = std::io::BufWriter::new(File::create(root.join("maps")).unwrap());
        for i in 0..8193 {
            writeln!(
                maps,
                "{:x}-{:x} rw-p 0 00:00 0 /{padding}",
                i * page,
                (i + 1) * page
            )
            .unwrap();
        }
        maps.flush().unwrap();
        drop(maps);
        assert!(fs::metadata(root.join("maps")).unwrap().len() > 64 * 1024 * 1024);
        let pagemap = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("pagemap"))
            .unwrap();
        pagemap.set_len(8193 * 8).unwrap();
        pagemap
            .write_all_at(&(1u64 << 62).to_ne_bytes(), 8192 * 8)
            .unwrap();
        let window = |cursor: &mut ProcessCursor| {
            scan_at(
                cursor,
                File::open(root.join("maps")).unwrap(),
                &pagemap,
                page,
                8192,
                8192,
            )
        };
        let mut cursor = cursor();
        assert_eq!(window(&mut cursor).unwrap(), Scan::More);
        let mut cursor: ProcessCursor =
            serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
        assert_eq!(
            window(&mut cursor).unwrap(),
            Scan::Range {
                address: 8192 * page,
                bytes: page
            }
        );
        pagemap
            .write_all_at(&(1u64 << 63).to_ne_bytes(), 8192 * 8)
            .unwrap();
        cursor.address += page;
        assert_eq!(window(&mut cursor).unwrap(), Scan::Complete);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_vma_prefix_cannot_skip_a_swapped_mapping_at_a_valid_byte_offset() {
        let root = std::env::temp_dir().join(format!(
            "amc-vma-churn-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let page = page_return::page_size().unwrap();
        let line = |first, last| format!("{first:016x}-{last:016x} rw-p 0 00:00 0\n");
        fs::write(
            root.join("maps"),
            line(page, 2 * page) + &line(3 * page, 4 * page),
        )
        .unwrap();
        let pagemap = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("pagemap"))
            .unwrap();
        pagemap.set_len(4 * 8).unwrap();
        pagemap
            .write_all_at(&(1u64 << 62).to_ne_bytes(), 0)
            .unwrap();
        let mut cursor = cursor();
        assert_eq!(
            scan_at(
                &mut cursor,
                File::open(root.join("maps")).unwrap(),
                &pagemap,
                page,
                1,
                8
            )
            .unwrap(),
            Scan::More
        );
        // mmap/munmap replaced the already-scanned prefix with an equally long
        // stream. The byte offset remains a valid boundary but has lost authority.
        fs::write(root.join("maps"), line(0, page) + &line(3 * page, 4 * page)).unwrap();
        assert_eq!(
            scan_at(
                &mut cursor,
                File::open(root.join("maps")).unwrap(),
                &pagemap,
                page,
                8,
                8
            )
            .unwrap(),
            Scan::Range {
                address: 0,
                bytes: page
            }
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn swapped_pages_in_unreadable_or_shared_mappings_prevent_complete_discovery() {
        let root = std::env::temp_dir().join(format!(
            "amc-skipped-vma-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let page = page_return::page_size().unwrap();
        let pagemap = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("pagemap"))
            .unwrap();
        pagemap.set_len(3 * 8).unwrap();
        for permission in ["---p", "rw-s", "r--s"] {
            fs::write(
                root.join("maps"),
                format!("{page:x}-{:x} {permission} 0 00:00 0\n", 2 * page),
            )
            .unwrap();
            pagemap.write_all_at(&0u64.to_ne_bytes(), 8).unwrap();
            assert_eq!(
                scan_at(
                    &mut cursor(),
                    File::open(root.join("maps")).unwrap(),
                    &pagemap,
                    page,
                    8,
                    8
                )
                .unwrap(),
                Scan::Complete
            );
            pagemap
                .write_all_at(&(1u64 << 62).to_ne_bytes(), 8)
                .unwrap();
            assert!(
                scan_at(
                    &mut cursor(),
                    File::open(root.join("maps")).unwrap(),
                    &pagemap,
                    page,
                    8,
                    8
                )
                .is_err(),
                "unfaultable swapped {permission} mapping appeared complete"
            );
        }
        if cfg!(target_arch = "x86_64") {
            fs::write(
                root.join("maps"),
                "ffffffffff600000-ffffffffff601000 --xp 0 00:00 0 [vsyscall]\n",
            )
            .unwrap();
            assert_eq!(
                scan_at(
                    &mut cursor(),
                    File::open(root.join("maps")).unwrap(),
                    &pagemap,
                    page,
                    8,
                    8
                )
                .unwrap(),
                Scan::Complete
            );
        }
        fs::remove_dir_all(root).unwrap();
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
        let mut sweep = Sweep::new(&restored);
        // Reaching EOF from an inherited frontier is progress, not a proof
        // about the first 512 processes from an interrupted campaign.
        assert!(candidates_at(&root, 600).unwrap().is_empty());
        assert!(!sweep.wrap([0; 32], [0; 32]));
        discovery.wrap();
        assert_eq!(candidates_at(&root, discovery.after_pid).unwrap(), first);
        assert!(sweep.wrap([0; 32], [0; 32]));
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
