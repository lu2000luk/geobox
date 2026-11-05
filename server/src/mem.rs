//! Linux: keeping the memory-mapped index from piling up in RAM. The kernel keeps every page a
//! search touched until memory runs out, and until then those pages count as the container's memory.

use std::path::{Path, PathBuf};

/// Resident file-backed pages of this process (index and code), from /proc/self/statm.
pub fn resident_file_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    // SAFETY: sysconf has no preconditions
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
    statm.split(' ').nth(2).and_then(|p| p.parse::<u64>().ok()).unwrap_or(0) * page
}

/// Address ranges and files of the file mappings under `dir` (all of them when `None`), in address order.
pub(crate) fn file_mappings(dir: Option<&Path>) -> std::io::Result<Vec<(usize, usize, PathBuf)>> {
    let mut out = Vec::new();
    for line in std::fs::read_to_string("/proc/self/maps")?.lines() {
        // "start-end perms offset dev inode      path"
        let mut parts = line.splitn(6, ' ');
        let (Some(range), Some(path)) = (parts.next(), parts.nth(4)) else { continue };
        let path = Path::new(path.trim_start());
        if !dir.map_or(path.is_absolute(), |d| path.starts_with(d)) {
            continue;
        }
        let Some((a, b)) = range.split_once('-') else { continue };
        if let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) {
            out.push((a, b, path.to_path_buf()));
        }
    }
    Ok(out)
}

/// Lets the kernel reclaim the pages of `start..end` now; they are re-read from the file on demand.
pub(crate) fn page_out(start: usize, end: usize) {
    // SAFETY: MADV_PAGEOUT never changes memory contents; a range unmapped meanwhile just fails
    // with ENOMEM, and an address reused by another mapping is at worst paged out too.
    unsafe { libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_PAGEOUT) };
}

/// Drops the file's pages from the page cache unless something still maps them. Pages read without a
/// mapping (by exec, by `read`) or left behind by an unmapped file stay cached otherwise, and count as
/// the container's memory.
pub(crate) fn drop_cached(path: &Path) {
    use std::os::fd::AsRawFd;
    if let Ok(f) = std::fs::File::open(path) {
        // SAFETY: valid open fd; the call is only advice
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_and_pages_out_a_mapped_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, vec![7u8; 16 << 20]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        // SAFETY: the file is not modified while mapped
        let map = unsafe { memmap2::Mmap::map(&file) }.unwrap();
        assert_eq!(map.iter().map(|&b| b as u64).sum::<u64>(), 7 << 24);
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let ranges = super::file_mappings(Some(&dir)).unwrap();
        assert_eq!(ranges, vec![(map.as_ptr() as usize, map.as_ptr() as usize + map.len(), dir.join("f"))]);
        // the kernel may sync its per-thread RSS counters lazily, hence the margin
        assert!(super::resident_file_bytes() >= 8 << 20);
        super::page_out(ranges[0].0, ranges[0].1);
        super::drop_cached(&ranges[0].2);
        assert_eq!(map[12345], 7, "contents survive a page-out");
    }
}
