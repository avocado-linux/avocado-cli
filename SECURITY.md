# Security policy

## Reporting a vulnerability

Please do **not** report security vulnerabilities through public issues, pull requests, or Discord.

Report them privately through GitHub: on the [Security tab](https://github.com/avocado-linux/avocado-cli/security) of this repository, select **Report a vulnerability**. If you cannot use GitHub, email [info@peridio.com](mailto:info@peridio.com).

Include as much of this as you can:

- The CLI version (`avocado --version`) and your host OS.
- The steps or configuration needed to reproduce the issue.
- What an attacker could do with it.

This policy covers the Avocado CLI and every open source repository in the Avocado OS ecosystem, including the repositories in the [`avocado-linux`](https://github.com/avocado-linux) organization. To report an issue in any of them, use the Security tab of this repository or email [info@peridio.com](mailto:info@peridio.com). Tell us which repository is affected. For Avocado Connect or Avocado Desktop, use the same email.

## What to expect

- We acknowledge your report within 3 business days.
- We send you a status update at least every 10 business days while we investigate.
- If we confirm the issue, we fix it, release the fix, and publish a GitHub Security Advisory. With your permission, the advisory credits you.
- If we decline the report, we tell you why.

Published advisories are on the [Security Advisories](https://github.com/avocado-linux/avocado-cli/security/advisories) page.

## Supported versions

We fix security issues in the [latest release](https://github.com/avocado-linux/avocado-cli/releases/latest). Run `avocado upgrade` before you report an issue.

## Avocado OS security documentation

For the security features of Avocado OS itself, see:

- [Security features](https://docs.peridio.com/developer-reference/security): encrypted `/var`, dm-verity, and signed boot, enabled from `avocado.yaml`.
- [Secure Boot](https://docs.peridio.com/avocado-os/security/secure-boot)
- [Production-ready features](https://docs.peridio.com/avocado-os/security/production-ready)
- [Cyber Resilience Act](https://docs.peridio.com/avocado-os/cra)
