//! Integrity hashing for the optional verification pass (BLAKE3 or xxh3).

use std::fs::OpenOptions;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use xxhash_rust::xxh3::Xxh3;

use super::aligned::AlignedBuf;
use super::{BLOCK, Control};
use crate::error::{Error, IoContext, Result};
use crate::winapi::{self, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, SHARE_ALL};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum HashAlgo {
    #[default]
    Blake3,
    Xxh3,
}

pub enum Hasher {
    Blake3(Box<blake3::Hasher>),
    Xxh3(Box<Xxh3>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest(pub Vec<u8>);

impl Digest {
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl Hasher {
    pub fn new(algo: HashAlgo) -> Hasher {
        match algo {
            HashAlgo::Blake3 => Hasher::Blake3(Box::new(blake3::Hasher::new())),
            HashAlgo::Xxh3 => Hasher::Xxh3(Box::new(Xxh3::new())),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            // multi-threaded for big blocks, plain for small ones
            Hasher::Blake3(h) => {
                if data.len() >= 1 << 20 {
                    h.update_rayon(data);
                } else {
                    h.update(data);
                }
            }
            Hasher::Xxh3(h) => h.update(data),
        }
    }

    pub fn finish(self) -> Digest {
        match self {
            Hasher::Blake3(h) => Digest(h.finalize().as_bytes().to_vec()),
            Hasher::Xxh3(h) => Digest(h.digest128().to_le_bytes().to_vec()),
        }
    }
}

pub fn hash_bytes(algo: HashAlgo, data: &[u8]) -> Digest {
    let mut h = Hasher::new(algo);
    h.update(data);
    h.finish()
}

/// Hashes a whole file. Reads unbuffered (so a just-written file is verified from the disk, not from the cache)
/// with a reader thread feeding the hasher.
pub fn hash_file(path: &Path, algo: HashAlgo, ctrl: &Control, bytes_counter: Option<&std::sync::atomic::AtomicU64>) -> Result<Digest> {
    let p = winapi::long_path(path);
    let file = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(&p)
        .at(path)?;
    let size = file.metadata().at(path)?.len();
    let (full_tx, full_rx) = crossbeam_channel::bounded::<(AlignedBuf, usize)>(2);
    let (free_tx, free_rx) = crossbeam_channel::bounded::<AlignedBuf>(3);
    for _ in 0..3 {
        let _ = free_tx.send(AlignedBuf::new(BLOCK));
    }
    let mut hasher = Hasher::new(algo);
    let result: Result<()> = std::thread::scope(|s| {
        let reader = s.spawn(move || -> Result<()> {
            let mut off = 0u64;
            while off < size {
                let Ok(mut buf) = free_rx.recv() else { return Ok(()) };
                let want = (size - off).min(BLOCK as u64) as usize;
                let padded = want.next_multiple_of(super::aligned::ALIGN);
                let mut got = 0;
                while got < want {
                    let n = file.seek_read(&mut buf[got..padded], off + got as u64).at(path)?;
                    if n == 0 {
                        return Err(Error::Verify(format!("{}: файл изменился во время чтения", path.display())));
                    }
                    got += n;
                }
                off += want as u64;
                if full_tx.send((buf, want.min(got))).is_err() {
                    return Ok(());
                }
            }
            Ok(())
        });
        for (buf, n) in full_rx {
            if ctrl.is_cancelled() {
                break;
            }
            ctrl.wait_if_paused();
            hasher.update(&buf[..n]);
            if let Some(c) = bytes_counter {
                c.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = free_tx.send(buf);
        }
        drop(free_tx);
        reader.join().unwrap_or_else(|_| Err(Error::Verify("поток чтения аварийно завершился".into())))
    });
    if ctrl.is_cancelled() {
        return Err(Error::Cancelled);
    }
    result?;
    Ok(hasher.finish())
}

/// Compares a finished copy with its source (used by mechanisms that do not hash while copying).
pub fn verify_copy(src: &Path, dst: &Path, algo: HashAlgo, size: u64, ctrl: &Control) -> Result<()> {
    let same = if size < super::SMALL_MAX {
        let a = std::fs::read(winapi::long_path(src)).at(src)?;
        let b = std::fs::read(winapi::long_path(dst)).at(dst)?;
        hash_bytes(algo, &a) == hash_bytes(algo, &b)
    } else {
        hash_file(src, algo, ctrl, None)? == hash_file(dst, algo, ctrl, None)?
    };
    if same {
        Ok(())
    } else {
        let _ = std::fs::remove_file(winapi::long_path(dst));
        Err(Error::Verify(format!("{}: копия не совпала с оригиналом", dst.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithms_differ_but_are_stable() {
        let a = hash_bytes(HashAlgo::Blake3, b"hello");
        let b = hash_bytes(HashAlgo::Xxh3, b"hello");
        assert_eq!(a.0.len(), 32);
        assert_eq!(b.0.len(), 16);
        assert_eq!(a, hash_bytes(HashAlgo::Blake3, b"hello"));
        assert_ne!(hash_bytes(HashAlgo::Blake3, b"hellp"), a);
        assert_eq!(a.hex(), "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f");
    }

    #[test]
    fn file_hash_matches_in_memory_hash_for_odd_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl = Control::default();
        for size in [0usize, 1, 4095, 4096, 4097, BLOCK - 1, BLOCK, BLOCK + 1, 3 * BLOCK + 123] {
            let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
            let p = dir.path().join(format!("f{size}"));
            std::fs::write(&p, &data).unwrap();
            for algo in [HashAlgo::Blake3, HashAlgo::Xxh3] {
                assert_eq!(hash_file(&p, algo, &ctrl, None).unwrap(), hash_bytes(algo, &data), "{algo:?} {size}");
            }
        }
    }
}
