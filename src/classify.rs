//! Cheap per-candidate decision from the `lstat` data collected by the walk (no syscalls).

use crate::sys::{PROTECTED_FLAGS, SF_DATALESS, UF_COMPRESSED};
use crate::walk::{Candidate, FileKey};
use std::collections::HashSet;

const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;

/// Measured on current macOS: `ditto --hfsCompression` leaves files larger than this
/// uncompressed, and ditto has no option to change it.
pub const DITTO_MAX_SIZE: u64 = 512 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    AlreadyCompressed,
    /// uchg/uappnd/schg/sappnd/restricted set (the protected bits of the flags); never touched.
    Flagged(u32),
    /// Cloud placeholder (`SF_DATALESS`); reading it would download it.
    Dataless,
    TooSmall,
    /// Larger than `DITTO_MAX_SIZE`: ditto would only stage a full uncompressed copy.
    TooLarge,
    /// Logged at INFO with the reason; `kind` names the reason in progress lines.
    Skip {
        kind: &'static str,
        reason: &'static str,
    },
    /// Modified less than `min_age_secs` ago.
    TooNew,
    /// Known incompressible version (cache hit).
    Incompressible,
    Compress,
}

pub struct Rules<'a> {
    pub euid: u32,
    pub min_size: u64,
    /// Files with `mtime_sec` greater than this are too new.
    pub mtime_cutoff: i64,
    pub incompressible: &'a HashSet<FileKey>,
}

impl Rules<'_> {
    pub fn classify(&self, c: &Candidate) -> Class {
        let skip = |kind, reason| Class::Skip { kind, reason };
        if c.flags & UF_COMPRESSED != 0 {
            Class::AlreadyCompressed
        } else if c.flags & PROTECTED_FLAGS != 0 {
            Class::Flagged(c.flags & PROTECTED_FLAGS)
        } else if c.flags & SF_DATALESS != 0 {
            Class::Dataless
        } else if c.size() < self.min_size {
            Class::TooSmall
        } else if c.size() > DITTO_MAX_SIZE {
            Class::TooLarge
        } else if c.nlink > 1 {
            skip("hardlink", "hardlinked, skipped")
        } else if c.uid != self.euid {
            skip("owner", "owned by another user, skipped")
        } else if c.mode & (S_ISUID | S_ISGID) != 0 {
            skip("setuid", "setuid/setgid, skipped")
        } else if c.key.mtime_sec > self.mtime_cutoff {
            Class::TooNew
        } else if self.incompressible.contains(&c.key) {
            Class::Incompressible
        } else {
            Class::Compress
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{SF_RESTRICTED, UF_APPEND, UF_IMMUTABLE};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn cand() -> Candidate {
        Candidate {
            path: PathBuf::from("/r/f"),
            match_root: Arc::from(Path::new("/r")),
            key: FileKey {
                dev: 1,
                ino: 2,
                size: 100,
                mtime_sec: 1000,
                mtime_nsec: 5,
            },
            atime_sec: 0,
            atime_nsec: 0,
            blocks: 8,
            mode: 0o100644,
            nlink: 1,
            uid: 501,
            gid: 20,
            flags: 0,
        }
    }

    #[test]
    fn classification_order() {
        let mut cache = HashSet::new();
        let rules = |cache: &HashSet<FileKey>| -> Vec<Class> {
            let r = Rules {
                euid: 501,
                min_size: 1,
                mtime_cutoff: 2000,
                incompressible: cache,
            };
            let mut out = vec![r.classify(&cand())];
            let mut c = cand();
            c.flags = UF_COMPRESSED | UF_IMMUTABLE;
            out.push(r.classify(&c));
            let mut c = cand();
            c.key.size = 0;
            out.push(r.classify(&c));
            let mut c = cand();
            c.nlink = 2;
            out.push(r.classify(&c));
            let mut c = cand();
            c.uid = 0;
            out.push(r.classify(&c));
            let mut c = cand();
            c.flags = SF_RESTRICTED;
            out.push(r.classify(&c));
            let mut c = cand();
            c.mode = 0o104755;
            out.push(r.classify(&c));
            let mut c = cand();
            c.key.mtime_sec = 2001;
            out.push(r.classify(&c));
            // Flags win over every other skip; dataless before size and the rest.
            let mut c = cand();
            c.flags = UF_IMMUTABLE | UF_APPEND | SF_DATALESS;
            c.key.size = 0;
            c.nlink = 2;
            out.push(r.classify(&c));
            let mut c = cand();
            c.flags = SF_DATALESS;
            c.key.size = 0;
            out.push(r.classify(&c));
            // The ditto size limit: inclusive bound, checked before the cache; flags and
            // dataless still win over it.
            let mut c = cand();
            c.key.size = DITTO_MAX_SIZE;
            out.push(r.classify(&c));
            let mut c = cand();
            c.key.size = DITTO_MAX_SIZE + 1;
            out.push(r.classify(&c));
            c.flags = UF_IMMUTABLE;
            out.push(r.classify(&c));
            c.flags = SF_DATALESS;
            out.push(r.classify(&c));
            out
        };
        let got = rules(&cache);
        assert_eq!(got[0], Class::Compress);
        assert_eq!(got[1], Class::AlreadyCompressed);
        assert_eq!(got[2], Class::TooSmall);
        assert!(matches!(
            got[3],
            Class::Skip {
                kind: "hardlink",
                ..
            }
        ));
        assert!(matches!(got[4], Class::Skip { kind: "owner", .. }));
        assert_eq!(got[5], Class::Flagged(SF_RESTRICTED));
        assert!(matches!(got[6], Class::Skip { kind: "setuid", .. }));
        assert_eq!(got[7], Class::TooNew);
        assert_eq!(got[8], Class::Flagged(UF_IMMUTABLE | UF_APPEND));
        assert_eq!(got[9], Class::Dataless);
        assert_eq!(got[10], Class::Compress);
        assert_eq!(got[11], Class::TooLarge);
        assert_eq!(got[12], Class::Flagged(UF_IMMUTABLE));
        assert_eq!(got[13], Class::Dataless);
        cache.insert(cand().key);
        let mut big = cand().key;
        big.size = DITTO_MAX_SIZE + 1;
        cache.insert(big);
        let got = rules(&cache);
        assert_eq!(got[0], Class::Incompressible);
        assert_eq!(got[11], Class::TooLarge);
    }
}
