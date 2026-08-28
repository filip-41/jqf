# Serve mode

`jqf serve` is the resident daemon: bind a listener, compile the program once,
and serve NDJSON sessions until a signal. It exists for the case where process
startup is the cost since a hot loop calling jqf thousands of times pays compile and
spawn every time while a serve socket pays them once.

```console
$ jqf serve --listen /tmp/jqf.sock '.a' &
jqf: serve: listening on /tmp/jqf.sock

$ printf '{"a":1}\n{"a":2}\n' | nc -U /tmp/jqf.sock
1
2
```

`PROGRAM` defaults to the identity filter.

## Sessions

A connection is a session: NDJSON in, NDJSON out, the same drive
[`--follow`](streaming.md) uses, fed incrementally.

- Records frame on newline boundaries, a partial record is **held** until its
  terminator arrives, the held tail is finalized when the client closes (the
  recovering dialect's law).
- Per-value errors are reported on the daemon's stderr and **never kill the
  session or the daemon**. An error-severity record issue additionally publishes
  one machine-readable frame on the connection itself, so a client is not left
  guessing:

```json
{"jqf:error":{"kind":"record-issue","code":"…","message":"…","record":3,"offset":41}}
{"jqf:error":{"kind":"value-error","message":"…","line":7}}
```

- Diagnostics never mix into the output stream — stdout of the session is
  program output and `jqf:error` frames only.

Sessions are served concurrently (capped), against one shared compiled program;
each session gets its own resource accounting.

## Trust model

| Listener                   | Trust                                                                                    |
| -------------------------- | ---------------------------------------------------------------------------------------- |
| `--listen /path/sock.sock` | unix socket — the file mode **is** the ACL; filesystem-authenticated                     |
| `--listen host:port`       | TCP — trusted-network-only **by design**: no auth, no TLS; do not bind a hostile network |
| `--listen :port`           | an empty host binds loopback (127.0.0.1)                                                 |

A stale unix socket from a dead daemon is detected (lock file) and replaced.

## Timeouts and the governor

`--read-timeout SECONDS` (default **60**, `0` disables) arms a per-connection
read/idle timeout: a connection that sends nothing for that long ends its
session cleanly and the daemon accepts the next one.

`--max-rss` works exactly as on an ordinary request, and the governor watches
the **whole daemon** — all sessions together, not each alone. Memory per session
is bounded the same way `--follow` bounds it: completed records are dropped as
they publish (the soak gate pushes 60 000 records through one connection under
an 8 MiB ceiling).

The daemon runs until SIGINT/SIGTERM. A client disconnect is a session end,
never a daemon death (SIGPIPE is ignored).
