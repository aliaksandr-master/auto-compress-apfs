# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub Security Advisories, not in public
issues: <https://github.com/aliaksandr-master/auto-compress-apfs/security/advisories/new>.
Include the version (`auto-compress-apfs --version`), the macOS version and the steps or
config that reproduce it.

## Scope

- Data loss or corruption: a file replaced by something that is not a byte-identical compressed
  copy, a file deleted or left in a state other programs cannot read.
- Guard bypass: a walk or compression outside the documented hard protections (other volumes,
  network or cloud storage, `/Volumes`, system paths, privacy-protected folders without an
  explicit root, flagged or dataless items).
- Executing anything other than `/usr/bin/ditto`.

Only the latest released version is supported.
