//! Mesures mémoire du système (Linux, `/proc`).

/// Mémoire du système, en octets.
#[derive(Debug, Clone, Copy)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
}

impl MemInfo {
    /// RAM utilisée par tout le système (SPEC.md §5.1).
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

fn field_kb(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with(name) && l.as_bytes().get(name.len()) == Some(&b':'))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

pub fn meminfo() -> Option<MemInfo> {
    let t = std::fs::read_to_string("/proc/meminfo").ok()?;
    Some(MemInfo {
        total: field_kb(&t, "MemTotal")? * 1024,
        available: field_kb(&t, "MemAvailable")? * 1024,
    })
}

/// Nombre cumulé de pages envoyées en swap par le système (`/proc/vmstat`).
/// Son accroissement signale que la RAM est saturée, quel que soit le processus.
pub fn pswpout() -> Option<u64> {
    let t = std::fs::read_to_string("/proc/vmstat").ok()?;
    t.lines()
        .find(|l| l.starts_with("pswpout "))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Mémoire résidente du processus, en octets.
pub fn rss() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096)
}
