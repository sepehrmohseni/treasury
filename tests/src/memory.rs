//! Process memory, for the memory budget checks.

/// The resident set size of this process in bytes, where the OS reports it (Linux).
pub fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    #[test]
    fn rss_is_readable_on_linux() {
        if cfg!(target_os = "linux") {
            assert!(super::rss_bytes().unwrap() > 1024 * 1024);
        }
    }
}
