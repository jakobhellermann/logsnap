//! The filesystem abstraction: the only contact with the outside world. A real
//! [`OsFs`] for the binary, an in-memory [`MemFs`] for tests and the demo (which
//! models log rotation and truncation explicitly, without touching disk).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Copy)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
}

/// `stat` identifies a file (device+inode, for rotation detection); `read` returns
/// its whole current contents (cursor math then happens on the bytes).
pub trait Fs {
    fn stat(&self, path: &str) -> Option<Stat>;
    fn read(&self, path: &str) -> Option<Vec<u8>>;
}

/// Real filesystem backing for the binary.
pub struct OsFs;

impl Fs for OsFs {
    fn stat(&self, path: &str) -> Option<Stat> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(Stat {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
        })
    }
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let mut f = File::open(path).ok()?;
        f.seek(SeekFrom::Start(0)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}

/// In-memory filesystem for tests and the demo. Models the cases that matter:
/// growth ([`append`](MemFs::append), same inode), rotation ([`rotate`](MemFs::rotate),
/// new inode) and truncation ([`put`](MemFs::put) with shorter content, same inode).
#[derive(Default)]
pub struct MemFs {
    files: BTreeMap<String, (u64, Vec<u8>)>, // path -> (inode, bytes); dev is constant
    next_ino: u64,
}

impl MemFs {
    pub fn new() -> Self {
        MemFs {
            files: BTreeMap::new(),
            next_ino: 1,
        }
    }
    fn alloc(&mut self) -> u64 {
        let i = self.next_ino;
        self.next_ino += 1;
        i
    }
    /// Set the whole content. Keeps the inode if the file exists (in-place rewrite —
    /// shorter content models a truncation); allocates a new inode if it's new.
    pub fn put(&mut self, path: &str, contents: &str) {
        match self.files.get_mut(path) {
            Some(entry) => entry.1 = contents.as_bytes().to_vec(),
            None => {
                let ino = self.alloc();
                self.files
                    .insert(path.to_string(), (ino, contents.as_bytes().to_vec()));
            }
        }
    }
    /// Append, keeping the inode (a log growing in place).
    pub fn append(&mut self, path: &str, contents: &str) {
        match self.files.get_mut(path) {
            Some(entry) => entry.1.extend_from_slice(contents.as_bytes()),
            None => self.put(path, contents),
        }
    }
    /// Replace the file with a fresh inode (rename-away + recreate, e.g. a game
    /// restart moving Player.log -> Player-prev.log and starting a new Player.log).
    pub fn rotate(&mut self, path: &str, contents: &str) {
        let ino = self.alloc();
        self.files
            .insert(path.to_string(), (ino, contents.as_bytes().to_vec()));
    }
    pub fn remove(&mut self, path: &str) {
        self.files.remove(path);
    }
}

impl Fs for MemFs {
    fn stat(&self, path: &str) -> Option<Stat> {
        self.files.get(path).map(|(ino, bytes)| Stat {
            dev: 1,
            ino: *ino,
            size: bytes.len() as u64,
        })
    }
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.files.get(path).map(|(_, bytes)| bytes.clone())
    }
}
