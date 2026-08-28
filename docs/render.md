# Render

`render` is the output-only codec for humans that outputs tables, trees, shell
assignments, and histograms. It is a real output format: `-o render` plus an
output dialect, so any program's output can land as a table without a formatting
pass in the program itself.

```console
$ printf '[{"name":"alice","age":30},{"name":"bob","age":40}]' | jqf -o render --output-dialect render.gfm-table@1 .
| name | age |
| :--- | ---: |
| alice | 30 |
| bob | 40 |
```

## Dialects

| Dialect               | Input shape                                   | Output                                              |
| --------------------- | --------------------------------------------- | --------------------------------------------------- |
| `render.plain@1`      | scalars only                                  | the bare text, containers refuse                    |
| `render.gfm-table@1`  | object, or array of objects                   | GitHub-flavored Markdown table (header required)    |
| `render.html-table@1` | object, or array of objects                   | a `<table>` fragment                                |
| `render.grid-table@1` | object, or array of objects                   | ASCII grid, wrappable                               |
| `render.tree@1`       | any value                                     | an indented path tree                               |
| `render.terminal@1`   | any value                                     | control-safe text, shape picked by `--render-shape` |
| `render.shell@1`      | flat leaves                                   | `key=value` assignments, shell-quoted               |
| `render.hist@1`       | array of numbers, or `{value, count}` objects | a ten-bin ASCII histogram                           |

**Tables** extract columns as the union of member keys in first-appearance
order: a missing key renders `null` and a nested container cell renders as
compact JSON. The layout is sampled (first 256 rows / 1 MiB by default), and an
input past the sample cap is a typed refusal.

**Tree** names every path:

```console
$ echo '{"name":"alice","tags":["a","b"]}' | jqf -o render --output-dialect render.tree@1 .
$ = object(2)
  $["name"]#0 = "alice"
  $["tags"]#1 = array(2)
    $["tags"]#1[0] = "a"
    $["tags"]#1[1] = "b"
```

Shared containers are labelled `&0` / `*0`, so an aliased YAML node is visible
as sharing.

**Shell** flattens leaves to assignments and refuses a collision:

```console
$ echo '{"a":1,"b":"x"}' | jqf -o render --output-dialect render.shell@1 .
a=1
b='x'
```

**Histogram**:

```console
$ echo '[1,2,2,3,3,3,8]' | jqf -o render --output-dialect render.hist@1 . | head -3
[1, 1.7)                                | 1 | ##############
[1.7, 2.4)                              | 2 | ###########################
[2.4, 3.0999999999999996)               | 3 | ########################################
```

## Render dials

Render includes 4 dials, everything else is the dialect's law.

| Flag                 | Values                     | Default                                                 |
| -------------------- | -------------------------- | ------------------------------------------------------- |
| `--render-header`    | `present` / `absent`       | `present` — GFM requires it                             |
| `--render-width`     | `western` / `cjk`          | `western` — display width of ambiguous-width characters |
| `--render-shape`     | `plain` / `table` / `tree` | `tree` — `render.terminal@1` only                       |
| `--render-max-width` | `N`                        | `0` — sampled per-column wrap width; 0 disables         |

```console
$ printf '[{"name":"alice","age":30}]' | jqf -o render --output-dialect render.grid-table@1 .
+-------+-----+
| name  | age |
+-------+-----+
| alice |  30 |
+-------+-----+
```

Color (`-C` / `-M` / `JQ_COLORS`) is a JSON-family concern and does not apply to
render frames. Zero published items render zero frames.
