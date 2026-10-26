# Security

## Reporting a vulnerability

When GitHub private vulnerability reporting is enabled, use this repository's
**Security → Advisories → Report a vulnerability** action to submit a private
report. Include the affected revision, reproduction steps, expected behavior,
impact and a minimal synthetic example.
Do not include credentials, customer content or an actual customer repository.

If the private reporting action is unavailable, do not put vulnerability details
in a public issue. You can open an issue requesting a private reporting channel
from the maintainers without disclosing the vulnerability.

## Support scope

There is no supported-release matrix or backport guarantee yet. Identify the
exact revision or version in every report. Integrity validation, untrusted
repository/content parsing and filesystem safety are within scope. A complete
local repository is readable by its filesystem owner; application permissions
are enforced by the embedding product.

