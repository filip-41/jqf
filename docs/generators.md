# Engine constructors: the `~` surface

jq generators are ephemeral — a stream exists only while it is being consumed.
The `~` marker opens the engine surface: **cursors**, suspended generators a
program can hold, pull one value at a time, and interleave.
`jqf --help generators` is the live page.

```console
$ echo '[[1,2,3],["a","b"]]' | jqf -c '~cursor(.[0][]) as ~a | ~cursor(.[1][]) as ~b | [limit(2; repeat([~a.next, ~b.next]))]'
[[1,"a"],[2,"b"]]
```

That zip is the motivating shape: two streams advancing in lockstep, each
keeping its own state between pulls. That's something plain jq generators cannot
spell.

## The constructors

`~` is a separate namespace, so the constructors can never collide with value
builtins.

| Spelling                            | Meaning                                                                                                                   |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `~cursor(f)`                        | wrap any filter `f` as a cursor over its output stream                                                                    |
| `~generator(init; update; extract)` | a phase-driven state machine: start at `init`, step with `update` until it emits `empty`, publish `extract` of each state |
| `~inputs`                           | the input sequence as a cursor (requires `-n`, like `inputs`)                                                             |
| `~rng(seed)`                        | a seeded xoshiro256\*\* stream — infinite, pure, `~rng(S).next == rand(S)`                                                |

```console
$ jqf -nc '~generator(0; if . < 3 then .+1 else empty end; .) as ~x | [~x.rest]'
[1,2,3]

$ jqf -nc '~rng(7) as ~r | [limit(3; ~r.rest)]'
[0.7005764821796896,0.2787512294737843,0.8396274618764198]
```

## The protocol

> **Experimental.** Engine bindings are a new jqf concept. The two-pull
> vocabulary below is the law for the four constructors that exist today, a
> future engine constructor may arrive with pulls of its own.

A constructor result is not a value and it must bind to an engine binding:

```jq
~CONSTRUCTOR(...) as ~x | BODY
```

The binding is lexically scoped like `$x` and released when the body ends.
Inside the body there are exactly two pulls:

| Pull      | Meaning                                         |
| --------- | ----------------------------------------------- |
| `~x.next` | one value; `empty` when the cursor is exhausted |
| `~x.rest` | every remaining value, as a stream              |

The pulls belong to the binding, not the constructor: all four constructors
bind the same kind of cursor, so there is no per-constructor pull vocabulary.
Any other projection is a compile error naming the two that exist.

`.rest` is lazy, not collected: on a cursor that never exhausts (`~rng`) it is
an infinite stream, and `limit`, `first`, or any consumer that stops pulling
bounds it. Collecting it whole (`[~r.rest]`) never completes.

What differs per constructor is how the stream is *produced*. `~generator`'s
three filters each emit **exactly one** value per pull, a second output raises
a catchable cardinality error. `~cursor` has no such law — a multi-output
wrapped filter yields multiple values per pull.

## Refusals

The surface is closed and each edge is a compile-time message, not a quirk:

- an unknown constructor name — the error lists the four that exist
- a constructor in value position (it must bind with `as ~x`)
- returning `~x` itself; only `.next` and `.rest` project it
- a projection other than `.next` / `.rest`
- a pull inside a recursive `def`, and a pull of an outer cursor from inside
  another `~generator`'s filters (no cross-machine capture)
- `~x` as a `reduce`/`foreach` pattern

Programs that use the engine surface run serially — cursor state is a
per-request machine, not a [morsel](parallelism.md).
