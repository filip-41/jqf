# Numbers

jqf computes exact values. This is the one divergence from jq that can change a
result silently, and it is deliberate.

| Program                | jq 1.8.2              | jqf                |
| ---------------------- | --------------------- | ------------------ |
| `0.1 + 0.2`            | `0.30000000000000004` | `0.3`              |
| `0.1 + 0.2 == 0.3`     | `false`               | `true`             |
| `9007199254740993 + 0` | `9007199254740992`    | `9007199254740993` |

There is no switch that restores jq's rounding. A consumer that needs jq's exact
bytes pins the rendering itself (`tostring`, `@json`, explicit rounding).

## The tri-state model

A number is one of three kinds, and stays the narrowest kind that holds it:

| Kind     | What it is                                           |
| -------- | ---------------------------------------------------- |
| integer  | arbitrary-precision — a literal past 2⁵³ stays exact |
| decimal  | exact base-ten `coefficient × 10⁻ˢᶜᵃˡᵉ`              |
| binary64 | IEEE 754 double, keeps `inf` and `nan`               |

A decoded literal keeps its **spelling** e.g. `1.50` is retained as
`(150, scale 2)` and prints back as `1.50` or `-0` stays `-0`. Value equality is
mathematical (`1 == 1.0`), spelling is presentation true to input.

```console
$ echo '{"x":1.50}' | jqf -c .x
1.50
```

## Arithmetic laws

- `+`, `-`, `*` on exact kinds are exact, ring-closed, no range surprises until
  the (astronomical) digit ceiling, which is a typed error.
- `/` takes the narrowest sound kind, so an integral quotient is an integer, a
  terminating decimal is a decimal, anything else is the nearest binary64 e.g.
  `1 / 3` is `0.3333333333333333`.
- `%` truncates both operands to integers first, the sign follows the dividend,
  a zero divisor is its own error class.
- Any binary64 operand makes the whole operation binary64
- Non-finite values are values and so that `nan` renders as `null`, infinities
  clamp to the widest finite binary64 on output.

`0 * -1` is `0`, not `-0` (although the *decoded* `-0` keeps its sign). `NaN`
sorts below every number, and `sort` is a real total order (`nan == nan` is
still false in a program, but sorting never depends on the platform)

## Rendering

Exact kinds render from their digits which means retained spelling when the
decode kept one and canonical positional/scientific form when constructed.
Binary64 renders shortest-round-trip with lowercase `e`: `1e-4` prints `0.0001`,
`1e16` prints `1e+16`, and a float computed as `0.30000000000000004` keeps those
digits.

Two riders answer capability probes: `have_decnum` and `have_literal_numbers`
are both `true`.

## Strictness at the boundary

Decode is RFC 8259-strict: `01`, `+1`, `.5`, `1.` refuse (exit 5) unless
`--strictness lenient` opts jq's grammar back in. `fromjson` is already
jq-lenient. See [JSON](json.md).

Structural nesting is capped at 10 000 levels — program, input, and output,
including constructed values. Linear iteration is not nesting.
