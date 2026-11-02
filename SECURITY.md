# Security policy

Medha can execute commands, modify files, connect to model providers, and call
external tools. Reports about failures in its authorization, isolation, or data
protection boundaries are especially valuable.

## Supported versions

Medha is currently pre-1.0. Include the affected version or commit in your
report. When possible, also indicate whether the latest published release or
current `main` is affected.

| Version | Reporting guidance |
| --- | --- |
| Latest published release | Preferred released version for reproducing an issue |
| `main` | Include the commit SHA; this is unreleased development code |
| Older releases | Reports are welcome; include newer affected versions if known |

See [GitHub Releases](https://github.com/kashyaprajharsh/medha-agent/releases)
for current versions. If you find a vulnerability in an older release, please
report it even if you cannot check whether it affects the latest version.
The affected versions, availability of a fix, and any backports will be
communicated in the relevant release notes or security advisory. This policy
does not guarantee a fix or backport for every affected release.

## Report a vulnerability privately

Email [reeturaj.harsh@manojavamlabs.com](mailto:reeturaj.harsh@manojavamlabs.com) with the subject
`[Medha security] <short summary>`.

If GitHub private vulnerability reporting is enabled for the repository, you
may alternatively use
[Report a vulnerability](https://github.com/kashyaprajharsh/medha-agent/security/advisories/new).
An unavailable form does not prevent reporting by email.

Please keep vulnerability details, exploit code, and sensitive logs out of public
issues, discussions, and pull requests until coordinated disclosure. Ordinary
bugs and setup questions can use the
[issue forms](https://github.com/kashyaprajharsh/medha-agent/issues/new/choose)
and [Discussions](https://github.com/kashyaprajharsh/medha-agent/discussions).

### What to include

Provide as much of the following as you can. A useful initial report does not
need a complete exploit or a proposed fix.

- A short description of the vulnerability and its security impact.
- Affected release or commit, operating system, and installation method.
- The affected interface: desktop, TUI, headless CLI, editor integration, backend,
  messaging gateway, plugin, or MCP integration.
- Required configuration, sandbox backend, permission grants, and attacker
  access. Explain what the user approved, if anything.
- Minimal reproduction steps or a proof of concept using synthetic data.
- Expected protection and the actual observed effect, with a short, redacted
  log excerpt or screenshot where useful.
- A mitigation or suggested fix, if known, and how you would like to be credited.

Do not send API keys, OAuth tokens, authorization headers, signing keys,
credential files, or an entire Medha home directory. Session transcripts and
artifacts may also contain private project content. Share a minimal, redacted
example first and arrange any further evidence privately.

## What happens after a report

Maintainers will assess the report, request clarification when needed, and work
with the reporter to establish the affected versions, impact, and remediation.
When a fix or mitigation is ready, maintainers will coordinate disclosure and
credit with the reporter, respecting requests for anonymity.

Response and remediation times depend on severity, reproducibility, and
maintainer availability; this project does not promise a fixed response time.
If you have not received a response, follow up in the same private thread.
Please discuss publication timing there so users can receive a fix or mitigation.

## Security boundaries and report scope

Examples of issues to report privately include:

- Bypassing a required approval or executing a tool intent that policy denied.
- Reading or writing protected files, escaping configured filesystem or network
  restrictions, or broadening a child agent's permissions beyond its parent.
- Exposing credentials or sending them to an unintended provider or endpoint.
- Unauthorized access to the local backend or another session's data.
- Reaching the agent through a messaging bot without pairing, or answering an
  approval as someone other than the paired person.
- Forging trusted provenance, accepted event history, or permission grants.
- Bypassing update integrity checks, or escaping desktop artifact and preview
  restrictions to access local resources without authorization.

Describe the boundary actually crossed. A harmful model response, a prompt
injection attempt, or an authorized command is not by itself evidence of a
policy bypass; evidence that it caused an unauthorized effect is relevant.
Report suspected boundary failures even if you are unsure of the root cause.

Isolation depends on the operating system and selected backend. Native execution
uses Seatbelt on macOS and Landlock on Linux when available. If native confinement
is unavailable, Linux refuses command execution; macOS can fall back to host
execution. Windows currently relies on command policy and approvals without an
OS sandbox. An explicitly selected host backend also runs without OS isolation.
Remote MCP services execute outside the local command sandbox. See the
[README](README.md#what-you-get) and
[technical overview](docs/WHAT_IS_MEDHA.md) for the documented behavior and
limitations.

Test only against systems and data you own or have permission to assess. Use
minimal proofs of concept, avoid disrupting other users or services, and stop
testing if it exposes someone else's private information. If the issue belongs
to a third-party provider or service, report it through that provider's security
channel; include Medha's role when it is relevant to the integration.
