//! Cheap per-candidate decision from the `lstat` data collected by the walk (no syscalls).

use crate::config::{self, Backend, SizeRange};
use crate::sys::{PROTECTED_FLAGS, SF_DATALESS, UF_COMPRESSED};
use crate::walk::{Candidate, FileKey};
use std::collections::HashSet;

const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;

/// Largest file ditto compresses. Measured on current macOS: `ditto --hfsCompression` leaves
/// files larger than this uncompressed, and ditto has no option to change it; a ditto range
/// of the routing may not go above it (applesauce ranges may).
pub const MAX_SIZE: u64 = 512 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    AlreadyCompressed,
    /// uchg/uappnd/schg/sappnd/restricted set (the protected bits of the flags); never touched.
    Flagged(u32),
    /// Cloud placeholder (`SF_DATALESS`); reading it would download it.
    Dataless,
    /// Below the lowest `min_size` of the routing.
    TooSmall,
    /// Outside every backend size range (above them all or in a gap between them).
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
    /// To compress with this backend (the one whose size range holds the file).
    Compress(Backend),
}

pub struct Rules<'a> {
    pub euid: u32,
    /// Which backend compresses which sizes ([`config::Config::routing`]).
    pub routing: &'a [SizeRange],
    /// Files with `mtime_sec` greater than this are too new.
    pub mtime_cutoff: i64,
    pub incompressible: &'a HashSet<FileKey>,
}

/// The `TooSmall` test of `Rules::classify`, with the flag checks that come before it: the
/// walk uses it to count such files without keeping them.
pub fn too_small(flags: u32, size: u64, min_size: u64) -> bool {
    flags & (UF_COMPRESSED | PROTECTED_FLAGS | SF_DATALESS) == 0 && size < min_size
}

impl Rules<'_> {
    pub fn classify(&self, c: &Candidate) -> Class {
        if c.flags & UF_COMPRESSED != 0 {
            Class::AlreadyCompressed
        } else if c.flags & PROTECTED_FLAGS != 0 {
            Class::Flagged(c.flags & PROTECTED_FLAGS)
        } else if c.flags & SF_DATALESS != 0 {
            Class::Dataless
        } else if too_small(c.flags, c.size(), config::lowest_min_size(self.routing)) {
            Class::TooSmall
        } else if let Some(backend) = config::backend_for(self.routing, c.size()) {
            self.classify_routed(c, backend)
        } else {
            Class::TooLarge
        }
    }

    /// The checks after the size: `backend` holds the file's size.
    fn classify_routed(&self, c: &Candidate, backend: Backend) -> Class {
        let skip = |kind, reason| Class::Skip { kind, reason };
        if c.nlink > 1 {
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
            Class::Compress(backend)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Backend, SizeRange};
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

    /// One ditto range `[min, MAX_SIZE]` (the old single-backend form).
    fn ditto_only(min: u64) -> Vec<SizeRange> {
        vec![SizeRange {
            backend: Backend::Ditto,
            min_size: min,
            max_size: Some(MAX_SIZE),
        }]
    }

    #[test]
    fn routing_at_the_boundaries() {
        let cache = HashSet::new();
        let class = |routing: &[SizeRange], size: u64| {
            let mut c = cand();
            c.key.size = size;
            Rules {
                euid: 501,
                routing,
                mtime_cutoff: 2000,
                incompressible: &cache,
            }
            .classify(&c)
        };
        let routing = config::default_routing();
        let min = config::DEFAULT_MIN_SIZE;
        assert_eq!(class(&routing, min - 1), Class::TooSmall);
        assert_eq!(class(&routing, min), Class::Compress(Backend::Ditto));
        assert_eq!(class(&routing, MAX_SIZE), Class::Compress(Backend::Ditto));
        assert_eq!(
            class(&routing, MAX_SIZE + 1),
            Class::Compress(Backend::Applesauce)
        );
        assert_eq!(
            class(&routing, u64::MAX),
            Class::Compress(Backend::Applesauce)
        );
        // A gap between ranges and the space above a bounded top range are TooLarge; only
        // below the lowest min_size is TooSmall.
        let gapped = [
            SizeRange {
                backend: Backend::Applesauce,
                min_size: 10,
                max_size: Some(20),
            },
            SizeRange {
                backend: Backend::Ditto,
                min_size: 30,
                max_size: Some(40),
            },
        ];
        assert_eq!(class(&gapped, 9), Class::TooSmall);
        assert_eq!(class(&gapped, 20), Class::Compress(Backend::Applesauce));
        assert_eq!(class(&gapped, 25), Class::TooLarge);
        assert_eq!(class(&gapped, 30), Class::Compress(Backend::Ditto));
        assert_eq!(class(&gapped, 41), Class::TooLarge);
    }

    #[test]
    fn classification_order() {
        let mut cache = HashSet::new();
        let rules = |cache: &HashSet<FileKey>| -> Vec<Class> {
            let routing = ditto_only(1);
            let r = Rules {
                euid: 501,
                routing: &routing,
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
            c.key.size = MAX_SIZE;
            out.push(r.classify(&c));
            let mut c = cand();
            c.key.size = MAX_SIZE + 1;
            out.push(r.classify(&c));
            c.flags = UF_IMMUTABLE;
            out.push(r.classify(&c));
            c.flags = SF_DATALESS;
            out.push(r.classify(&c));
            out
        };
        let got = rules(&cache);
        assert_eq!(got[0], Class::Compress(Backend::Ditto));
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
        assert_eq!(got[10], Class::Compress(Backend::Ditto));
        assert_eq!(got[11], Class::TooLarge);
        assert_eq!(got[12], Class::Flagged(UF_IMMUTABLE));
        assert_eq!(got[13], Class::Dataless);
        cache.insert(cand().key);
        let mut big = cand().key;
        big.size = MAX_SIZE + 1;
        cache.insert(big);
        let got = rules(&cache);
        assert_eq!(got[0], Class::Incompressible);
        assert_eq!(got[11], Class::TooLarge);
    }

    #[test]
    fn too_small_matches_classify() {
        let cache = HashSet::new();
        let min = 100;
        let routing = ditto_only(min);
        let r = Rules {
            euid: 501,
            routing: &routing,
            mtime_cutoff: 2000,
            incompressible: &cache,
        };
        let flags = [
            0,
            UF_COMPRESSED,
            UF_IMMUTABLE,
            SF_RESTRICTED,
            SF_DATALESS,
            UF_COMPRESSED | SF_DATALESS,
        ];
        let sizes = [0, min - 1, min, min + 1, MAX_SIZE + 1];
        for f in flags {
            for size in sizes {
                for nlink in [1, 2] {
                    for uid in [501, 0] {
                        for mtime in [1000, 3000] {
                            let mut c = cand();
                            c.flags = f;
                            c.key.size = size;
                            c.nlink = nlink;
                            c.uid = uid;
                            c.key.mtime_sec = mtime;
                            assert_eq!(
                                too_small(c.flags, c.size(), min),
                                r.classify(&c) == Class::TooSmall,
                                "{c:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
