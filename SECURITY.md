# Security Policy

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository (Security →
Report a vulnerability). Include a minimal reproducing input and program, the
jqf version (`jqf -V`), and the platform. Do not open a public issue for
anything you believe is exploitable.

Please allow up to 90 days of coordinated disclosure before publishing details.

## Supported versions

The current release line and `main` receive fixes.

## Scope notes

jqf is routinely run against untrusted *data* (documents on stdin or file
arguments); memory-safety, resource-exhaustion, or crash findings on
untrusted data are in scope. Untrusted *programs* are a weaker boundary: jq
programs can already read any file passed to the process and exhaust CPU by
design, so program-driven findings are in scope only when they break a
documented limit (recursion ceilings, RSS governor, output ceilings).

jqf's own temp-file handling is in scope as well. The external sort's spill
store (`--max-spill-bytes N`) creates its directory with mkdtemp discipline
and unlinks every run file at creation. Findings on that surface are treated
like untrusted-data findings.
