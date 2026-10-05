//! Loopback ports for tests, shared safely by test binaries running at once.
//!
//! A fixed port (the 8100-8199 engine default, say) collides as soon as two
//! test binaries that stand engines or listeners up run in parallel, and a port
//! learned by binding `:0` and releasing it can be taken back by anything that
//! binds next. This allocator hands out ports from blocks below the kernel's
//! ephemeral range (where outbound connections take their local ports), and a
//! test binary claims each block it uses with an exclusive lock on a file under
//! the system temporary directory, held until the binary exits. Two test
//! binaries therefore never hand out the same port, and within one binary a
//! port is never handed out twice. A port some other program already listens
//! on is skipped.

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::Mutex;

/// The lowest port the pool uses; below it sit well-known and registered
/// services a developer machine may run.
const POOL_LOW: u16 = 20_000;
/// Ports per claimed block; also the most one request may ask for.
const BLOCK: u16 = 128;

/// The blocks this test binary holds and the next port it has not handed out.
struct Pool {
    /// Lock files of every claimed block; dropping one would release it.
    _locks: Vec<File>,
    next: u16,
    end: u16,
}

static POOL: Mutex<Option<Pool>> = Mutex::new(None);

/// The pool's upper bound (exclusive): the start of the kernel's ephemeral
/// range.
fn pool_high() -> u16 {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<u16>().ok())
        .filter(|low| *low >= POOL_LOW + 8 * BLOCK)
        .unwrap_or(32_768)
}

/// A random number, so concurrent test binaries start claiming in different
/// places.
fn random() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    hasher.finish()
}

fn lock_dir() -> PathBuf {
    std::env::temp_dir().join("capyctl-test-ports")
}

/// Claims one block no other test binary holds: `(lock, first port)`.
fn claim_block() -> (File, u16) {
    let dir = lock_dir();
    std::fs::create_dir_all(&dir).expect("the test port lock directory");
    let blocks = (pool_high() - POOL_LOW) / BLOCK;
    let first = (random() % u64::from(blocks)) as u16;
    for offset in 0..blocks {
        let start = POOL_LOW + ((first + offset) % blocks) * BLOCK;
        let path = dir.join(format!("block-{start}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .or_else(|_| File::open(&path));
        let Ok(file) = file else { continue };
        if file.try_lock().is_ok() {
            return (file, start);
        }
    }
    panic!("every test port block below the ephemeral range is held");
}

fn bindable(port: u16) -> Option<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).ok()
}

/// `count` loopback ports (at most 128), `consecutive` or not, each bindable
/// at the moment the whole set is chosen and none handed out before by any
/// test binary running now.
pub fn free_ports(count: usize, consecutive: bool) -> Vec<u16> {
    assert!(
        count > 0 && count <= usize::from(BLOCK),
        "1 to {BLOCK} ports"
    );
    let mut guard = POOL.lock().unwrap_or_else(|error| error.into_inner());
    let pool = guard.get_or_insert_with(|| Pool {
        _locks: Vec::new(),
        next: 0,
        end: 0,
    });
    for _ in 0..64 {
        let mut chosen = Vec::with_capacity(count);
        // Held until the whole set is chosen, so no port is counted twice.
        let mut held = Vec::with_capacity(count);
        while chosen.len() < count && pool.next < pool.end {
            let port = pool.next;
            pool.next += 1;
            match bindable(port) {
                Some(listener) => {
                    held.push(listener);
                    chosen.push(port);
                }
                // A run broken by a busy port starts again after it.
                None if consecutive => {
                    chosen.clear();
                    held.clear();
                }
                None => {}
            }
        }
        if chosen.len() == count {
            return chosen;
        }
        // Not enough left in this block: what was chosen goes unused, and the
        // next block continues.
        let (lock, start) = claim_block();
        pool._locks.push(lock);
        pool.next = start;
        pool.end = start + BLOCK;
    }
    panic!("no {count} free loopback ports below the ephemeral range");
}

/// One loopback port (see [`free_ports`]).
pub fn free_port() -> u16 {
    free_ports(1, false)[0]
}

/// The engine port range of this test binary's Fake installation: 100
/// consecutive ports, the size of the product's default range, chosen once.
pub fn fake_engine_ports() -> (u16, u16) {
    static RANGE: std::sync::OnceLock<(u16, u16)> = std::sync::OnceLock::new();
    *RANGE.get_or_init(|| {
        let ports = free_ports(100, true);
        (ports[0], ports[99])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_distinct_bindable_and_below_the_ephemeral_range() {
        let single: Vec<u16> = (0..200).map(|_| free_port()).collect();
        let run = free_ports(8, true);
        assert!(run.windows(2).all(|pair| pair[1] == pair[0] + 1));
        let mut all = single.clone();
        all.extend(&run);
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count, "no port is handed out twice");
        assert!(all
            .iter()
            .all(|port| (POOL_LOW..pool_high()).contains(port)));
        assert!(all.iter().all(|port| bindable(*port).is_some()));
        let (low, high) = fake_engine_ports();
        assert_eq!(high - low, 99);
        assert_eq!(fake_engine_ports(), (low, high), "chosen once");
    }
}
