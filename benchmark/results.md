# jqf benchmark

These numbers are a local snapshot for guidance, not a published result.

- jqf: pgo · `fb1212d508bd90c59dcccb72f23a5fdc75e737a4`
- time: 2026-08-30T16:43:52Z
- diagnostics: `jqf: build=pgo profile=e0e45a21.cdb907a9.aarch64-apple-darwin.9b60b845 allocator=mimalloc platform=aarch64-macos pcores=6 ecores=12 pcore_source=detected`
- jq: 1.8.2
- jaq: 3.1.1
- gojq: 0.12.19
- yq: 4.53.6
- dasel: 3.11.2
- mlr: 6.21.0
- warmup 1, runs 3, median wall; RSS from that run

## host

- os: macOS-26.5.1-arm64-arm-64bit-Mach-O
- arch: arm64
- python: 3.14.6
- cpus: 18
- cpu: Apple M5 Max
- memory: 128.0 GiB
- physical_cpus: 18

## geomean vs jqf

| tool | wall | rss | n |
| --- | --- | --- | --- |
| jqf-serial | 1.08× (median 1.00×) | 0.97× (median 1.00×) | 678 |
| jq | 2.66× (median 2.41×) | 1.35× (median 1.25×) | 406 |
| jaq | 1.51× (median 1.34×) | 1.60× (median 1.11×) | 622 |
| gojq | 2.17× (median 2.18×) | 2.10× (median 1.97×) | 492 |
| yq | 4.70× (median 3.93×) | 9.67× (median 7.65×) | 363 |
| dasel | 2.57× (median 2.90×) | 3.50× (median 2.91×) | 96 |
| mlr | 1.29× (median 1.72×) | 6.76× (median 7.40×) | 56 |

document = json/yaml/toml. streaming = ndjson/csv records.

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 1.00×) | 1.00× (median 1.00×) | 552 | 1.60× (median 1.06×) | 0.83× (median 1.00×) | 126 |
| jq | 2.43× (median 2.24×) | 2.14× (median 1.98×) | 336 | 4.14× (median 3.93×) | 0.15× (median 0.22×) | 70 |
| jaq | 1.40× (median 1.32×) | 1.81× (median 1.43×) | 552 | 2.64× (median 2.56×) | 0.63× (median 0.60×) | 70 |
| gojq | 2.03× (median 2.12×) | 2.40× (median 2.31×) | 433 | 3.59× (median 3.68×) | 0.80× (median 1.18×) | 59 |
| yq | 4.49× (median 3.76×) | 9.24× (median 7.47×) | 333 | 7.78× (median 7.02×) | 15.95× (median 15.26×) | 30 |
| dasel | 2.57× (median 2.90×) | 3.50× (median 2.91×) | 96 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.29× (median 1.72×) | 6.76× (median 7.40×) | 56 |

## geomean vs jqf · 100

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.00× (median 1.01×) | 1.00× (median 1.00×) | 84 | 1.00× (median 1.02×) | 1.01× (median 1.01×) | 18 |
| jq | 1.06× (median 1.06×) | 0.63× (median 0.58×) | 48 | 1.09× (median 1.08×) | 0.56× (median 0.56×) | 10 |
| jaq | 1.02× (median 1.02×) | 0.89× (median 0.87×) | 84 | 1.00× (median 1.02×) | 0.84× (median 0.83×) | 10 |
| gojq | 1.10× (median 1.09×) | 1.36× (median 1.31×) | 66 | 1.06× (median 1.08×) | 1.37× (median 1.38×) | 10 |
| yq | 2.63× (median 2.19×) | 5.95× (median 6.20×) | 65 | 2.21× (median 2.31×) | 4.80× (median 4.80×) | 6 |
| dasel | 1.36× (median 1.43×) | 2.37× (median 2.36×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 2.00× (median 2.11×) | 6.74× (median 6.70×) | 8 |

## geomean vs jqf · 1k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 1.00×) | 1.00× (median 1.00×) | 84 | 1.11× (median 1.06×) | 0.85× (median 1.00×) | 18 |
| jq | 1.50× (median 1.37×) | 1.05× (median 0.76×) | 48 | 1.68× (median 1.61×) | 0.38× (median 0.41×) | 10 |
| jaq | 1.13× (median 1.12×) | 1.18× (median 0.94×) | 84 | 1.19× (median 1.16×) | 0.63× (median 0.68×) | 10 |
| gojq | 1.43× (median 1.27×) | 1.70× (median 1.44×) | 64 | 1.58× (median 1.50×) | 1.40× (median 1.61×) | 9 |
| yq | 4.45× (median 2.73×) | 7.24× (median 7.24×) | 58 | 5.65× (median 7.37×) | 7.67× (median 8.37×) | 6 |
| dasel | 1.99× (median 1.91×) | 3.16× (median 2.55×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 2.09× (median 2.10×) | 7.12× (median 7.19×) | 8 |

## geomean vs jqf · 5k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.96× (median 1.00×) | 1.00× (median 1.00×) | 84 | 1.21× (median 1.00×) | 0.82× (median 1.00×) | 18 |
| jq | 2.01× (median 1.63×) | 1.73× (median 1.27×) | 48 | 2.61× (median 2.74×) | 0.27× (median 0.34×) | 10 |
| jaq | 1.28× (median 1.28×) | 1.61× (median 1.16×) | 84 | 1.69× (median 1.62×) | 0.61× (median 0.66×) | 10 |
| gojq | 1.90× (median 1.76×) | 2.14× (median 2.05×) | 64 | 2.09× (median 1.55×) | 1.47× (median 2.28×) | 8 |
| yq | 3.80× (median 3.24×) | 9.06× (median 7.83×) | 50 | 12.37× (median 16.19×) | 15.24× (median 18.14×) | 6 |
| dasel | 2.66× (median 2.77×) | 3.61× (median 3.19×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.72× (median 1.72×) | 7.32× (median 7.17×) | 8 |

## geomean vs jqf · 25k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.01× (median 1.00×) | 0.99× (median 1.00×) | 84 | 1.79× (median 1.96×) | 0.79× (median 0.79×) | 18 |
| jq | 3.03× (median 2.78×) | 2.77× (median 3.61×) | 48 | 5.72× (median 6.85×) | 0.14× (median 0.19×) | 10 |
| jaq | 1.57× (median 1.47×) | 2.16× (median 1.94×) | 84 | 3.29× (median 3.50×) | 0.60× (median 0.59×) | 10 |
| gojq | 2.46× (median 2.39×) | 2.68× (median 3.17×) | 64 | 5.13× (median 4.13×) | 0.88× (median 1.49×) | 8 |
| yq | 5.06× (median 4.80×) | 10.24× (median 8.35×) | 46 | 8.47× (median 6.35×) | 19.82× (median 18.23×) | 3 |
| dasel | 3.23× (median 3.49×) | 4.06× (median 4.29×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.22× (median 1.42×) | 6.83× (median 7.92×) | 8 |

## geomean vs jqf · 50k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.01× (median 1.00×) | 1.00× (median 1.00×) | 84 | 2.01× (median 2.48×) | 0.78× (median 0.89×) | 18 |
| jq | 3.41× (median 3.22×) | 3.59× (median 6.05×) | 48 | 7.48× (median 9.28×) | 0.09× (median 0.13×) | 10 |
| jaq | 1.63× (median 1.59×) | 2.43× (median 2.07×) | 84 | 4.14× (median 4.21×) | 0.59× (median 0.52×) | 10 |
| gojq | 2.66× (median 2.68×) | 3.15× (median 3.42×) | 64 | 6.79× (median 5.80×) | 0.62× (median 1.11×) | 8 |
| yq | 5.35× (median 4.99×) | 11.22× (median 9.69×) | 43 | 12.40× (median 8.01×) | 33.37× (median 30.46×) | 3 |
| dasel | 3.43× (median 3.72×) | 4.03× (median 4.21×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.04× (median 1.36×) | 6.78× (median 8.04×) | 8 |

## geomean vs jqf · 100k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.97× (median 0.99×) | 1.00× (median 1.00×) | 84 | 2.27× (median 3.27×) | 0.77× (median 0.93×) | 18 |
| jq | 3.68× (median 3.79×) | 4.09× (median 6.24×) | 48 | 9.45× (median 10.93×) | 0.06× (median 0.09×) | 10 |
| jaq | 1.67× (median 1.63×) | 2.60× (median 2.11×) | 84 | 5.28× (median 5.36×) | 0.59× (median 0.54×) | 10 |
| gojq | 2.74× (median 2.82×) | 3.37× (median 3.74×) | 64 | 8.92× (median 8.60×) | 0.42× (median 0.79×) | 8 |
| yq | 5.61× (median 5.13×) | 11.97× (median 9.55×) | 43 | 15.70× (median 9.18×) | 56.13× (median 52.61×) | 3 |
| dasel | 3.58× (median 3.77×) | 4.14× (median 4.47×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 0.87× (median 1.27×) | 6.55× (median 8.18×) | 8 |

## geomean vs jqf · 200k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 0.99×) | 1.00× (median 1.00×) | 48 | 2.44× (median 4.01×) | 0.77× (median 0.96×) | 18 |
| jq | 4.06× (median 3.98×) | 4.45× (median 6.45×) | 48 | 10.66× (median 12.03×) | 0.04× (median 0.06×) | 10 |
| jaq | 1.95× (median 1.93×) | 3.66× (median 6.47×) | 48 | 6.09× (median 5.98×) | 0.60× (median 0.56×) | 10 |
| gojq | 2.91× (median 2.95×) | 3.72× (median 4.81×) | 47 | 10.81× (median 10.23×) | 0.28× (median 0.60×) | 8 |
| yq | 9.52× (median 9.83×) | 18.62× (median 21.33×) | 28 | 20.47× (median 9.85×) | 90.96× (median 81.93×) | 3 |
| dasel | n/a | n/a | 0 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 0.73× (median 1.07×) | 6.06× (median 7.72×) | 8 |

## results

| case | jqf | jqf-serial | jq | jaq | gojq | yq | dasel | mlr |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| csv-broad-100-count | 9.37 ms / 4.9 MB | 9.23 ms / 4.9 MB | n/a | n/a | n/a | 30.1 ms / 31 MB | n/a | 19.8 ms / 33 MB |
| csv-broad-100-first-id | 9.42 ms / 4.7 MB | 9.49 ms / 4.8 MB | n/a | n/a | n/a | 28.8 ms / 26 MB | n/a | 18.8 ms / 33 MB |
| csv-broad-100-high-count | 11.6 ms / 5.0 MB | 12.9 ms / 5.0 MB | n/a | n/a | n/a | 34.2 ms / 27 MB | n/a | 21.5 ms / 33 MB |
| csv-broad-100-sum-score | 16.0 ms / 4.9 MB | 12.8 ms / 5.0 MB | n/a | n/a | n/a | n/a | n/a | 24.2 ms / 33 MB |
| csv-broad-1k-count | 9.88 ms / 5.5 MB | 10.5 ms / 5.5 MB | n/a | n/a | n/a | 126 ms / 70 MB | n/a | 21.0 ms / 41 MB |
| csv-broad-1k-first-id | 10.1 ms / 5.4 MB | 9.23 ms / 5.4 MB | n/a | n/a | n/a | 122 ms / 63 MB | n/a | 19.7 ms / 41 MB |
| csv-broad-1k-high-count | 9.85 ms / 5.6 MB | 10.1 ms / 5.6 MB | n/a | n/a | n/a | 133 ms / 67 MB | n/a | 21.7 ms / 41 MB |
| csv-broad-1k-sum-score | 10.3 ms / 5.5 MB | 10.9 ms / 5.6 MB | n/a | n/a | n/a | n/a | n/a | 20.7 ms / 41 MB |
| csv-broad-5k-count | 16.9 ms / 8.2 MB | 16.5 ms / 8.2 MB | n/a | n/a | n/a | 515 ms / 229 MB | n/a | 30.2 ms / 65 MB |
| csv-broad-5k-first-id | 9.27 ms / 8.0 MB | 11.4 ms / 8.0 MB | n/a | n/a | n/a | 528 ms / 232 MB | n/a | 23.5 ms / 50 MB |
| csv-broad-5k-high-count | 20.5 ms / 8.2 MB | 21.3 ms / 8.3 MB | n/a | n/a | n/a | 575 ms / 259 MB | n/a | 31.8 ms / 66 MB |
| csv-broad-5k-sum-score | 18.9 ms / 8.2 MB | 17.9 ms / 8.2 MB | n/a | n/a | n/a | n/a | n/a | 31.3 ms / 66 MB |
| csv-broad-25k-count | 37.7 ms / 22 MB | 39.0 ms / 22 MB | n/a | n/a | n/a | excluded | n/a | 72.6 ms / 158 MB |
| csv-broad-25k-first-id | 13.3 ms / 21 MB | 14.0 ms / 21 MB | n/a | n/a | n/a | excluded | n/a | 24.2 ms / 50 MB |
| csv-broad-25k-high-count | 53.2 ms / 22 MB | 51.9 ms / 22 MB | n/a | n/a | n/a | excluded | n/a | 73.7 ms / 175 MB |
| csv-broad-25k-sum-score | 50.5 ms / 22 MB | 50.2 ms / 22 MB | n/a | n/a | n/a | n/a | n/a | 73.0 ms / 166 MB |
| csv-broad-50k-count | 62.8 ms / 38 MB | 61.6 ms / 38 MB | n/a | n/a | n/a | excluded | n/a | 120 ms / 283 MB |
| csv-broad-50k-first-id | 16.0 ms / 38 MB | 16.0 ms / 38 MB | n/a | n/a | n/a | excluded | n/a | 23.8 ms / 50 MB |
| csv-broad-50k-high-count | 92.9 ms / 38 MB | 91.7 ms / 38 MB | n/a | n/a | n/a | excluded | n/a | 123 ms / 326 MB |
| csv-broad-50k-sum-score | 87.9 ms / 38 MB | 88.6 ms / 38 MB | n/a | n/a | n/a | n/a | n/a | 123 ms / 289 MB |
| csv-broad-100k-count | 114 ms / 72 MB | 111 ms / 72 MB | n/a | n/a | n/a | excluded | n/a | 224 ms / 534 MB |
| csv-broad-100k-first-id | 20.6 ms / 71 MB | 20.3 ms / 71 MB | n/a | n/a | n/a | excluded | n/a | 23.9 ms / 49 MB |
| csv-broad-100k-high-count | 166 ms / 72 MB | 171 ms / 72 MB | n/a | n/a | n/a | excluded | n/a | 228 ms / 607 MB |
| csv-broad-100k-sum-score | 163 ms / 72 MB | 163 ms / 72 MB | n/a | n/a | n/a | n/a | n/a | 225 ms / 566 MB |
| csv-broad-200k-count | 215 ms / 139 MB | 212 ms / 139 MB | n/a | n/a | n/a | excluded | n/a | 421 ms / 1053 MB |
| csv-broad-200k-first-id | 30.0 ms / 138 MB | 28.9 ms / 138 MB | n/a | n/a | n/a | excluded | n/a | 24.4 ms / 50 MB |
| csv-broad-200k-high-count | 322 ms / 139 MB | 321 ms / 139 MB | n/a | n/a | n/a | excluded | n/a | 426 ms / 1079 MB |
| csv-broad-200k-sum-score | 308 ms / 139 MB | 315 ms / 139 MB | n/a | n/a | n/a | n/a | n/a | 417 ms / 1062 MB |
| csv-narrow-100-count | 9.59 ms / 4.8 MB | 9.09 ms / 4.8 MB | n/a | n/a | n/a | 14.6 ms / 19 MB | n/a | 20.3 ms / 32 MB |
| csv-narrow-100-first-id | 8.37 ms / 4.6 MB | 9.26 ms / 4.7 MB | n/a | n/a | n/a | 14.1 ms / 20 MB | n/a | 18.4 ms / 32 MB |
| csv-narrow-100-high-count | 9.00 ms / 4.9 MB | 9.10 ms / 4.9 MB | n/a | n/a | n/a | 14.3 ms / 19 MB | n/a | 19.5 ms / 32 MB |
| csv-narrow-100-sum-score | 9.39 ms / 4.8 MB | 10.1 ms / 4.8 MB | n/a | n/a | n/a | n/a | n/a | 20.5 ms / 32 MB |
| csv-narrow-1k-count | 8.79 ms / 4.8 MB | 8.66 ms / 4.8 MB | n/a | n/a | n/a | 20.8 ms / 22 MB | n/a | 19.7 ms / 33 MB |
| csv-narrow-1k-first-id | 7.93 ms / 4.6 MB | 8.78 ms / 4.7 MB | n/a | n/a | n/a | 21.0 ms / 23 MB | n/a | 17.5 ms / 33 MB |
| csv-narrow-1k-high-count | 9.67 ms / 4.9 MB | 9.84 ms / 4.9 MB | n/a | n/a | n/a | 24.1 ms / 25 MB | n/a | 20.1 ms / 33 MB |
| csv-narrow-1k-sum-score | 10.1 ms / 4.8 MB | 12.2 ms / 4.9 MB | n/a | n/a | n/a | n/a | n/a | 19.4 ms / 33 MB |
| csv-narrow-5k-count | 11.6 ms / 4.8 MB | 11.4 ms / 4.9 MB | n/a | n/a | n/a | 47.9 ms / 37 MB | n/a | 20.7 ms / 35 MB |
| csv-narrow-5k-first-id | 10.4 ms / 4.7 MB | 9.09 ms / 4.7 MB | n/a | n/a | n/a | 45.0 ms / 39 MB | n/a | 19.0 ms / 33 MB |
| csv-narrow-5k-high-count | 13.2 ms / 4.9 MB | 13.0 ms / 5.0 MB | n/a | n/a | n/a | 53.9 ms / 39 MB | n/a | 18.8 ms / 35 MB |
| csv-narrow-5k-sum-score | 13.7 ms / 4.9 MB | 12.9 ms / 4.9 MB | n/a | n/a | n/a | n/a | n/a | 19.0 ms / 35 MB |
| csv-narrow-25k-count | 23.3 ms / 5.0 MB | 23.3 ms / 5.1 MB | n/a | n/a | n/a | 148 ms / 90 MB | n/a | 19.7 ms / 42 MB |
| csv-narrow-25k-first-id | 9.51 ms / 4.9 MB | 8.98 ms / 4.9 MB | n/a | n/a | n/a | 152 ms / 89 MB | n/a | 19.3 ms / 33 MB |
| csv-narrow-25k-high-count | 32.3 ms / 5.1 MB | 34.2 ms / 5.2 MB | n/a | n/a | n/a | 193 ms / 123 MB | n/a | 22.3 ms / 46 MB |
| csv-narrow-25k-sum-score | 31.8 ms / 5.1 MB | 30.5 ms / 5.1 MB | n/a | n/a | n/a | n/a | n/a | 19.2 ms / 44 MB |
| csv-narrow-50k-count | 35.8 ms / 5.3 MB | 35.9 ms / 5.4 MB | n/a | n/a | n/a | 287 ms / 162 MB | n/a | 21.3 ms / 52 MB |
| csv-narrow-50k-first-id | 8.11 ms / 5.1 MB | 9.08 ms / 5.2 MB | n/a | n/a | n/a | 287 ms / 151 MB | n/a | 18.7 ms / 32 MB |
| csv-narrow-50k-high-count | 57.0 ms / 5.4 MB | 54.0 ms / 5.4 MB | n/a | n/a | n/a | 383 ms / 224 MB | n/a | 24.5 ms / 59 MB |
| csv-narrow-50k-sum-score | 52.0 ms / 5.3 MB | 52.5 ms / 5.4 MB | n/a | n/a | n/a | n/a | n/a | 23.6 ms / 56 MB |
| csv-narrow-100k-count | 60.3 ms / 5.8 MB | 57.9 ms / 5.9 MB | n/a | n/a | n/a | 553 ms / 283 MB | n/a | 25.9 ms / 72 MB |
| csv-narrow-100k-first-id | 9.55 ms / 5.7 MB | 10.1 ms / 5.7 MB | n/a | n/a | n/a | 550 ms / 298 MB | n/a | 18.7 ms / 33 MB |
| csv-narrow-100k-high-count | 101 ms / 5.9 MB | 99.2 ms / 5.9 MB | n/a | n/a | n/a | 736 ms / 411 MB | n/a | 29.1 ms / 70 MB |
| csv-narrow-100k-sum-score | 92.0 ms / 5.9 MB | 91.9 ms / 5.9 MB | n/a | n/a | n/a | n/a | n/a | 29.1 ms / 68 MB |
| csv-narrow-200k-count | 108 ms / 7.0 MB | 108 ms / 7.0 MB | n/a | n/a | n/a | 1063 ms / 554 MB | n/a | 33.6 ms / 77 MB |
| csv-narrow-200k-first-id | 9.88 ms / 6.8 MB | 10.0 ms / 6.9 MB | n/a | n/a | n/a | 1068 ms / 557 MB | n/a | 18.9 ms / 33 MB |
| csv-narrow-200k-high-count | 187 ms / 7.1 MB | 187 ms / 7.1 MB | n/a | n/a | n/a | 1507 ms / 816 MB | n/a | 39.5 ms / 107 MB |
| csv-narrow-200k-sum-score | 171 ms / 7.0 MB | 171 ms / 7.0 MB | n/a | n/a | n/a | n/a | n/a | 37.8 ms / 97 MB |
| ndjson-broad-100-first-id | 6.83 ms / 4.7 MB | 7.16 ms / 4.7 MB | 8.51 ms / 2.7 MB | 7.46 ms / 4.0 MB | 7.71 ms / 6.7 MB | n/a | n/a | n/a |
| ndjson-broad-100-identity | 8.33 ms / 4.8 MB | 7.90 ms / 4.8 MB | 10.6 ms / 2.7 MB | 10.2 ms / 4.0 MB | 8.22 ms / 6.9 MB | n/a | n/a | n/a |
| ndjson-broad-100-score | 6.93 ms / 4.8 MB | 7.16 ms / 4.8 MB | 7.38 ms / 2.7 MB | 7.30 ms / 4.0 MB | 8.06 ms / 6.7 MB | n/a | n/a | n/a |
| ndjson-broad-100-select-id | 6.85 ms / 4.8 MB | 6.73 ms / 4.9 MB | 7.47 ms / 2.7 MB | 7.38 ms / 4.0 MB | 8.25 ms / 6.8 MB | n/a | n/a | n/a |
| ndjson-broad-100-select-score | 8.07 ms / 4.9 MB | 8.33 ms / 4.9 MB | 13.2 ms / 2.7 MB | 7.81 ms / 4.0 MB | 8.27 ms / 7.1 MB | n/a | n/a | n/a |
| ndjson-broad-1k-first-id | 8.58 ms / 9.5 MB | 9.13 ms / 5.7 MB | 18.2 ms / 2.7 MB | 9.95 ms / 4.9 MB | 16.1 ms / 11 MB | n/a | n/a | n/a |
| ndjson-broad-1k-identity | 8.89 ms / 12 MB | 11.4 ms / 5.7 MB | 36.1 ms / 2.7 MB | 14.4 ms / 4.9 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-1k-score | 8.09 ms / 9.3 MB | 9.67 ms / 5.7 MB | 16.3 ms / 2.7 MB | 11.2 ms / 4.9 MB | 16.5 ms / 11 MB | n/a | n/a | n/a |
| ndjson-broad-1k-select-id | 7.90 ms / 10 MB | 9.91 ms / 5.8 MB | 16.9 ms / 2.7 MB | 11.1 ms / 5.0 MB | 16.9 ms / 11 MB | n/a | n/a | n/a |
| ndjson-broad-1k-select-score | 9.32 ms / 12 MB | 14.4 ms / 5.8 MB | 38.2 ms / 2.7 MB | 13.9 ms / 5.0 MB | 20.2 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-5k-first-id | 10.5 ms / 19 MB | 20.9 ms / 10 MB | 54.2 ms / 2.7 MB | 26.5 ms / 9.2 MB | 50.3 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-identity | 13.5 ms / 26 MB | 28.8 ms / 10 MB | 153 ms / 2.7 MB | 44.7 ms / 9.2 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-5k-score | 12.1 ms / 18 MB | 19.3 ms / 10 MB | 53.6 ms / 2.6 MB | 27.1 ms / 9.2 MB | 51.4 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-5k-select-id | 12.8 ms / 18 MB | 23.3 ms / 10 MB | 52.6 ms / 2.7 MB | 25.0 ms / 9.3 MB | 47.1 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-select-score | 17.0 ms / 23 MB | 46.7 ms / 10 MB | 139 ms / 2.8 MB | 45.1 ms / 9.3 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-25k-first-id | 21.5 ms / 40 MB | 67.5 ms / 32 MB | 227 ms / 2.7 MB | 100 ms / 31 MB | 216 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-25k-identity | 27.0 ms / 69 MB | 114 ms / 32 MB | 670 ms / 2.7 MB | 193 ms / 31 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-25k-score | 21.1 ms / 41 MB | 69.8 ms / 32 MB | 231 ms / 2.7 MB | 104 ms / 31 MB | 220 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-25k-select-id | 21.4 ms / 41 MB | 80.7 ms / 32 MB | 229 ms / 2.7 MB | 98.4 ms / 31 MB | 199 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-25k-select-score | 40.9 ms / 66 MB | 203 ms / 32 MB | 631 ms / 2.7 MB | 198 ms / 31 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-50k-first-id | 31.2 ms / 66 MB | 130 ms / 59 MB | 448 ms / 2.7 MB | 195 ms / 58 MB | 416 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-identity | 43.4 ms / 115 MB | 219 ms / 59 MB | 1384 ms / 2.7 MB | 367 ms / 58 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-50k-score | 31.0 ms / 66 MB | 125 ms / 59 MB | 445 ms / 2.7 MB | 196 ms / 58 MB | 414 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-select-id | 33.2 ms / 67 MB | 152 ms / 59 MB | 449 ms / 2.7 MB | 189 ms / 58 MB | 394 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-select-score | 71.8 ms / 104 MB | 396 ms / 59 MB | 1262 ms / 2.7 MB | 380 ms / 58 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-100k-first-id | 51.9 ms / 122 MB | 249 ms / 113 MB | 890 ms / 2.7 MB | 378 ms / 112 MB | 822 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-100k-identity | 72.6 ms / 184 MB | 427 ms / 113 MB | 2638 ms / 2.7 MB | 728 ms / 112 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-100k-score | 50.4 ms / 121 MB | 248 ms / 113 MB | 890 ms / 2.7 MB | 384 ms / 112 MB | 829 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-100k-select-id | 58.8 ms / 120 MB | 297 ms / 113 MB | 892 ms / 2.7 MB | 363 ms / 112 MB | 758 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-100k-select-score | 126 ms / 194 MB | 769 ms / 113 MB | 2478 ms / 2.7 MB | 751 ms / 112 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-200k-first-id | 94.3 ms / 230 MB | 473 ms / 221 MB | 1764 ms / 2.7 MB | 760 ms / 220 MB | 1640 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-identity | 141 ms / 387 MB | 829 ms / 221 MB | 5235 ms / 2.7 MB | 1433 ms / 220 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-200k-score | 90.6 ms / 231 MB | 483 ms / 221 MB | 1744 ms / 2.7 MB | 746 ms / 220 MB | 1682 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-select-id | 103 ms / 228 MB | 578 ms / 221 MB | 1765 ms / 2.6 MB | 721 ms / 220 MB | 1507 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-select-score | 246 ms / 359 MB | 1550 ms / 221 MB | 4906 ms / 2.7 MB | 1512 ms / 220 MB | disagreed | n/a | n/a | n/a |
| ndjson-narrow-100-first-id | 5.60 ms / 4.6 MB | 6.10 ms / 4.6 MB | 6.41 ms / 2.6 MB | 6.15 ms / 3.9 MB | 5.66 ms / 6.0 MB | n/a | n/a | n/a |
| ndjson-narrow-100-identity | 5.43 ms / 4.5 MB | 5.72 ms / 4.5 MB | 5.47 ms / 2.6 MB | 5.39 ms / 3.9 MB | 5.72 ms / 6.0 MB | n/a | n/a | n/a |
| ndjson-narrow-100-score | 5.50 ms / 4.6 MB | 5.97 ms / 4.6 MB | 5.37 ms / 2.6 MB | 5.06 ms / 3.9 MB | 6.04 ms / 6.0 MB | n/a | n/a | n/a |
| ndjson-narrow-100-select-id | 6.74 ms / 4.7 MB | 5.83 ms / 4.7 MB | 5.54 ms / 2.6 MB | 5.25 ms / 3.9 MB | 5.64 ms / 6.1 MB | n/a | n/a | n/a |
| ndjson-narrow-100-select-score | 6.68 ms / 4.7 MB | 5.69 ms / 4.7 MB | 5.89 ms / 2.6 MB | 5.87 ms / 3.9 MB | 7.35 ms / 6.1 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-first-id | 6.21 ms / 4.7 MB | 6.23 ms / 4.7 MB | 6.04 ms / 2.6 MB | 5.88 ms / 3.9 MB | 7.80 ms / 7.5 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-identity | 6.70 ms / 4.5 MB | 6.81 ms / 4.5 MB | 6.90 ms / 2.6 MB | 6.57 ms / 3.9 MB | 10.1 ms / 7.6 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-score | 5.87 ms / 4.7 MB | 6.27 ms / 4.7 MB | 6.06 ms / 2.6 MB | 6.03 ms / 3.9 MB | 6.99 ms / 7.6 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-select-id | 6.86 ms / 4.7 MB | 7.18 ms / 4.7 MB | 6.52 ms / 2.6 MB | 6.58 ms / 3.9 MB | 8.43 ms / 8.2 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-select-score | 6.74 ms / 4.8 MB | 8.46 ms / 4.8 MB | 8.06 ms / 2.6 MB | 7.85 ms / 3.9 MB | 8.23 ms / 8.6 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-first-id | 9.83 ms / 4.8 MB | 9.58 ms / 4.8 MB | 9.50 ms / 2.6 MB | 12.6 ms / 4.0 MB | 12.7 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-identity | 7.92 ms / 4.7 MB | 8.05 ms / 4.7 MB | 10.8 ms / 2.6 MB | 9.28 ms / 4.0 MB | 13.2 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-score | 8.83 ms / 4.8 MB | 8.86 ms / 4.8 MB | 8.97 ms / 2.6 MB | 11.2 ms / 4.0 MB | 12.6 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-select-id | 8.86 ms / 4.8 MB | 8.81 ms / 4.8 MB | 9.02 ms / 2.6 MB | 8.66 ms / 4.0 MB | 9.74 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-select-score | 9.59 ms / 4.9 MB | 9.44 ms / 4.9 MB | 11.8 ms / 2.6 MB | 9.88 ms / 4.0 MB | 13.8 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-first-id | 10.6 ms / 7.4 MB | 23.1 ms / 5.2 MB | 21.6 ms / 2.6 MB | 22.3 ms / 4.4 MB | 38.7 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-identity | 9.48 ms / 8.1 MB | 18.1 ms / 5.1 MB | 29.7 ms / 2.6 MB | 22.8 ms / 4.4 MB | 41.5 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-score | 10.8 ms / 7.5 MB | 21.6 ms / 5.3 MB | 21.8 ms / 2.7 MB | 22.5 ms / 4.4 MB | 38.9 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-select-id | 10.7 ms / 7.3 MB | 24.6 ms / 5.3 MB | 22.1 ms / 2.6 MB | 17.9 ms / 4.5 MB | 23.6 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-select-score | 11.8 ms / 8.5 MB | 25.3 ms / 5.3 MB | 34.7 ms / 2.6 MB | 27.6 ms / 4.5 MB | 45.8 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-first-id | 14.0 ms / 10 MB | 38.0 ms / 5.9 MB | 38.6 ms / 2.6 MB | 40.8 ms / 5.0 MB | 76.8 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-identity | 12.6 ms / 11 MB | 28.8 ms / 5.8 MB | 63.2 ms / 2.7 MB | 39.0 ms / 5.0 MB | 77.0 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-score | 15.5 ms / 10 MB | 41.2 ms / 5.9 MB | 39.2 ms / 2.6 MB | 39.3 ms / 5.0 MB | 71.0 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-select-id | 14.7 ms / 9.5 MB | 39.8 ms / 5.9 MB | 39.2 ms / 2.6 MB | 30.4 ms / 5.1 MB | 41.0 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-select-score | 16.6 ms / 12 MB | 48.5 ms / 6.0 MB | 63.0 ms / 2.7 MB | 52.0 ms / 5.1 MB | 83.4 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-first-id | 18.5 ms / 13 MB | 68.9 ms / 7.0 MB | 70.0 ms / 2.6 MB | 73.5 ms / 6.2 MB | 135 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-identity | 16.0 ms / 16 MB | 52.3 ms / 6.9 MB | 107 ms / 2.6 MB | 74.8 ms / 6.2 MB | 146 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-score | 20.4 ms / 13 MB | 69.5 ms / 7.0 MB | 72.1 ms / 2.6 MB | 73.7 ms / 6.2 MB | 134 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-select-id | 22.1 ms / 12 MB | 72.1 ms / 7.1 MB | 72.9 ms / 2.6 MB | 57.1 ms / 6.3 MB | 75.2 ms / 14 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-select-score | 19.9 ms / 16 MB | 87.3 ms / 7.1 MB | 117 ms / 2.7 MB | 94.8 ms / 6.3 MB | 160 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-first-id | 30.4 ms / 18 MB | 133 ms / 9.5 MB | 134 ms / 2.7 MB | 142 ms / 8.7 MB | 261 ms / 14 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-identity | 28.1 ms / 23 MB | 103 ms / 9.4 MB | 196 ms / 2.7 MB | 147 ms / 8.7 MB | 298 ms / 14 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-score | 28.4 ms / 18 MB | 133 ms / 9.5 MB | 133 ms / 2.7 MB | 141 ms / 8.7 MB | 267 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-select-id | 32.4 ms / 16 MB | 141 ms / 9.5 MB | 138 ms / 2.6 MB | 110 ms / 8.7 MB | 152 ms / 14 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-select-score | 31.8 ms / 21 MB | 169 ms / 9.6 MB | 217 ms / 2.7 MB | 184 ms / 8.7 MB | 313 ms / 13 MB | n/a | n/a | n/a |
| toml-broad-100-count | 9.70 ms / 6.2 MB | 9.89 ms / 6.2 MB | n/a | 10.8 ms / 5.5 MB | n/a | 147 ms / 39 MB | 11.8 ms / 13 MB | n/a |
| toml-broad-100-descent | 10.1 ms / 6.4 MB | 10.2 ms / 6.3 MB | n/a | 9.88 ms / 5.6 MB | n/a | 152 ms / 37 MB | n/a | n/a |
| toml-broad-100-exact-name | 8.56 ms / 4.9 MB | 9.33 ms / 4.9 MB | n/a | 10.6 ms / 5.3 MB | n/a | 151 ms / 38 MB | 12.3 ms / 13 MB | n/a |
| toml-broad-100-first-id | 8.61 ms / 4.9 MB | 9.39 ms / 4.9 MB | n/a | 11.4 ms / 5.4 MB | n/a | 154 ms / 39 MB | 12.4 ms / 13 MB | n/a |
| toml-broad-100-identity | 9.66 ms / 6.2 MB | 10.8 ms / 6.2 MB | n/a | 11.8 ms / 5.4 MB | n/a | 150 ms / 45 MB | 14.7 ms / 15 MB | n/a |
| toml-broad-100-ids | 9.19 ms / 6.1 MB | 10.6 ms / 6.1 MB | n/a | 10.1 ms / 5.4 MB | n/a | 145 ms / 43 MB | n/a | n/a |
| toml-broad-100-keys-publish | 9.34 ms / 5.0 MB | 9.11 ms / 5.0 MB | n/a | 9.97 ms / 5.6 MB | n/a | disagreed | n/a | n/a |
| toml-broad-100-nested-dept | 9.16 ms / 4.9 MB | 9.98 ms / 4.9 MB | n/a | 11.3 ms / 5.3 MB | n/a | 156 ms / 37 MB | 12.1 ms / 13 MB | n/a |
| toml-broad-100-type-path | 10.2 ms / 7.1 MB | 9.81 ms / 7.0 MB | n/a | 10.5 ms / 5.4 MB | n/a | disagreed | n/a | n/a |
| toml-broad-1k-count | 16.7 ms / 20 MB | 18.2 ms / 21 MB | n/a | 24.1 ms / 15 MB | n/a | excluded | 32.2 ms / 32 MB | n/a |
| toml-broad-1k-descent | 21.3 ms / 21 MB | 22.5 ms / 21 MB | n/a | 32.4 ms / 17 MB | n/a | excluded | n/a | n/a |
| toml-broad-1k-exact-name | 18.8 ms / 6.5 MB | 19.6 ms / 6.5 MB | n/a | 29.1 ms / 15 MB | n/a | excluded | 35.4 ms / 33 MB | n/a |
| toml-broad-1k-first-id | 15.3 ms / 6.5 MB | 13.8 ms / 6.5 MB | n/a | 25.6 ms / 15 MB | n/a | excluded | 33.8 ms / 32 MB | n/a |
| toml-broad-1k-identity | 20.5 ms / 21 MB | 22.2 ms / 21 MB | n/a | 32.3 ms / 15 MB | n/a | excluded | 61.9 ms / 44 MB | n/a |
| toml-broad-1k-ids | 17.0 ms / 18 MB | 19.8 ms / 18 MB | n/a | 26.9 ms / 15 MB | n/a | excluded | n/a | n/a |
| toml-broad-1k-keys-publish | 16.3 ms / 6.5 MB | 15.4 ms / 6.5 MB | n/a | 25.6 ms / 15 MB | n/a | excluded | n/a | n/a |
| toml-broad-1k-nested-dept | 18.8 ms / 6.5 MB | 15.3 ms / 6.5 MB | n/a | 25.9 ms / 15 MB | n/a | excluded | 34.7 ms / 32 MB | n/a |
| toml-broad-1k-type-path | 26.7 ms / 26 MB | 25.6 ms / 26 MB | n/a | 26.4 ms / 15 MB | n/a | excluded | n/a | n/a |
| toml-broad-5k-count | 53.1 ms / 78 MB | 54.3 ms / 78 MB | n/a | 87.7 ms / 70 MB | n/a | excluded | 106 ms / 107 MB | n/a |
| toml-broad-5k-descent | 61.3 ms / 78 MB | 61.8 ms / 78 MB | n/a | 102 ms / 78 MB | n/a | excluded | n/a | n/a |
| toml-broad-5k-exact-name | 44.6 ms / 14 MB | 35.9 ms / 14 MB | n/a | 85.4 ms / 70 MB | n/a | excluded | 113 ms / 107 MB | n/a |
| toml-broad-5k-first-id | 36.8 ms / 14 MB | 35.9 ms / 14 MB | n/a | 87.5 ms / 70 MB | n/a | excluded | 111 ms / 106 MB | n/a |
| toml-broad-5k-identity | 57.2 ms / 78 MB | 58.9 ms / 78 MB | n/a | 107 ms / 70 MB | n/a | excluded | 215 ms / 162 MB | n/a |
| toml-broad-5k-ids | 56.1 ms / 68 MB | 57.0 ms / 68 MB | n/a | 85.6 ms / 70 MB | n/a | excluded | n/a | n/a |
| toml-broad-5k-keys-publish | 37.4 ms / 14 MB | 37.5 ms / 14 MB | n/a | 87.8 ms / 70 MB | n/a | excluded | n/a | n/a |
| toml-broad-5k-nested-dept | 35.7 ms / 14 MB | 36.3 ms / 14 MB | n/a | 87.9 ms / 70 MB | n/a | excluded | 110 ms / 109 MB | n/a |
| toml-broad-5k-type-path | 85.1 ms / 113 MB | 82.0 ms / 113 MB | n/a | 87.1 ms / 70 MB | n/a | excluded | n/a | n/a |
| toml-broad-25k-count | 207 ms / 320 MB | 205 ms / 304 MB | n/a | 380 ms / 285 MB | n/a | excluded | 470 ms / 522 MB | n/a |
| toml-broad-25k-descent | 249 ms / 321 MB | 248 ms / 305 MB | n/a | 460 ms / 332 MB | n/a | excluded | n/a | n/a |
| toml-broad-25k-exact-name | 133 ms / 50 MB | 129 ms / 49 MB | n/a | 381 ms / 284 MB | n/a | excluded | 466 ms / 504 MB | n/a |
| toml-broad-25k-first-id | 134 ms / 50 MB | 133 ms / 49 MB | n/a | 385 ms / 284 MB | n/a | excluded | 467 ms / 510 MB | n/a |
| toml-broad-25k-identity | 242 ms / 302 MB | 238 ms / 305 MB | n/a | 474 ms / 284 MB | n/a | excluded | 998 ms / 810 MB | n/a |
| toml-broad-25k-ids | 329 ms / 285 MB | 328 ms / 275 MB | n/a | 378 ms / 284 MB | n/a | excluded | n/a | n/a |
| toml-broad-25k-keys-publish | 130 ms / 51 MB | 133 ms / 49 MB | n/a | 386 ms / 285 MB | n/a | excluded | n/a | n/a |
| toml-broad-25k-nested-dept | 143 ms / 50 MB | 133 ms / 49 MB | n/a | 388 ms / 284 MB | n/a | excluded | 478 ms / 512 MB | n/a |
| toml-broad-25k-type-path | 361 ms / 452 MB | 368 ms / 422 MB | n/a | 387 ms / 284 MB | n/a | excluded | n/a | n/a |
| toml-broad-50k-count | 407 ms / 673 MB | 414 ms / 673 MB | n/a | 756 ms / 545 MB | n/a | excluded | 927 ms / 966 MB | n/a |
| toml-broad-50k-descent | 489 ms / 674 MB | 495 ms / 674 MB | n/a | 894 ms / 626 MB | n/a | excluded | n/a | n/a |
| toml-broad-50k-exact-name | 254 ms / 93 MB | 254 ms / 93 MB | n/a | 755 ms / 545 MB | n/a | excluded | 945 ms / 968 MB | n/a |
| toml-broad-50k-first-id | 253 ms / 93 MB | 248 ms / 93 MB | n/a | 748 ms / 545 MB | n/a | excluded | 947 ms / 964 MB | n/a |
| toml-broad-50k-identity | 469 ms / 674 MB | 464 ms / 674 MB | n/a | 946 ms / 545 MB | n/a | excluded | 1953 ms / 1507 MB | n/a |
| toml-broad-50k-ids | 865 ms / 618 MB | 874 ms / 618 MB | n/a | 761 ms / 545 MB | n/a | excluded | n/a | n/a |
| toml-broad-50k-keys-publish | 254 ms / 93 MB | 246 ms / 93 MB | n/a | 755 ms / 545 MB | n/a | excluded | n/a | n/a |
| toml-broad-50k-nested-dept | 253 ms / 93 MB | 247 ms / 93 MB | n/a | 756 ms / 545 MB | n/a | excluded | 950 ms / 961 MB | n/a |
| toml-broad-50k-type-path | 718 ms / 873 MB | 704 ms / 873 MB | n/a | 760 ms / 545 MB | n/a | excluded | n/a | n/a |
| toml-broad-100k-count | 810 ms / 1324 MB | 804 ms / 1324 MB | n/a | 1497 ms / 1086 MB | n/a | excluded | 1862 ms / 1904 MB | n/a |
| toml-broad-100k-descent | 964 ms / 1325 MB | 975 ms / 1325 MB | n/a | 1807 ms / 1272 MB | n/a | excluded | n/a | n/a |
| toml-broad-100k-exact-name | 496 ms / 180 MB | 482 ms / 180 MB | n/a | 1490 ms / 1086 MB | n/a | excluded | 1871 ms / 1914 MB | n/a |
| toml-broad-100k-first-id | 486 ms / 180 MB | 490 ms / 180 MB | n/a | 1495 ms / 1086 MB | n/a | excluded | 1866 ms / 1914 MB | n/a |
| toml-broad-100k-identity | 934 ms / 1325 MB | 922 ms / 1325 MB | n/a | 1864 ms / 1086 MB | n/a | excluded | 3951 ms / 3003 MB | n/a |
| toml-broad-100k-ids | 2716 ms / 1217 MB | 2685 ms / 1217 MB | n/a | 1504 ms / 1086 MB | n/a | excluded | n/a | n/a |
| toml-broad-100k-keys-publish | 498 ms / 181 MB | 498 ms / 181 MB | n/a | 1512 ms / 1086 MB | n/a | excluded | n/a | n/a |
| toml-broad-100k-nested-dept | 494 ms / 180 MB | 496 ms / 180 MB | n/a | 1501 ms / 1086 MB | n/a | excluded | 1866 ms / 1923 MB | n/a |
| toml-broad-100k-type-path | 1422 ms / 1746 MB | 1416 ms / 1746 MB | n/a | 1503 ms / 1086 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100-count | 9.20 ms / 4.8 MB | 8.46 ms / 4.8 MB | n/a | 7.94 ms / 4.2 MB | n/a | 22.4 ms / 29 MB | 9.81 ms / 9.6 MB | n/a |
| toml-narrow-100-descent | 7.68 ms / 5.0 MB | 7.38 ms / 5.0 MB | n/a | 7.26 ms / 4.2 MB | n/a | 22.0 ms / 29 MB | n/a | n/a |
| toml-narrow-100-exact-name | 7.27 ms / 4.7 MB | 7.70 ms / 4.7 MB | n/a | 7.98 ms / 4.1 MB | n/a | 23.5 ms / 29 MB | error | n/a |
| toml-narrow-100-first-id | 8.32 ms / 4.7 MB | 7.85 ms / 4.7 MB | n/a | 8.60 ms / 4.1 MB | n/a | 22.5 ms / 29 MB | 9.71 ms / 9.7 MB | n/a |
| toml-narrow-100-identity | 7.27 ms / 4.7 MB | 7.47 ms / 4.7 MB | n/a | 7.37 ms / 4.1 MB | n/a | 20.5 ms / 29 MB | 8.75 ms / 9.9 MB | n/a |
| toml-narrow-100-ids | 8.09 ms / 5.0 MB | 7.84 ms / 5.0 MB | n/a | 7.75 ms / 4.2 MB | n/a | 22.1 ms / 29 MB | n/a | n/a |
| toml-narrow-100-keys-publish | 8.86 ms / 4.8 MB | 9.03 ms / 4.8 MB | n/a | 11.8 ms / 4.3 MB | n/a | 21.9 ms / 30 MB | n/a | n/a |
| toml-narrow-100-nested-dept | 7.90 ms / 4.7 MB | 7.96 ms / 4.7 MB | n/a | 8.61 ms / 4.1 MB | n/a | 24.9 ms / 29 MB | error | n/a |
| toml-narrow-100-type-path | 8.52 ms / 4.9 MB | 8.24 ms / 4.9 MB | n/a | 8.04 ms / 4.1 MB | n/a | disagreed | n/a | n/a |
| toml-narrow-1k-count | 8.90 ms / 5.6 MB | 9.19 ms / 5.6 MB | n/a | 8.69 ms / 5.7 MB | n/a | 738 ms / 38 MB | 11.0 ms / 13 MB | n/a |
| toml-narrow-1k-descent | 8.70 ms / 5.8 MB | 8.87 ms / 5.8 MB | n/a | 10.4 ms / 5.8 MB | n/a | 715 ms / 31 MB | n/a | n/a |
| toml-narrow-1k-exact-name | 8.63 ms / 5.1 MB | 7.98 ms / 5.1 MB | n/a | 9.19 ms / 5.7 MB | n/a | 734 ms / 37 MB | error | n/a |
| toml-narrow-1k-first-id | 8.40 ms / 5.1 MB | 7.94 ms / 5.1 MB | n/a | 9.15 ms / 5.7 MB | n/a | 737 ms / 31 MB | 10.8 ms / 13 MB | n/a |
| toml-narrow-1k-identity | 8.51 ms / 5.6 MB | 10.2 ms / 5.6 MB | n/a | 9.85 ms / 5.6 MB | n/a | 738 ms / 38 MB | 12.5 ms / 14 MB | n/a |
| toml-narrow-1k-ids | 9.24 ms / 5.9 MB | 9.15 ms / 5.9 MB | n/a | 9.32 ms / 5.7 MB | n/a | 738 ms / 38 MB | n/a | n/a |
| toml-narrow-1k-keys-publish | 7.85 ms / 5.1 MB | 8.57 ms / 5.2 MB | n/a | 9.54 ms / 5.7 MB | n/a | 718 ms / 33 MB | n/a | n/a |
| toml-narrow-1k-nested-dept | 8.62 ms / 5.0 MB | 9.13 ms / 5.1 MB | n/a | 9.66 ms / 5.5 MB | n/a | 711 ms / 37 MB | error | n/a |
| toml-narrow-1k-type-path | 8.50 ms / 5.9 MB | 8.44 ms / 6.0 MB | n/a | 9.80 ms / 5.6 MB | n/a | disagreed | n/a | n/a |
| toml-narrow-5k-count | 10.2 ms / 9.9 MB | 9.85 ms / 9.9 MB | n/a | 12.2 ms / 11 MB | n/a | excluded | 17.2 ms / 19 MB | n/a |
| toml-narrow-5k-descent | 12.8 ms / 10 MB | 11.2 ms / 10 MB | n/a | 12.8 ms / 11 MB | n/a | excluded | n/a | n/a |
| toml-narrow-5k-exact-name | 9.17 ms / 6.6 MB | 9.48 ms / 6.6 MB | n/a | 12.6 ms / 11 MB | n/a | excluded | error | n/a |
| toml-narrow-5k-first-id | 9.75 ms / 6.6 MB | 10.3 ms / 6.6 MB | n/a | 12.6 ms / 11 MB | n/a | excluded | 18.0 ms / 19 MB | n/a |
| toml-narrow-5k-identity | 10.7 ms / 10.0 MB | 10.8 ms / 10.0 MB | n/a | 14.7 ms / 11 MB | n/a | excluded | 24.9 ms / 23 MB | n/a |
| toml-narrow-5k-ids | 11.2 ms / 10 MB | 11.3 ms / 10 MB | n/a | 13.2 ms / 11 MB | n/a | excluded | n/a | n/a |
| toml-narrow-5k-keys-publish | 10.5 ms / 6.7 MB | 10.5 ms / 6.7 MB | n/a | 13.2 ms / 11 MB | n/a | excluded | n/a | n/a |
| toml-narrow-5k-nested-dept | 10.8 ms / 6.6 MB | 10.1 ms / 6.6 MB | n/a | 13.2 ms / 11 MB | n/a | excluded | error | n/a |
| toml-narrow-5k-type-path | 16.7 ms / 11 MB | 11.9 ms / 11 MB | n/a | 13.3 ms / 11 MB | n/a | excluded | n/a | n/a |
| toml-narrow-25k-count | 19.7 ms / 26 MB | 18.9 ms / 27 MB | n/a | 29.5 ms / 50 MB | n/a | excluded | 45.9 ms / 46 MB | n/a |
| toml-narrow-25k-descent | 23.1 ms / 27 MB | 24.5 ms / 27 MB | n/a | 37.2 ms / 51 MB | n/a | excluded | n/a | n/a |
| toml-narrow-25k-exact-name | 15.6 ms / 14 MB | 17.8 ms / 14 MB | n/a | 34.6 ms / 50 MB | n/a | excluded | error | n/a |
| toml-narrow-25k-first-id | 14.9 ms / 14 MB | 15.6 ms / 14 MB | n/a | 30.9 ms / 50 MB | n/a | excluded | 48.0 ms / 46 MB | n/a |
| toml-narrow-25k-identity | 21.7 ms / 26 MB | 22.6 ms / 27 MB | n/a | 35.9 ms / 50 MB | n/a | excluded | 79.2 ms / 68 MB | n/a |
| toml-narrow-25k-ids | 33.6 ms / 27 MB | 35.2 ms / 27 MB | n/a | 32.6 ms / 50 MB | n/a | excluded | n/a | n/a |
| toml-narrow-25k-keys-publish | 18.6 ms / 14 MB | 19.8 ms / 14 MB | n/a | 34.7 ms / 50 MB | n/a | excluded | n/a | n/a |
| toml-narrow-25k-nested-dept | 15.1 ms / 14 MB | 16.3 ms / 14 MB | n/a | 32.1 ms / 50 MB | n/a | excluded | error | n/a |
| toml-narrow-25k-type-path | 30.1 ms / 32 MB | 30.8 ms / 32 MB | n/a | 34.1 ms / 50 MB | n/a | excluded | n/a | n/a |
| toml-narrow-50k-count | 31.7 ms / 54 MB | 31.6 ms / 54 MB | n/a | 50.9 ms / 80 MB | n/a | excluded | 80.5 ms / 81 MB | n/a |
| toml-narrow-50k-descent | 35.6 ms / 54 MB | 36.0 ms / 54 MB | n/a | 60.0 ms / 80 MB | n/a | excluded | n/a | n/a |
| toml-narrow-50k-exact-name | 22.9 ms / 24 MB | 21.6 ms / 24 MB | n/a | 52.9 ms / 80 MB | n/a | excluded | error | n/a |
| toml-narrow-50k-first-id | 22.5 ms / 24 MB | 23.2 ms / 24 MB | n/a | 52.2 ms / 80 MB | n/a | excluded | 80.0 ms / 82 MB | n/a |
| toml-narrow-50k-identity | 34.5 ms / 54 MB | 36.0 ms / 54 MB | n/a | 60.9 ms / 80 MB | n/a | excluded | 141 ms / 123 MB | n/a |
| toml-narrow-50k-ids | 66.4 ms / 57 MB | 66.3 ms / 57 MB | n/a | 56.2 ms / 80 MB | n/a | excluded | n/a | n/a |
| toml-narrow-50k-keys-publish | 22.5 ms / 24 MB | 22.6 ms / 24 MB | n/a | 51.0 ms / 80 MB | n/a | excluded | n/a | n/a |
| toml-narrow-50k-nested-dept | 23.4 ms / 24 MB | 23.3 ms / 24 MB | n/a | 51.5 ms / 80 MB | n/a | excluded | error | n/a |
| toml-narrow-50k-type-path | 44.8 ms / 65 MB | 45.1 ms / 64 MB | n/a | 52.2 ms / 80 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100k-count | 49.9 ms / 94 MB | 49.1 ms / 94 MB | n/a | 92.4 ms / 139 MB | n/a | excluded | 144 ms / 158 MB | n/a |
| toml-narrow-100k-descent | 61.0 ms / 94 MB | 62.3 ms / 94 MB | n/a | 108 ms / 139 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100k-exact-name | 35.4 ms / 42 MB | 35.3 ms / 42 MB | n/a | 92.1 ms / 139 MB | n/a | excluded | error | n/a |
| toml-narrow-100k-first-id | 35.4 ms / 42 MB | 36.7 ms / 42 MB | n/a | 93.8 ms / 139 MB | n/a | excluded | 146 ms / 159 MB | n/a |
| toml-narrow-100k-identity | 55.2 ms / 94 MB | 55.4 ms / 94 MB | n/a | 112 ms / 139 MB | n/a | excluded | 267 ms / 223 MB | n/a |
| toml-narrow-100k-ids | 184 ms / 95 MB | 181 ms / 95 MB | n/a | 100 ms / 139 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100k-keys-publish | 35.1 ms / 42 MB | 33.8 ms / 42 MB | n/a | 93.2 ms / 139 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100k-nested-dept | 35.1 ms / 42 MB | 34.4 ms / 42 MB | n/a | 94.8 ms / 139 MB | n/a | excluded | error | n/a |
| toml-narrow-100k-type-path | 78.9 ms / 129 MB | 76.5 ms / 126 MB | n/a | 94.2 ms / 139 MB | n/a | excluded | n/a | n/a |
| users-broad-100-all-nonneg | 4.58 ms / 5.0 MB | 4.60 ms / 5.0 MB | 5.85 ms / 3.6 MB | 4.58 ms / 5.1 MB | 5.12 ms / 7.1 MB | n/a | n/a | n/a |
| users-broad-100-any-high | 4.49 ms / 5.0 MB | 4.53 ms / 5.0 MB | 5.18 ms / 3.6 MB | 4.42 ms / 5.1 MB | 5.08 ms / 6.8 MB | n/a | n/a | n/a |
| users-broad-100-count | 4.28 ms / 4.7 MB | 4.24 ms / 4.7 MB | 5.55 ms / 3.6 MB | 4.58 ms / 4.8 MB | 4.95 ms / 6.7 MB | 11.4 ms / 37 MB | n/a | n/a |
| users-broad-100-descent | 4.84 ms / 6.0 MB | 4.86 ms / 6.0 MB | 6.11 ms / 4.1 MB | 4.95 ms / 5.0 MB | 7.73 ms / 10 MB | 14.8 ms / 53 MB | n/a | n/a |
| users-broad-100-filter-active | 4.36 ms / 5.0 MB | 4.46 ms / 5.0 MB | 5.52 ms / 3.6 MB | 4.39 ms / 4.8 MB | 5.01 ms / 7.0 MB | 12.2 ms / 40 MB | n/a | n/a |
| users-broad-100-first-id | 4.99 ms / 4.6 MB | 4.45 ms / 4.6 MB | 5.44 ms / 3.6 MB | 8.60 ms / 4.7 MB | 5.21 ms / 6.9 MB | 10.3 ms / 37 MB | n/a | n/a |
| users-broad-100-group-mod | 5.46 ms / 6.4 MB | 5.77 ms / 6.4 MB | 5.69 ms / 3.6 MB | 4.86 ms / 5.1 MB | 6.09 ms / 7.0 MB | 13.1 ms / 47 MB | n/a | n/a |
| users-broad-100-high-score | 5.21 ms / 4.9 MB | 5.13 ms / 4.9 MB | 5.82 ms / 3.6 MB | 5.15 ms / 4.8 MB | 5.81 ms / 6.8 MB | 12.3 ms / 41 MB | n/a | n/a |
| users-broad-100-identity | 5.44 ms / 6.4 MB | 5.72 ms / 6.4 MB | 7.86 ms / 3.7 MB | 4.92 ms / 4.7 MB | 8.26 ms / 7.0 MB | 13.9 ms / 46 MB | n/a | n/a |
| users-broad-100-ids | 5.59 ms / 4.7 MB | 4.73 ms / 4.7 MB | 5.10 ms / 3.6 MB | 4.47 ms / 4.7 MB | 5.04 ms / 6.8 MB | 10.6 ms / 21 MB | n/a | n/a |
| users-broad-100-keys-len | 4.48 ms / 4.8 MB | 4.53 ms / 4.8 MB | 5.80 ms / 3.6 MB | 4.38 ms / 4.9 MB | 5.00 ms / 7.0 MB | 10.2 ms / 37 MB | n/a | n/a |
| users-broad-100-keys-publish | 5.05 ms / 4.8 MB | 4.75 ms / 4.8 MB | 5.36 ms / 3.6 MB | 4.49 ms / 4.8 MB | 5.01 ms / 6.9 MB | disagreed | n/a | n/a |
| users-broad-100-max-score | 6.25 ms / 5.0 MB | 4.55 ms / 5.0 MB | 5.25 ms / 3.6 MB | 4.72 ms / 4.9 MB | 5.18 ms / 7.0 MB | 10.4 ms / 37 MB | n/a | n/a |
| users-broad-100-nested-dept | 4.40 ms / 4.6 MB | 4.56 ms / 4.6 MB | 5.34 ms / 3.5 MB | 4.48 ms / 4.7 MB | 5.10 ms / 6.9 MB | 9.77 ms / 37 MB | n/a | n/a |
| users-broad-100-project-names | 4.28 ms / 4.6 MB | 4.45 ms / 4.6 MB | 5.75 ms / 3.6 MB | 4.32 ms / 4.7 MB | 5.08 ms / 7.0 MB | 11.3 ms / 37 MB | n/a | n/a |
| users-broad-100-project-pair | 5.74 ms / 4.8 MB | 4.61 ms / 4.8 MB | 5.26 ms / 3.6 MB | 4.44 ms / 4.7 MB | 5.19 ms / 7.0 MB | 11.7 ms / 41 MB | n/a | n/a |
| users-broad-100-reduce-score | 4.31 ms / 5.0 MB | 4.37 ms / 5.0 MB | 5.41 ms / 3.6 MB | 4.38 ms / 4.7 MB | 5.02 ms / 6.8 MB | n/a | n/a | n/a |
| users-broad-100-reverse-id | 5.69 ms / 6.2 MB | 5.52 ms / 6.2 MB | 5.20 ms / 3.6 MB | 4.92 ms / 4.8 MB | 5.03 ms / 7.0 MB | 11.9 ms / 41 MB | n/a | n/a |
| users-broad-100-select-id-stream | 4.82 ms / 4.7 MB | 5.04 ms / 4.7 MB | 5.28 ms / 3.5 MB | 4.50 ms / 4.7 MB | 5.10 ms / 7.0 MB | n/a | n/a | n/a |
| users-broad-100-slice-length | 4.28 ms / 4.7 MB | 4.13 ms / 4.8 MB | 5.44 ms / 3.6 MB | 4.34 ms / 4.8 MB | 5.56 ms / 6.9 MB | 11.5 ms / 37 MB | n/a | n/a |
| users-broad-100-sort-last | 4.95 ms / 6.2 MB | 5.13 ms / 6.2 MB | 5.41 ms / 3.6 MB | 4.83 ms / 5.0 MB | 4.98 ms / 6.8 MB | 10.6 ms / 41 MB | n/a | n/a |
| users-broad-100-sum-score | 4.43 ms / 5.0 MB | 4.48 ms / 5.0 MB | 5.26 ms / 3.6 MB | 4.58 ms / 4.8 MB | 5.55 ms / 6.8 MB | n/a | n/a | n/a |
| users-broad-100-type-path | 4.74 ms / 4.6 MB | 4.45 ms / 4.6 MB | 5.30 ms / 3.6 MB | 4.65 ms / 4.7 MB | 5.02 ms / 7.0 MB | disagreed | n/a | n/a |
| users-broad-100-unique-scores | 5.74 ms / 6.1 MB | 5.87 ms / 6.1 MB | 5.97 ms / 3.6 MB | 4.93 ms / 4.9 MB | 6.26 ms / 6.8 MB | 11.2 ms / 37 MB | n/a | n/a |
| users-broad-1k-all-nonneg | 7.39 ms / 6.3 MB | 7.74 ms / 6.3 MB | 15.5 ms / 12 MB | 9.18 ms / 13 MB | 13.1 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-any-high | 7.79 ms / 6.3 MB | 8.06 ms / 6.3 MB | 15.5 ms / 12 MB | 9.76 ms / 13 MB | 14.7 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-count | 5.38 ms / 5.8 MB | 5.95 ms / 5.8 MB | 14.0 ms / 12 MB | 8.75 ms / 13 MB | 12.2 ms / 14 MB | 26.8 ms / 70 MB | n/a | n/a |
| users-broad-1k-descent | 11.4 ms / 13 MB | 11.1 ms / 13 MB | 30.0 ms / 16 MB | 13.3 ms / 15 MB | 32.3 ms / 24 MB | 81.0 ms / 204 MB | n/a | n/a |
| users-broad-1k-filter-active | 5.72 ms / 6.4 MB | 6.59 ms / 6.3 MB | 13.8 ms / 12 MB | 9.15 ms / 13 MB | 12.5 ms / 15 MB | 33.4 ms / 100 MB | n/a | n/a |
| users-broad-1k-first-id | 6.01 ms / 5.5 MB | 6.39 ms / 5.6 MB | 14.0 ms / 12 MB | 9.49 ms / 13 MB | 13.2 ms / 15 MB | 27.8 ms / 70 MB | n/a | n/a |
| users-broad-1k-group-mod | 12.0 ms / 16 MB | 11.0 ms / 16 MB | 15.5 ms / 12 MB | 8.87 ms / 14 MB | 12.5 ms / 15 MB | 45.1 ms / 134 MB | n/a | n/a |
| users-broad-1k-high-score | 5.78 ms / 6.3 MB | 6.08 ms / 6.3 MB | 14.4 ms / 12 MB | 10.8 ms / 13 MB | 13.8 ms / 15 MB | 38.3 ms / 107 MB | n/a | n/a |
| users-broad-1k-identity | 15.0 ms / 17 MB | 13.7 ms / 17 MB | 33.0 ms / 13 MB | 12.4 ms / 13 MB | disagreed | 59.3 ms / 111 MB | n/a | n/a |
| users-broad-1k-ids | 6.35 ms / 5.8 MB | 6.61 ms / 5.8 MB | 14.3 ms / 12 MB | 9.17 ms / 13 MB | 12.8 ms / 15 MB | 28.8 ms / 72 MB | n/a | n/a |
| users-broad-1k-keys-len | 6.26 ms / 5.8 MB | 6.52 ms / 5.8 MB | 14.0 ms / 12 MB | 9.92 ms / 13 MB | 12.4 ms / 15 MB | 26.9 ms / 71 MB | n/a | n/a |
| users-broad-1k-keys-publish | 6.80 ms / 5.8 MB | 7.34 ms / 5.8 MB | 14.2 ms / 12 MB | 8.71 ms / 13 MB | 12.6 ms / 14 MB | disagreed | n/a | n/a |
| users-broad-1k-max-score | 7.17 ms / 6.3 MB | 7.77 ms / 6.3 MB | 15.4 ms / 12 MB | 8.78 ms / 13 MB | 12.4 ms / 15 MB | 29.6 ms / 73 MB | n/a | n/a |
| users-broad-1k-nested-dept | 7.35 ms / 5.6 MB | 7.19 ms / 5.6 MB | 16.2 ms / 12 MB | 9.49 ms / 13 MB | 13.7 ms / 15 MB | 28.9 ms / 71 MB | n/a | n/a |
| users-broad-1k-project-names | 6.19 ms / 5.7 MB | 6.77 ms / 5.7 MB | 14.3 ms / 12 MB | 8.85 ms / 13 MB | 12.4 ms / 15 MB | 28.4 ms / 72 MB | n/a | n/a |
| users-broad-1k-project-pair | 8.74 ms / 6.1 MB | 8.79 ms / 6.1 MB | 16.5 ms / 13 MB | 9.72 ms / 13 MB | 13.3 ms / 15 MB | 41.2 ms / 109 MB | n/a | n/a |
| users-broad-1k-reduce-score | 5.63 ms / 6.2 MB | 5.99 ms / 6.3 MB | 14.1 ms / 12 MB | 8.89 ms / 13 MB | 12.4 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-reverse-id | 10.6 ms / 16 MB | 10.4 ms / 15 MB | 15.4 ms / 12 MB | 8.61 ms / 13 MB | 12.5 ms / 14 MB | 35.9 ms / 101 MB | n/a | n/a |
| users-broad-1k-select-id-stream | 7.25 ms / 5.7 MB | 7.45 ms / 5.7 MB | 15.8 ms / 12 MB | 10.7 ms / 13 MB | 14.7 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-slice-length | 6.01 ms / 5.9 MB | 6.19 ms / 5.9 MB | 14.4 ms / 12 MB | 8.39 ms / 13 MB | 12.4 ms / 15 MB | 27.9 ms / 74 MB | n/a | n/a |
| users-broad-1k-sort-last | 11.4 ms / 15 MB | 11.0 ms / 15 MB | 15.3 ms / 12 MB | 9.12 ms / 13 MB | 13.3 ms / 15 MB | 38.9 ms / 112 MB | n/a | n/a |
| users-broad-1k-sum-score | 5.66 ms / 6.2 MB | 5.95 ms / 6.2 MB | 14.1 ms / 12 MB | 8.92 ms / 13 MB | 12.5 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-type-path | 6.19 ms / 5.6 MB | 6.86 ms / 5.6 MB | 13.8 ms / 12 MB | 8.63 ms / 13 MB | 12.3 ms / 14 MB | disagreed | n/a | n/a |
| users-broad-1k-unique-scores | 9.21 ms / 14 MB | 9.21 ms / 14 MB | 15.6 ms / 12 MB | 8.77 ms / 13 MB | 12.5 ms / 15 MB | 28.7 ms / 73 MB | n/a | n/a |
| users-broad-5k-all-nonneg | 19.2 ms / 12 MB | 18.5 ms / 12 MB | 55.7 ms / 50 MB | 27.5 ms / 51 MB | 43.2 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-any-high | 19.9 ms / 12 MB | 17.1 ms / 12 MB | 53.2 ms / 50 MB | 26.3 ms / 51 MB | 43.1 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-count | 12.0 ms / 11 MB | 9.60 ms / 11 MB | 52.4 ms / 50 MB | 24.7 ms / 51 MB | 42.9 ms / 41 MB | 102 ms / 223 MB | n/a | n/a |
| users-broad-5k-descent | 29.2 ms / 42 MB | 28.8 ms / 42 MB | 92.6 ms / 74 MB | 42.1 ms / 63 MB | 123 ms / 77 MB | 346 ms / 866 MB | n/a | n/a |
| users-broad-5k-filter-active | 13.1 ms / 13 MB | 10.8 ms / 13 MB | 55.3 ms / 50 MB | 27.3 ms / 51 MB | 43.0 ms / 42 MB | 135 ms / 329 MB | n/a | n/a |
| users-broad-5k-first-id | 18.1 ms / 9.9 MB | 12.7 ms / 9.9 MB | 53.2 ms / 50 MB | 24.6 ms / 51 MB | 41.5 ms / 41 MB | 99.2 ms / 218 MB | n/a | n/a |
| users-broad-5k-group-mod | 39.3 ms / 64 MB | 38.9 ms / 64 MB | 58.7 ms / 50 MB | 29.8 ms / 52 MB | 43.9 ms / 41 MB | 191 ms / 503 MB | n/a | n/a |
| users-broad-5k-high-score | 14.4 ms / 13 MB | 11.6 ms / 13 MB | 55.1 ms / 50 MB | 27.0 ms / 51 MB | 45.0 ms / 41 MB | 145 ms / 380 MB | n/a | n/a |
| users-broad-5k-identity | 48.4 ms / 66 MB | 48.6 ms / 66 MB | 167 ms / 55 MB | 43.2 ms / 51 MB | disagreed | 253 ms / 378 MB | n/a | n/a |
| users-broad-5k-ids | 15.5 ms / 11 MB | 13.7 ms / 11 MB | 55.1 ms / 50 MB | 24.7 ms / 51 MB | 41.6 ms / 42 MB | 108 ms / 239 MB | n/a | n/a |
| users-broad-5k-keys-len | 15.3 ms / 10 MB | 13.8 ms / 10 MB | 54.1 ms / 50 MB | 26.8 ms / 51 MB | 43.9 ms / 41 MB | 99.4 ms / 223 MB | n/a | n/a |
| users-broad-5k-keys-publish | 15.0 ms / 10 MB | 12.7 ms / 10 MB | 52.6 ms / 50 MB | 24.8 ms / 51 MB | 41.4 ms / 41 MB | disagreed | n/a | n/a |
| users-broad-5k-max-score | 19.8 ms / 12 MB | 18.1 ms / 12 MB | 55.1 ms / 50 MB | 25.6 ms / 51 MB | 42.6 ms / 41 MB | 110 ms / 231 MB | n/a | n/a |
| users-broad-5k-nested-dept | 15.3 ms / 9.9 MB | 13.9 ms / 9.9 MB | 53.4 ms / 50 MB | 24.3 ms / 51 MB | 41.3 ms / 41 MB | 99.3 ms / 219 MB | n/a | n/a |
| users-broad-5k-project-names | 19.2 ms / 11 MB | 14.0 ms / 11 MB | 54.7 ms / 50 MB | 25.6 ms / 51 MB | 42.7 ms / 41 MB | 109 ms / 230 MB | n/a | n/a |
| users-broad-5k-project-pair | 20.9 ms / 12 MB | 20.1 ms / 12 MB | 59.5 ms / 52 MB | 28.7 ms / 51 MB | 44.3 ms / 43 MB | 170 ms / 321 MB | n/a | n/a |
| users-broad-5k-reduce-score | 12.7 ms / 12 MB | 10.5 ms / 12 MB | 54.6 ms / 50 MB | 25.4 ms / 51 MB | 42.5 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-reverse-id | 36.0 ms / 63 MB | 35.4 ms / 63 MB | 54.5 ms / 50 MB | 25.2 ms / 51 MB | 41.6 ms / 41 MB | 142 ms / 399 MB | n/a | n/a |
| users-broad-5k-select-id-stream | 15.8 ms / 10 MB | 15.9 ms / 10 MB | 55.8 ms / 50 MB | 28.9 ms / 51 MB | 45.5 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-slice-length | 10.6 ms / 11 MB | 10.0 ms / 11 MB | 52.3 ms / 50 MB | 23.9 ms / 51 MB | 40.9 ms / 41 MB | 102 ms / 243 MB | n/a | n/a |
| users-broad-5k-sort-last | 36.5 ms / 63 MB | 35.9 ms / 63 MB | 58.9 ms / 50 MB | 28.4 ms / 51 MB | 45.0 ms / 42 MB | 150 ms / 391 MB | n/a | n/a |
| users-broad-5k-sum-score | 16.9 ms / 12 MB | 10.7 ms / 12 MB | 54.9 ms / 50 MB | 25.0 ms / 51 MB | 42.6 ms / 42 MB | n/a | n/a | n/a |
| users-broad-5k-type-path | 18.5 ms / 9.9 MB | 12.9 ms / 9.9 MB | 53.2 ms / 50 MB | 24.2 ms / 51 MB | 42.0 ms / 42 MB | disagreed | n/a | n/a |
| users-broad-5k-unique-scores | 28.3 ms / 46 MB | 28.2 ms / 46 MB | 56.4 ms / 50 MB | 25.0 ms / 51 MB | 44.8 ms / 41 MB | 110 ms / 226 MB | n/a | n/a |
| users-broad-25k-all-nonneg | 70.2 ms / 39 MB | 70.8 ms / 39 MB | 253 ms / 237 MB | 124 ms / 239 MB | 186 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-any-high | 65.8 ms / 39 MB | 65.1 ms / 39 MB | 240 ms / 237 MB | 103 ms / 239 MB | 175 ms / 179 MB | n/a | n/a | n/a |
| users-broad-25k-count | 29.8 ms / 36 MB | 29.6 ms / 36 MB | 238 ms / 237 MB | 106 ms / 239 MB | 175 ms / 179 MB | 441 ms / 941 MB | n/a | n/a |
| users-broad-25k-descent | 117 ms / 189 MB | 122 ms / 189 MB | 448 ms / 371 MB | 188 ms / 299 MB | 574 ms / 386 MB | excluded | n/a | n/a |
| users-broad-25k-filter-active | 36.8 ms / 41 MB | 36.3 ms / 41 MB | 255 ms / 237 MB | 119 ms / 239 MB | 186 ms / 182 MB | excluded | n/a | n/a |
| users-broad-25k-first-id | 46.2 ms / 31 MB | 45.5 ms / 31 MB | 242 ms / 237 MB | 108 ms / 238 MB | 181 ms / 178 MB | 450 ms / 955 MB | n/a | n/a |
| users-broad-25k-group-mod | 179 ms / 267 MB | 175 ms / 237 MB | 273 ms / 239 MB | 132 ms / 243 MB | 191 ms / 186 MB | 872 ms / 2213 MB | n/a | n/a |
| users-broad-25k-high-score | 37.9 ms / 42 MB | 36.6 ms / 41 MB | 253 ms / 237 MB | 119 ms / 239 MB | 184 ms / 183 MB | excluded | n/a | n/a |
| users-broad-25k-identity | 214 ms / 273 MB | 212 ms / 261 MB | 708 ms / 266 MB | 197 ms / 238 MB | disagreed | excluded | n/a | n/a |
| users-broad-25k-ids | 49.9 ms / 34 MB | 50.0 ms / 34 MB | 251 ms / 237 MB | 107 ms / 239 MB | 185 ms / 183 MB | 489 ms / 994 MB | n/a | n/a |
| users-broad-25k-keys-len | 45.1 ms / 32 MB | 45.0 ms / 32 MB | 241 ms / 237 MB | 103 ms / 239 MB | 177 ms / 179 MB | 442 ms / 940 MB | n/a | n/a |
| users-broad-25k-keys-publish | 44.8 ms / 32 MB | 45.0 ms / 32 MB | 246 ms / 237 MB | 107 ms / 239 MB | 182 ms / 179 MB | disagreed | n/a | n/a |
| users-broad-25k-max-score | 68.8 ms / 41 MB | 69.2 ms / 41 MB | 250 ms / 237 MB | 111 ms / 239 MB | 182 ms / 183 MB | 485 ms / 1008 MB | n/a | n/a |
| users-broad-25k-nested-dept | 46.7 ms / 31 MB | 45.8 ms / 31 MB | 240 ms / 237 MB | 106 ms / 239 MB | 180 ms / 178 MB | 445 ms / 940 MB | n/a | n/a |
| users-broad-25k-project-names | 52.2 ms / 36 MB | 52.0 ms / 36 MB | 250 ms / 237 MB | 108 ms / 239 MB | 186 ms / 183 MB | 498 ms / 995 MB | n/a | n/a |
| users-broad-25k-project-pair | 72.8 ms / 40 MB | 73.0 ms / 40 MB | 277 ms / 248 MB | 123 ms / 239 MB | 193 ms / 196 MB | 784 ms / 1426 MB | n/a | n/a |
| users-broad-25k-reduce-score | 35.4 ms / 37 MB | 35.5 ms / 38 MB | 255 ms / 237 MB | 110 ms / 239 MB | 184 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-reverse-id | 148 ms / 264 MB | 155 ms / 235 MB | 247 ms / 237 MB | 107 ms / 239 MB | 177 ms / 179 MB | 647 ms / 1758 MB | n/a | n/a |
| users-broad-25k-select-id-stream | 50.7 ms / 33 MB | 50.4 ms / 33 MB | 254 ms / 237 MB | 127 ms / 239 MB | 203 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-slice-length | 29.8 ms / 36 MB | 29.6 ms / 36 MB | 239 ms / 237 MB | 104 ms / 239 MB | 183 ms / 178 MB | 461 ms / 1048 MB | n/a | n/a |
| users-broad-25k-sort-last | 166 ms / 265 MB | 166 ms / 238 MB | 284 ms / 239 MB | 133 ms / 241 MB | 202 ms / 190 MB | 699 ms / 1809 MB | n/a | n/a |
| users-broad-25k-sum-score | 35.4 ms / 37 MB | 35.6 ms / 38 MB | 254 ms / 237 MB | 109 ms / 239 MB | 184 ms / 183 MB | n/a | n/a | n/a |
| users-broad-25k-type-path | 45.5 ms / 31 MB | 44.7 ms / 32 MB | 244 ms / 237 MB | 107 ms / 239 MB | 179 ms / 179 MB | disagreed | n/a | n/a |
| users-broad-25k-unique-scores | 121 ms / 192 MB | 121 ms / 201 MB | 254 ms / 238 MB | 107 ms / 239 MB | 198 ms / 188 MB | 488 ms / 993 MB | n/a | n/a |
| users-broad-50k-all-nonneg | 136 ms / 73 MB | 137 ms / 73 MB | 495 ms / 472 MB | 238 ms / 475 MB | 360 ms / 354 MB | n/a | n/a | n/a |
| users-broad-50k-any-high | 124 ms / 73 MB | 124 ms / 73 MB | 482 ms / 472 MB | 208 ms / 475 MB | 348 ms / 351 MB | n/a | n/a | n/a |
| users-broad-50k-count | 54.7 ms / 64 MB | 54.5 ms / 64 MB | 485 ms / 472 MB | 205 ms / 474 MB | 341 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-descent | 232 ms / 365 MB | 233 ms / 365 MB | 878 ms / 764 MB | 373 ms / 591 MB | 1156 ms / 744 MB | excluded | n/a | n/a |
| users-broad-50k-filter-active | 71.3 ms / 78 MB | 71.0 ms / 78 MB | 502 ms / 473 MB | 233 ms / 475 MB | 352 ms / 356 MB | excluded | n/a | n/a |
| users-broad-50k-first-id | 83.0 ms / 58 MB | 83.9 ms / 58 MB | 482 ms / 472 MB | 201 ms / 473 MB | 351 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-group-mod | 358 ms / 492 MB | 351 ms / 492 MB | 540 ms / 476 MB | 259 ms / 481 MB | 371 ms / 365 MB | 1788 ms / 4303 MB | n/a | n/a |
| users-broad-50k-high-score | 70.2 ms / 78 MB | 70.4 ms / 78 MB | 509 ms / 473 MB | 237 ms / 475 MB | 363 ms / 358 MB | excluded | n/a | n/a |
| users-broad-50k-identity | 436 ms / 499 MB | 427 ms / 499 MB | 1396 ms / 529 MB | 386 ms / 473 MB | disagreed | excluded | n/a | n/a |
| users-broad-50k-ids | 93.2 ms / 64 MB | 96.1 ms / 64 MB | 496 ms / 473 MB | 204 ms / 474 MB | 353 ms / 358 MB | 959 ms / 1931 MB | n/a | n/a |
| users-broad-50k-keys-len | 83.0 ms / 59 MB | 84.6 ms / 59 MB | 485 ms / 472 MB | 205 ms / 474 MB | 348 ms / 349 MB | 877 ms / 1859 MB | n/a | n/a |
| users-broad-50k-keys-publish | 84.4 ms / 59 MB | 83.0 ms / 59 MB | 475 ms / 472 MB | 198 ms / 474 MB | 350 ms / 349 MB | disagreed | n/a | n/a |
| users-broad-50k-max-score | 136 ms / 78 MB | 132 ms / 78 MB | 489 ms / 473 MB | 211 ms / 474 MB | 359 ms / 358 MB | 970 ms / 1963 MB | n/a | n/a |
| users-broad-50k-nested-dept | 83.1 ms / 58 MB | 82.9 ms / 58 MB | 477 ms / 472 MB | 199 ms / 473 MB | 344 ms / 349 MB | 888 ms / 1827 MB | n/a | n/a |
| users-broad-50k-project-names | 94.4 ms / 66 MB | 96.0 ms / 66 MB | 498 ms / 473 MB | 216 ms / 474 MB | 359 ms / 358 MB | 966 ms / 1936 MB | n/a | n/a |
| users-broad-50k-project-pair | 140 ms / 75 MB | 139 ms / 75 MB | 541 ms / 494 MB | 242 ms / 474 MB | 381 ms / 383 MB | 1548 ms / 2693 MB | n/a | n/a |
| users-broad-50k-reduce-score | 66.3 ms / 70 MB | 66.7 ms / 70 MB | 489 ms / 472 MB | 210 ms / 474 MB | 357 ms / 354 MB | n/a | n/a | n/a |
| users-broad-50k-reverse-id | 301 ms / 486 MB | 298 ms / 486 MB | 489 ms / 473 MB | 210 ms / 474 MB | 343 ms / 350 MB | 1274 ms / 3441 MB | n/a | n/a |
| users-broad-50k-select-id-stream | 98.9 ms / 62 MB | 98.7 ms / 62 MB | 502 ms / 473 MB | 244 ms / 474 MB | 392 ms / 353 MB | n/a | n/a | n/a |
| users-broad-50k-slice-length | 53.0 ms / 64 MB | 53.1 ms / 64 MB | 476 ms / 472 MB | 204 ms / 474 MB | 350 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-sort-last | 333 ms / 487 MB | 330 ms / 487 MB | 562 ms / 477 MB | 257 ms / 478 MB | 407 ms / 372 MB | 1419 ms / 3483 MB | n/a | n/a |
| users-broad-50k-sum-score | 68.7 ms / 70 MB | 66.2 ms / 70 MB | 490 ms / 473 MB | 208 ms / 474 MB | 352 ms / 358 MB | n/a | n/a | n/a |
| users-broad-50k-type-path | 83.0 ms / 58 MB | 85.2 ms / 58 MB | 475 ms / 472 MB | 201 ms / 473 MB | 342 ms / 349 MB | disagreed | n/a | n/a |
| users-broad-50k-unique-scores | 239 ms / 379 MB | 238 ms / 379 MB | 502 ms / 473 MB | 205 ms / 474 MB | 419 ms / 369 MB | 962 ms / 1922 MB | n/a | n/a |
| users-broad-100k-all-nonneg | 268 ms / 143 MB | 263 ms / 143 MB | 978 ms / 943 MB | 472 ms / 945 MB | 709 ms / 698 MB | n/a | n/a | n/a |
| users-broad-100k-any-high | 245 ms / 143 MB | 243 ms / 143 MB | 946 ms / 943 MB | 402 ms / 945 MB | 675 ms / 692 MB | n/a | n/a | n/a |
| users-broad-100k-count | 100 ms / 123 MB | 101 ms / 123 MB | 938 ms / 943 MB | 402 ms / 943 MB | 681 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-descent | 452 ms / 703 MB | 452 ms / 703 MB | 1754 ms / 1583 MB | 735 ms / 1180 MB | 2300 ms / 1555 MB | excluded | n/a | n/a |
| users-broad-100k-filter-active | 132 ms / 150 MB | 130 ms / 150 MB | 1016 ms / 943 MB | 471 ms / 946 MB | 715 ms / 704 MB | excluded | n/a | n/a |
| users-broad-100k-first-id | 160 ms / 112 MB | 159 ms / 112 MB | 947 ms / 943 MB | 397 ms / 943 MB | 694 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-group-mod | 689 ms / 913 MB | 675 ms / 913 MB | 1090 ms / 952 MB | 518 ms / 959 MB | 739 ms / 724 MB | 3532 ms / 8722 MB | n/a | n/a |
| users-broad-100k-high-score | 133 ms / 152 MB | 135 ms / 152 MB | 996 ms / 943 MB | 460 ms / 946 MB | 718 ms / 706 MB | excluded | n/a | n/a |
| users-broad-100k-identity | 846 ms / 977 MB | 828 ms / 977 MB | 2740 ms / 1057 MB | 775 ms / 943 MB | disagreed | excluded | n/a | n/a |
| users-broad-100k-ids | 183 ms / 123 MB | 180 ms / 123 MB | 990 ms / 944 MB | 410 ms / 944 MB | 696 ms / 706 MB | 1913 ms / 3854 MB | n/a | n/a |
| users-broad-100k-keys-len | 163 ms / 113 MB | 159 ms / 113 MB | 945 ms / 943 MB | 397 ms / 944 MB | 661 ms / 689 MB | 1755 ms / 3667 MB | n/a | n/a |
| users-broad-100k-keys-publish | 160 ms / 113 MB | 163 ms / 113 MB | 939 ms / 943 MB | 401 ms / 943 MB | 707 ms / 689 MB | disagreed | n/a | n/a |
| users-broad-100k-max-score | 261 ms / 150 MB | 257 ms / 150 MB | 973 ms / 944 MB | 433 ms / 944 MB | 718 ms / 707 MB | 1915 ms / 3925 MB | n/a | n/a |
| users-broad-100k-nested-dept | 164 ms / 112 MB | 160 ms / 112 MB | 944 ms / 943 MB | 402 ms / 943 MB | 679 ms / 688 MB | 1764 ms / 3666 MB | n/a | n/a |
| users-broad-100k-project-names | 192 ms / 127 MB | 184 ms / 127 MB | 1012 ms / 944 MB | 427 ms / 944 MB | 717 ms / 706 MB | 1916 ms / 3858 MB | n/a | n/a |
| users-broad-100k-project-pair | 274 ms / 143 MB | 271 ms / 143 MB | 1058 ms / 988 MB | 488 ms / 944 MB | 738 ms / 758 MB | 3097 ms / 5316 MB | n/a | n/a |
| users-broad-100k-reduce-score | 127 ms / 136 MB | 126 ms / 136 MB | 988 ms / 943 MB | 424 ms / 945 MB | 718 ms / 697 MB | n/a | n/a | n/a |
| users-broad-100k-reverse-id | 593 ms / 906 MB | 602 ms / 905 MB | 974 ms / 944 MB | 428 ms / 943 MB | 672 ms / 691 MB | 2574 ms / 6972 MB | n/a | n/a |
| users-broad-100k-select-id-stream | 187 ms / 119 MB | 188 ms / 119 MB | 1029 ms / 945 MB | 499 ms / 943 MB | 800 ms / 697 MB | n/a | n/a | n/a |
| users-broad-100k-slice-length | 103 ms / 123 MB | 102 ms / 123 MB | 931 ms / 943 MB | 411 ms / 944 MB | 695 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-sort-last | 657 ms / 915 MB | 657 ms / 915 MB | 1146 ms / 951 MB | 520 ms / 953 MB | 818 ms / 734 MB | 2848 ms / 7039 MB | n/a | n/a |
| users-broad-100k-sum-score | 133 ms / 136 MB | 129 ms / 136 MB | 992 ms / 944 MB | 421 ms / 944 MB | 712 ms / 707 MB | n/a | n/a | n/a |
| users-broad-100k-type-path | 167 ms / 112 MB | 162 ms / 112 MB | 933 ms / 943 MB | 392 ms / 943 MB | 682 ms / 689 MB | disagreed | n/a | n/a |
| users-broad-100k-unique-scores | 470 ms / 724 MB | 474 ms / 724 MB | 1029 ms / 946 MB | 415 ms / 944 MB | 820 ms / 729 MB | 1927 ms / 3833 MB | n/a | n/a |
| users-broad-200k-all-nonneg | 519 ms / 281 MB | 521 ms / 281 MB | 1974 ms / 1882 MB | 932 ms / 1887 MB | 1401 ms / 1384 MB | n/a | n/a | n/a |
| users-broad-200k-any-high | 473 ms / 281 MB | 474 ms / 281 MB | 1867 ms / 1882 MB | 786 ms / 1887 MB | 1337 ms / 1376 MB | n/a | n/a | n/a |
| users-broad-200k-count | 192 ms / 241 MB | 192 ms / 241 MB | 1893 ms / 1882 MB | 809 ms / 1884 MB | 1341 ms / 1368 MB | excluded | n/a | n/a |
| users-broad-200k-descent | 899 ms / 1406 MB | 887 ms / 1406 MB | 3509 ms / 2923 MB | 1506 ms / 2357 MB | 4582 ms / 2866 MB | excluded | n/a | n/a |
| users-broad-200k-filter-active | 268 ms / 292 MB | 257 ms / 292 MB | 2001 ms / 1882 MB | 919 ms / 1889 MB | 1422 ms / 1399 MB | excluded | n/a | n/a |
| users-broad-200k-first-id | 313 ms / 220 MB | 309 ms / 220 MB | 1880 ms / 1882 MB | 801 ms / 1884 MB | 1346 ms / 1368 MB | excluded | n/a | n/a |
| users-broad-200k-group-mod | 1348 ms / 1803 MB | 1351 ms / 1803 MB | 2157 ms / 1899 MB | 1021 ms / 1913 MB | 1463 ms / 1439 MB | 6986 ms / 17516 MB | n/a | n/a |
| users-broad-200k-high-score | 256 ms / 292 MB | 257 ms / 292 MB | 1980 ms / 1884 MB | 931 ms / 1889 MB | 1446 ms / 1403 MB | excluded | n/a | n/a |
| users-broad-200k-identity | 1684 ms / 1934 MB | 1655 ms / 1934 MB | 5314 ms / 2110 MB | 1520 ms / 1884 MB | disagreed | excluded | n/a | n/a |
| users-broad-200k-ids | 353 ms / 248 MB | 358 ms / 248 MB | 1968 ms / 1885 MB | 805 ms / 1884 MB | 1395 ms / 1402 MB | 3849 ms / 7676 MB | n/a | n/a |
| users-broad-200k-keys-len | 308 ms / 221 MB | 307 ms / 221 MB | 1824 ms / 1882 MB | 792 ms / 1884 MB | 1363 ms / 1368 MB | 3434 ms / 7295 MB | n/a | n/a |
| users-broad-200k-keys-publish | 311 ms / 221 MB | 308 ms / 221 MB | 1858 ms / 1882 MB | 770 ms / 1884 MB | 1318 ms / 1368 MB | disagreed | n/a | n/a |
| users-broad-200k-max-score | 505 ms / 286 MB | 502 ms / 286 MB | 1974 ms / 1885 MB | 826 ms / 1884 MB | 1376 ms / 1405 MB | 3826 ms / 7832 MB | n/a | n/a |
| users-broad-200k-nested-dept | 310 ms / 220 MB | 313 ms / 221 MB | 1865 ms / 1882 MB | 782 ms / 1884 MB | 1326 ms / 1368 MB | 3444 ms / 7288 MB | n/a | n/a |
| users-broad-200k-project-names | 357 ms / 249 MB | 357 ms / 249 MB | 1993 ms / 1885 MB | 845 ms / 1884 MB | 1402 ms / 1403 MB | 3880 ms / 7700 MB | n/a | n/a |
| users-broad-200k-project-pair | 535 ms / 291 MB | 534 ms / 291 MB | 2143 ms / 1972 MB | 958 ms / 1887 MB | 1456 ms / 1506 MB | 6134 ms / 10716 MB | n/a | n/a |
| users-broad-200k-reduce-score | 253 ms / 264 MB | 247 ms / 263 MB | 1956 ms / 1882 MB | 839 ms / 1887 MB | 1423 ms / 1387 MB | n/a | n/a | n/a |
| users-broad-200k-reverse-id | 1133 ms / 1803 MB | 1152 ms / 1803 MB | 1919 ms / 1885 MB | 819 ms / 1884 MB | 1342 ms / 1372 MB | 5032 ms / 13848 MB | n/a | n/a |
| users-broad-200k-select-id-stream | 356 ms / 233 MB | 353 ms / 233 MB | 2018 ms / 1886 MB | 960 ms / 1884 MB | 1534 ms / 1384 MB | n/a | n/a | n/a |
| users-broad-200k-slice-length | 197 ms / 241 MB | 193 ms / 241 MB | 1871 ms / 1882 MB | 804 ms / 1884 MB | 1354 ms / 1369 MB | excluded | n/a | n/a |
| users-broad-200k-sort-last | 1317 ms / 1828 MB | 1316 ms / 1828 MB | 2355 ms / 1899 MB | 1028 ms / 1902 MB | 1697 ms / 1459 MB | 6016 ms / 13369 MB | n/a | n/a |
| users-broad-200k-sum-score | 245 ms / 264 MB | 253 ms / 263 MB | 1986 ms / 1885 MB | 817 ms / 1884 MB | 1416 ms / 1406 MB | n/a | n/a | n/a |
| users-broad-200k-type-path | 312 ms / 221 MB | 308 ms / 221 MB | 1883 ms / 1882 MB | 798 ms / 1884 MB | 1345 ms / 1368 MB | disagreed | n/a | n/a |
| users-broad-200k-unique-scores | 919 ms / 1446 MB | 940 ms / 1446 MB | 2060 ms / 1885 MB | 817 ms / 1884 MB | 1677 ms / 1449 MB | 3814 ms / 7661 MB | n/a | n/a |
| users-narrow-100-all-nonneg | 3.09 ms / 4.9 MB | 3.63 ms / 4.9 MB | 3.22 ms / 2.7 MB | 3.85 ms / 4.3 MB | 3.20 ms / 6.5 MB | n/a | n/a | n/a |
| users-narrow-100-any-high | 3.66 ms / 4.9 MB | 3.17 ms / 4.9 MB | 2.94 ms / 2.7 MB | 3.16 ms / 4.2 MB | 2.97 ms / 6.1 MB | n/a | n/a | n/a |
| users-narrow-100-count | 3.34 ms / 4.5 MB | 3.01 ms / 4.5 MB | 2.88 ms / 2.6 MB | 2.87 ms / 4.0 MB | 3.14 ms / 6.1 MB | 6.32 ms / 32 MB | n/a | n/a |
| users-narrow-100-descent | 3.13 ms / 4.8 MB | 3.60 ms / 4.8 MB | 3.72 ms / 2.7 MB | 3.39 ms / 4.0 MB | 3.29 ms / 6.1 MB | 6.89 ms / 34 MB | n/a | n/a |
| users-narrow-100-filter-active | 3.40 ms / 4.8 MB | 3.37 ms / 4.8 MB | 3.12 ms / 2.6 MB | 3.64 ms / 4.0 MB | 2.92 ms / 6.0 MB | 6.37 ms / 29 MB | n/a | n/a |
| users-narrow-100-first-id | 2.91 ms / 4.5 MB | 2.93 ms / 4.5 MB | 3.10 ms / 2.6 MB | 2.92 ms / 3.9 MB | 2.91 ms / 5.9 MB | 5.76 ms / 22 MB | n/a | n/a |
| users-narrow-100-group-mod | 3.05 ms / 5.0 MB | 3.68 ms / 5.0 MB | 4.82 ms / 2.8 MB | 3.14 ms / 4.2 MB | 3.20 ms / 6.1 MB | 6.97 ms / 34 MB | n/a | n/a |
| users-narrow-100-high-score | 3.83 ms / 4.8 MB | 3.17 ms / 4.8 MB | 3.00 ms / 2.6 MB | 3.18 ms / 4.0 MB | 3.08 ms / 6.0 MB | 6.98 ms / 33 MB | n/a | n/a |
| users-narrow-100-identity | 3.18 ms / 4.3 MB | 3.67 ms / 4.3 MB | 3.17 ms / 2.6 MB | 3.57 ms / 3.9 MB | 3.15 ms / 6.0 MB | 6.87 ms / 32 MB | n/a | n/a |
| users-narrow-100-ids | 2.93 ms / 4.6 MB | 2.89 ms / 4.6 MB | 2.89 ms / 2.7 MB | 3.89 ms / 3.9 MB | 3.24 ms / 6.0 MB | 6.20 ms / 28 MB | n/a | n/a |
| users-narrow-100-keys-len | 3.46 ms / 4.6 MB | 3.31 ms / 4.7 MB | 3.03 ms / 2.7 MB | 3.33 ms / 4.1 MB | 3.07 ms / 5.9 MB | 6.23 ms / 28 MB | n/a | n/a |
| users-narrow-100-keys-publish | 2.98 ms / 4.6 MB | 3.01 ms / 4.6 MB | 2.90 ms / 2.6 MB | 3.56 ms / 4.0 MB | 4.15 ms / 6.0 MB | 6.56 ms / 30 MB | n/a | n/a |
| users-narrow-100-max-score | 3.00 ms / 4.8 MB | 3.02 ms / 4.8 MB | 3.02 ms / 2.7 MB | 3.55 ms / 4.1 MB | 3.34 ms / 5.9 MB | 6.70 ms / 29 MB | n/a | n/a |
| users-narrow-100-nested-dept | 3.04 ms / 4.5 MB | 3.33 ms / 4.5 MB | 3.37 ms / 2.6 MB | 3.06 ms / 3.9 MB | 3.08 ms / 5.8 MB | 6.37 ms / 26 MB | n/a | n/a |
| users-narrow-100-project-names | 3.15 ms / 4.5 MB | 3.90 ms / 4.6 MB | 3.04 ms / 2.6 MB | 3.28 ms / 3.9 MB | 3.89 ms / 5.9 MB | 7.65 ms / 32 MB | n/a | n/a |
| users-narrow-100-project-pair | 3.03 ms / 4.7 MB | 2.97 ms / 4.7 MB | 2.90 ms / 2.7 MB | 3.27 ms / 3.9 MB | 3.54 ms / 5.9 MB | 7.73 ms / 36 MB | n/a | n/a |
| users-narrow-100-reduce-score | 3.86 ms / 4.8 MB | 3.53 ms / 4.9 MB | 3.57 ms / 2.7 MB | 3.06 ms / 3.9 MB | 2.99 ms / 5.9 MB | n/a | n/a | n/a |
| users-narrow-100-reverse-id | 3.01 ms / 4.8 MB | 2.91 ms / 4.8 MB | 3.46 ms / 2.7 MB | 3.32 ms / 4.0 MB | 3.14 ms / 6.0 MB | 5.98 ms / 17 MB | n/a | n/a |
| users-narrow-100-select-id-stream | 3.11 ms / 4.5 MB | 3.46 ms / 4.5 MB | 3.04 ms / 2.6 MB | 3.44 ms / 3.9 MB | 3.11 ms / 5.9 MB | n/a | n/a | n/a |
| users-narrow-100-slice-length | 3.38 ms / 4.6 MB | 2.80 ms / 4.6 MB | 2.85 ms / 2.6 MB | 3.12 ms / 4.0 MB | 3.15 ms / 6.0 MB | 6.64 ms / 32 MB | n/a | n/a |
| users-narrow-100-sort-last | 3.01 ms / 4.8 MB | 3.71 ms / 4.8 MB | 3.14 ms / 2.7 MB | 3.26 ms / 4.2 MB | 3.11 ms / 6.0 MB | 6.61 ms / 31 MB | n/a | n/a |
| users-narrow-100-sum-score | 3.00 ms / 4.8 MB | 3.86 ms / 4.8 MB | 3.21 ms / 2.7 MB | 3.13 ms / 4.0 MB | 3.11 ms / 5.9 MB | n/a | n/a | n/a |
| users-narrow-100-type-path | 3.00 ms / 4.5 MB | 3.69 ms / 4.5 MB | 3.17 ms / 2.6 MB | 3.19 ms / 3.9 MB | 3.06 ms / 5.8 MB | disagreed | n/a | n/a |
| users-narrow-100-unique-scores | 2.93 ms / 5.0 MB | 2.93 ms / 5.0 MB | 2.81 ms / 2.7 MB | 3.84 ms / 4.1 MB | 3.60 ms / 5.8 MB | 6.14 ms / 28 MB | n/a | n/a |
| users-narrow-1k-all-nonneg | 4.07 ms / 5.3 MB | 3.75 ms / 5.3 MB | 3.92 ms / 3.3 MB | 3.74 ms / 4.6 MB | 3.73 ms / 6.7 MB | n/a | n/a | n/a |
| users-narrow-1k-any-high | 3.24 ms / 5.3 MB | 3.28 ms / 5.3 MB | 3.34 ms / 3.3 MB | 3.73 ms / 4.6 MB | 3.51 ms / 6.7 MB | n/a | n/a | n/a |
| users-narrow-1k-count | 3.75 ms / 4.8 MB | 4.11 ms / 4.8 MB | 3.83 ms / 3.3 MB | 3.40 ms / 4.3 MB | 3.48 ms / 6.4 MB | 7.98 ms / 35 MB | n/a | n/a |
| users-narrow-1k-descent | 6.55 ms / 5.4 MB | 4.48 ms / 5.4 MB | 4.23 ms / 3.6 MB | 3.50 ms / 4.4 MB | 4.30 ms / 7.5 MB | 12.2 ms / 46 MB | n/a | n/a |
| users-narrow-1k-filter-active | 4.23 ms / 5.0 MB | 3.62 ms / 5.0 MB | 4.38 ms / 3.3 MB | 3.82 ms / 4.3 MB | 3.96 ms / 6.5 MB | 9.10 ms / 37 MB | n/a | n/a |
| users-narrow-1k-first-id | 3.15 ms / 4.5 MB | 3.36 ms / 4.5 MB | 3.55 ms / 3.3 MB | 3.36 ms / 4.2 MB | 3.35 ms / 6.5 MB | 7.49 ms / 35 MB | n/a | n/a |
| users-narrow-1k-group-mod | 4.71 ms / 5.9 MB | 4.23 ms / 6.0 MB | 4.77 ms / 3.5 MB | 3.42 ms / 4.7 MB | 3.97 ms / 6.8 MB | 10.3 ms / 42 MB | n/a | n/a |
| users-narrow-1k-high-score | 3.71 ms / 5.3 MB | 3.52 ms / 5.3 MB | 3.80 ms / 3.4 MB | 3.45 ms / 4.3 MB | 3.49 ms / 6.5 MB | 9.15 ms / 40 MB | n/a | n/a |
| users-narrow-1k-identity | 3.32 ms / 4.4 MB | 3.22 ms / 4.4 MB | 3.87 ms / 3.3 MB | 3.49 ms / 4.2 MB | 3.99 ms / 6.6 MB | 9.81 ms / 36 MB | n/a | n/a |
| users-narrow-1k-ids | 3.41 ms / 4.7 MB | 3.32 ms / 4.7 MB | 4.81 ms / 3.4 MB | 3.53 ms / 4.2 MB | 3.70 ms / 6.6 MB | 8.34 ms / 37 MB | n/a | n/a |
| users-narrow-1k-keys-len | 3.70 ms / 4.7 MB | 3.67 ms / 4.7 MB | 3.72 ms / 3.3 MB | 3.70 ms / 4.4 MB | 3.84 ms / 6.4 MB | 8.17 ms / 35 MB | n/a | n/a |
| users-narrow-1k-keys-publish | 3.20 ms / 4.6 MB | 3.08 ms / 4.6 MB | 4.05 ms / 3.3 MB | 4.29 ms / 4.4 MB | 3.75 ms / 6.4 MB | 8.81 ms / 19 MB | n/a | n/a |
| users-narrow-1k-max-score | 3.59 ms / 5.2 MB | 3.67 ms / 5.3 MB | 4.18 ms / 3.4 MB | 3.76 ms / 4.4 MB | 3.65 ms / 6.5 MB | 8.73 ms / 38 MB | n/a | n/a |
| users-narrow-1k-nested-dept | 3.78 ms / 4.5 MB | 3.82 ms / 4.5 MB | 3.68 ms / 3.3 MB | 4.04 ms / 4.2 MB | 3.46 ms / 6.4 MB | 7.73 ms / 35 MB | n/a | n/a |
| users-narrow-1k-project-names | 3.71 ms / 4.6 MB | 3.34 ms / 4.6 MB | 3.94 ms / 3.4 MB | 3.76 ms / 4.2 MB | 4.01 ms / 6.5 MB | 8.83 ms / 38 MB | n/a | n/a |
| users-narrow-1k-project-pair | 3.41 ms / 5.1 MB | 3.32 ms / 5.1 MB | 4.83 ms / 3.9 MB | 4.55 ms / 4.3 MB | 3.81 ms / 7.1 MB | 20.4 ms / 64 MB | n/a | n/a |
| users-narrow-1k-reduce-score | 3.59 ms / 5.2 MB | 3.66 ms / 5.2 MB | 4.13 ms / 3.3 MB | 4.01 ms / 4.2 MB | 3.86 ms / 6.4 MB | n/a | n/a | n/a |
| users-narrow-1k-reverse-id | 3.51 ms / 5.6 MB | 3.85 ms / 5.7 MB | 4.52 ms / 3.5 MB | 3.76 ms / 4.3 MB | 3.75 ms / 6.5 MB | 9.02 ms / 37 MB | n/a | n/a |
| users-narrow-1k-select-id-stream | 3.23 ms / 4.5 MB | 3.06 ms / 4.5 MB | 3.54 ms / 3.3 MB | 3.94 ms / 4.2 MB | 3.65 ms / 6.6 MB | n/a | n/a | n/a |
| users-narrow-1k-slice-length | 3.67 ms / 4.8 MB | 3.88 ms / 4.9 MB | 3.53 ms / 3.3 MB | 3.39 ms / 4.3 MB | 3.45 ms / 6.4 MB | 8.41 ms / 35 MB | n/a | n/a |
| users-narrow-1k-sort-last | 6.97 ms / 5.8 MB | 3.91 ms / 5.8 MB | 4.72 ms / 3.4 MB | 3.31 ms / 4.5 MB | 3.96 ms / 6.8 MB | 9.20 ms / 39 MB | n/a | n/a |
| users-narrow-1k-sum-score | 3.58 ms / 5.2 MB | 3.40 ms / 5.2 MB | 3.69 ms / 3.4 MB | 3.57 ms / 4.3 MB | 3.55 ms / 6.6 MB | n/a | n/a | n/a |
| users-narrow-1k-type-path | 3.57 ms / 4.5 MB | 3.60 ms / 4.5 MB | 3.79 ms / 3.3 MB | 3.32 ms / 4.2 MB | 4.06 ms / 6.4 MB | disagreed | n/a | n/a |
| users-narrow-1k-unique-scores | 3.43 ms / 5.7 MB | 3.49 ms / 5.8 MB | 4.35 ms / 3.5 MB | 3.79 ms / 4.6 MB | 4.02 ms / 6.8 MB | 8.41 ms / 38 MB | n/a | n/a |
| users-narrow-5k-all-nonneg | 6.66 ms / 6.7 MB | 6.90 ms / 6.7 MB | 7.52 ms / 6.0 MB | 8.88 ms / 6.4 MB | 7.95 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-any-high | 5.65 ms / 6.8 MB | 5.50 ms / 6.8 MB | 6.29 ms / 6.0 MB | 5.25 ms / 6.3 MB | 6.27 ms / 9.4 MB | n/a | n/a | n/a |
| users-narrow-5k-count | 4.78 ms / 5.8 MB | 5.06 ms / 5.9 MB | 6.46 ms / 6.0 MB | 5.09 ms / 6.0 MB | 5.80 ms / 8.3 MB | 13.9 ms / 46 MB | n/a | n/a |
| users-narrow-5k-descent | 6.24 ms / 7.3 MB | 6.52 ms / 7.3 MB | 8.84 ms / 6.8 MB | 6.72 ms / 6.6 MB | 10.9 ms / 14 MB | 26.9 ms / 79 MB | n/a | n/a |
| users-narrow-5k-filter-active | 5.47 ms / 6.1 MB | 5.46 ms / 6.1 MB | 7.38 ms / 6.0 MB | 7.49 ms / 6.0 MB | 7.50 ms / 9.9 MB | 17.2 ms / 52 MB | n/a | n/a |
| users-narrow-5k-first-id | 5.24 ms / 4.6 MB | 4.84 ms / 4.6 MB | 6.25 ms / 5.9 MB | 5.07 ms / 5.9 MB | 6.52 ms / 8.3 MB | 13.8 ms / 31 MB | n/a | n/a |
| users-narrow-5k-group-mod | 9.62 ms / 10 MB | 10.1 ms / 10 MB | 11.6 ms / 6.7 MB | 6.81 ms / 7.4 MB | 8.93 ms / 11 MB | 24.2 ms / 68 MB | n/a | n/a |
| users-narrow-5k-high-score | 6.26 ms / 7.8 MB | 6.51 ms / 7.8 MB | 7.55 ms / 6.1 MB | 7.35 ms / 6.2 MB | 7.46 ms / 10 MB | 21.3 ms / 63 MB | n/a | n/a |
| users-narrow-5k-identity | 4.93 ms / 4.5 MB | 4.71 ms / 4.5 MB | 8.41 ms / 6.2 MB | 6.55 ms / 5.9 MB | 7.63 ms / 10.0 MB | 20.6 ms / 57 MB | n/a | n/a |
| users-narrow-5k-ids | 4.86 ms / 5.3 MB | 5.34 ms / 5.3 MB | 7.34 ms / 6.0 MB | 5.71 ms / 6.1 MB | 7.18 ms / 10 MB | 17.9 ms / 53 MB | n/a | n/a |
| users-narrow-5k-keys-len | 5.06 ms / 4.8 MB | 4.93 ms / 4.8 MB | 6.54 ms / 6.0 MB | 5.38 ms / 6.1 MB | 6.57 ms / 8.3 MB | 14.0 ms / 46 MB | n/a | n/a |
| users-narrow-5k-keys-publish | 5.05 ms / 4.7 MB | 4.86 ms / 4.7 MB | 6.60 ms / 5.9 MB | 5.43 ms / 6.1 MB | 6.71 ms / 8.3 MB | 14.5 ms / 54 MB | n/a | n/a |
| users-narrow-5k-max-score | 6.18 ms / 7.0 MB | 6.72 ms / 7.0 MB | 7.72 ms / 6.1 MB | 6.55 ms / 6.3 MB | 7.50 ms / 10 MB | 18.0 ms / 52 MB | n/a | n/a |
| users-narrow-5k-nested-dept | 5.24 ms / 4.6 MB | 5.34 ms / 4.7 MB | 7.07 ms / 6.0 MB | 6.39 ms / 5.9 MB | 6.83 ms / 8.4 MB | 14.0 ms / 47 MB | n/a | n/a |
| users-narrow-5k-project-names | 4.79 ms / 5.2 MB | 4.77 ms / 5.2 MB | 7.14 ms / 6.1 MB | 5.99 ms / 6.1 MB | 7.50 ms / 10 MB | 19.1 ms / 54 MB | n/a | n/a |
| users-narrow-5k-project-pair | 5.78 ms / 6.8 MB | 5.87 ms / 6.8 MB | 10.9 ms / 8.3 MB | 9.44 ms / 6.1 MB | 8.28 ms / 12 MB | 73.6 ms / 112 MB | n/a | n/a |
| users-narrow-5k-reduce-score | 6.62 ms / 7.0 MB | 5.90 ms / 7.0 MB | 6.99 ms / 6.0 MB | 6.79 ms / 6.0 MB | 9.84 ms / 9.8 MB | n/a | n/a | n/a |
| users-narrow-5k-reverse-id | 6.18 ms / 9.2 MB | 6.33 ms / 9.2 MB | 7.33 ms / 6.1 MB | 5.26 ms / 6.0 MB | 6.41 ms / 8.4 MB | 16.4 ms / 53 MB | n/a | n/a |
| users-narrow-5k-select-id-stream | 4.93 ms / 4.6 MB | 4.90 ms / 4.6 MB | 7.71 ms / 6.0 MB | 7.93 ms / 5.9 MB | 9.76 ms / 9.9 MB | n/a | n/a | n/a |
| users-narrow-5k-slice-length | 4.79 ms / 5.9 MB | 4.73 ms / 5.9 MB | 6.44 ms / 5.9 MB | 5.12 ms / 6.0 MB | 5.63 ms / 8.5 MB | 14.0 ms / 52 MB | n/a | n/a |
| users-narrow-5k-sort-last | 6.71 ms / 9.8 MB | 7.21 ms / 9.8 MB | 11.0 ms / 6.5 MB | 6.74 ms / 6.7 MB | 10.1 ms / 11 MB | 21.4 ms / 56 MB | n/a | n/a |
| users-narrow-5k-sum-score | 6.16 ms / 7.0 MB | 6.27 ms / 7.0 MB | 8.37 ms / 6.1 MB | 6.19 ms / 6.1 MB | 7.16 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-type-path | 4.88 ms / 4.7 MB | 4.99 ms / 4.7 MB | 6.85 ms / 6.0 MB | 5.21 ms / 5.9 MB | 6.41 ms / 8.5 MB | disagreed | n/a | n/a |
| users-narrow-5k-unique-scores | 7.26 ms / 8.6 MB | 7.21 ms / 8.6 MB | 8.56 ms / 6.1 MB | 7.19 ms / 7.0 MB | 10.0 ms / 11 MB | 17.2 ms / 52 MB | n/a | n/a |
| users-narrow-25k-all-nonneg | 11.7 ms / 14 MB | 11.1 ms / 14 MB | 16.8 ms / 19 MB | 17.3 ms / 16 MB | 16.1 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-any-high | 7.79 ms / 14 MB | 7.88 ms / 14 MB | 11.5 ms / 19 MB | 7.45 ms / 16 MB | 10.7 ms / 20 MB | n/a | n/a | n/a |
| users-narrow-25k-count | 7.03 ms / 9.8 MB | 7.37 ms / 9.8 MB | 14.3 ms / 19 MB | 9.33 ms / 15 MB | 12.7 ms / 19 MB | 33.2 ms / 83 MB | n/a | n/a |
| users-narrow-25k-descent | 11.3 ms / 20 MB | 10.6 ms / 20 MB | 24.3 ms / 24 MB | 12.2 ms / 18 MB | 30.2 ms / 34 MB | 86.8 ms / 271 MB | n/a | n/a |
| users-narrow-25k-filter-active | 8.47 ms / 10 MB | 7.93 ms / 10 MB | 18.7 ms / 19 MB | 18.3 ms / 15 MB | 14.1 ms / 21 MB | 47.1 ms / 121 MB | n/a | n/a |
| users-narrow-25k-first-id | 6.97 ms / 5.1 MB | 6.87 ms / 5.1 MB | 13.3 ms / 19 MB | 9.20 ms / 15 MB | 12.7 ms / 19 MB | 32.3 ms / 83 MB | n/a | n/a |
| users-narrow-25k-group-mod | 25.6 ms / 27 MB | 24.8 ms / 27 MB | 37.9 ms / 21 MB | 15.3 ms / 20 MB | 20.4 ms / 25 MB | 81.7 ms / 184 MB | n/a | n/a |
| users-narrow-25k-high-score | 11.8 ms / 18 MB | 12.4 ms / 19 MB | 17.9 ms / 19 MB | 14.0 ms / 16 MB | 16.0 ms / 23 MB | 66.1 ms / 145 MB | n/a | n/a |
| users-narrow-25k-identity | 4.50 ms / 4.9 MB | 4.98 ms / 4.9 MB | 23.8 ms / 20 MB | 12.4 ms / 15 MB | 13.4 ms / 23 MB | 60.9 ms / 122 MB | n/a | n/a |
| users-narrow-25k-ids | 5.78 ms / 7.8 MB | 6.02 ms / 7.8 MB | 16.2 ms / 19 MB | 9.46 ms / 16 MB | 12.4 ms / 23 MB | 51.2 ms / 125 MB | n/a | n/a |
| users-narrow-25k-keys-len | 5.35 ms / 5.2 MB | 5.50 ms / 5.2 MB | 11.4 ms / 19 MB | 7.83 ms / 15 MB | 11.0 ms / 19 MB | 29.6 ms / 83 MB | n/a | n/a |
| users-narrow-25k-keys-publish | 5.34 ms / 5.2 MB | 5.64 ms / 5.2 MB | 11.5 ms / 19 MB | 8.66 ms / 15 MB | 10.1 ms / 19 MB | 29.0 ms / 85 MB | n/a | n/a |
| users-narrow-25k-max-score | 9.64 ms / 16 MB | 9.23 ms / 16 MB | 16.7 ms / 19 MB | 12.3 ms / 16 MB | 13.5 ms / 23 MB | 51.6 ms / 126 MB | n/a | n/a |
| users-narrow-25k-nested-dept | 5.35 ms / 5.1 MB | 5.56 ms / 5.1 MB | 12.9 ms / 19 MB | 7.45 ms / 15 MB | 10.6 ms / 19 MB | 31.7 ms / 84 MB | n/a | n/a |
| users-narrow-25k-project-names | 5.74 ms / 7.7 MB | 5.76 ms / 7.7 MB | 15.4 ms / 19 MB | 12.4 ms / 16 MB | 12.4 ms / 23 MB | 55.8 ms / 131 MB | n/a | n/a |
| users-narrow-25k-project-pair | 9.59 ms / 14 MB | 9.28 ms / 14 MB | 31.4 ms / 30 MB | 25.3 ms / 16 MB | 19.2 ms / 35 MB | 323 ms / 340 MB | n/a | n/a |
| users-narrow-25k-reduce-score | 10.1 ms / 15 MB | 13.6 ms / 15 MB | 16.0 ms / 19 MB | 13.1 ms / 15 MB | 13.7 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-reverse-id | 10.3 ms / 25 MB | 10.1 ms / 25 MB | 16.8 ms / 19 MB | 7.42 ms / 15 MB | 10.4 ms / 19 MB | 41.4 ms / 128 MB | n/a | n/a |
| users-narrow-25k-select-id-stream | 5.17 ms / 5.1 MB | 5.30 ms / 5.1 MB | 16.6 ms / 19 MB | 18.8 ms / 15 MB | 13.8 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-slice-length | 6.65 ms / 9.9 MB | 7.09 ms / 9.9 MB | 13.5 ms / 19 MB | 9.61 ms / 15 MB | 12.6 ms / 19 MB | 34.0 ms / 90 MB | n/a | n/a |
| users-narrow-25k-sort-last | 13.5 ms / 26 MB | 13.9 ms / 26 MB | 37.3 ms / 21 MB | 13.6 ms / 17 MB | 30.0 ms / 28 MB | 82.3 ms / 137 MB | n/a | n/a |
| users-narrow-25k-sum-score | 10.8 ms / 15 MB | 11.3 ms / 15 MB | 18.2 ms / 19 MB | 11.8 ms / 16 MB | 14.0 ms / 23 MB | n/a | n/a | n/a |
| users-narrow-25k-type-path | 5.45 ms / 5.1 MB | 5.73 ms / 5.1 MB | 11.4 ms / 19 MB | 8.46 ms / 15 MB | 10.6 ms / 19 MB | disagreed | n/a | n/a |
| users-narrow-25k-unique-scores | 12.4 ms / 21 MB | 12.6 ms / 21 MB | 25.1 ms / 20 MB | 11.8 ms / 16 MB | 26.0 ms / 26 MB | 46.1 ms / 106 MB | n/a | n/a |
| users-narrow-50k-all-nonneg | 19.7 ms / 18 MB | 19.0 ms / 18 MB | 31.6 ms / 35 MB | 30.6 ms / 27 MB | 28.4 ms / 34 MB | n/a | n/a | n/a |
| users-narrow-50k-any-high | 12.1 ms / 18 MB | 11.4 ms / 18 MB | 20.5 ms / 35 MB | 11.6 ms / 27 MB | 17.7 ms / 32 MB | n/a | n/a | n/a |
| users-narrow-50k-count | 6.35 ms / 11 MB | 6.39 ms / 11 MB | 20.1 ms / 35 MB | 11.4 ms / 26 MB | 16.7 ms / 30 MB | 50.6 ms / 133 MB | n/a | n/a |
| users-narrow-50k-descent | 15.7 ms / 23 MB | 14.1 ms / 23 MB | 43.0 ms / 47 MB | 19.2 ms / 33 MB | 52.5 ms / 58 MB | 194 ms / 466 MB | n/a | n/a |
| users-narrow-50k-filter-active | 12.2 ms / 12 MB | 11.5 ms / 12 MB | 32.0 ms / 35 MB | 33.0 ms / 26 MB | 25.6 ms / 34 MB | 84.6 ms / 212 MB | n/a | n/a |
| users-narrow-50k-first-id | 7.25 ms / 5.7 MB | 7.99 ms / 5.7 MB | 20.4 ms / 35 MB | 11.9 ms / 26 MB | 17.2 ms / 31 MB | 52.5 ms / 133 MB | n/a | n/a |
| users-narrow-50k-group-mod | 45.4 ms / 32 MB | 45.1 ms / 32 MB | 77.2 ms / 39 MB | 27.6 ms / 34 MB | 37.3 ms / 47 MB | 161 ms / 295 MB | n/a | n/a |
| users-narrow-50k-high-score | 20.0 ms / 24 MB | 18.0 ms / 25 MB | 33.4 ms / 36 MB | 24.7 ms / 28 MB | 29.2 ms / 39 MB | 127 ms / 239 MB | n/a | n/a |
| users-narrow-50k-identity | 6.16 ms / 5.5 MB | 7.05 ms / 5.6 MB | 45.8 ms / 38 MB | 21.7 ms / 26 MB | 25.3 ms / 39 MB | 113 ms / 203 MB | n/a | n/a |
| users-narrow-50k-ids | 9.13 ms / 11 MB | 12.0 ms / 11 MB | 28.5 ms / 36 MB | 15.4 ms / 27 MB | 20.9 ms / 39 MB | 93.9 ms / 209 MB | n/a | n/a |
| users-narrow-50k-keys-len | 7.41 ms / 5.8 MB | 7.73 ms / 5.8 MB | 20.6 ms / 35 MB | 11.7 ms / 26 MB | 17.1 ms / 30 MB | 54.4 ms / 133 MB | n/a | n/a |
| users-narrow-50k-keys-publish | 7.75 ms / 5.8 MB | 7.72 ms / 5.8 MB | 20.7 ms / 35 MB | 13.1 ms / 26 MB | 19.2 ms / 30 MB | 53.6 ms / 123 MB | n/a | n/a |
| users-narrow-50k-max-score | 15.6 ms / 19 MB | 15.1 ms / 19 MB | 28.7 ms / 36 MB | 21.4 ms / 27 MB | 23.2 ms / 39 MB | 92.6 ms / 200 MB | n/a | n/a |
| users-narrow-50k-nested-dept | 7.36 ms / 5.7 MB | 8.53 ms / 5.7 MB | 20.8 ms / 35 MB | 11.5 ms / 26 MB | 16.9 ms / 31 MB | 52.1 ms / 123 MB | n/a | n/a |
| users-narrow-50k-project-names | 8.68 ms / 11 MB | 9.18 ms / 11 MB | 28.1 ms / 36 MB | 21.1 ms / 27 MB | 21.3 ms / 38 MB | 99.4 ms / 221 MB | n/a | n/a |
| users-narrow-50k-project-pair | 15.0 ms / 22 MB | 15.2 ms / 22 MB | 61.8 ms / 58 MB | 47.4 ms / 28 MB | 35.6 ms / 61 MB | 647 ms / 637 MB | n/a | n/a |
| users-narrow-50k-reduce-score | 17.3 ms / 18 MB | 15.5 ms / 18 MB | 30.0 ms / 35 MB | 23.3 ms / 27 MB | 23.4 ms / 35 MB | n/a | n/a | n/a |
| users-narrow-50k-reverse-id | 16.6 ms / 28 MB | 17.5 ms / 28 MB | 30.0 ms / 36 MB | 11.8 ms / 26 MB | 17.1 ms / 31 MB | 77.7 ms / 214 MB | n/a | n/a |
| users-narrow-50k-select-id-stream | 8.21 ms / 5.7 MB | 10.4 ms / 5.7 MB | 31.8 ms / 35 MB | 34.5 ms / 26 MB | 24.9 ms / 34 MB | n/a | n/a | n/a |
| users-narrow-50k-slice-length | 7.18 ms / 11 MB | 7.22 ms / 11 MB | 21.1 ms / 35 MB | 11.9 ms / 26 MB | 17.7 ms / 30 MB | 55.0 ms / 131 MB | n/a | n/a |
| users-narrow-50k-sort-last | 21.8 ms / 33 MB | 22.0 ms / 33 MB | 78.3 ms / 41 MB | 25.2 ms / 31 MB | 56.6 ms / 47 MB | 171 ms / 243 MB | n/a | n/a |
| users-narrow-50k-sum-score | 17.2 ms / 18 MB | 15.6 ms / 18 MB | 32.3 ms / 36 MB | 19.1 ms / 27 MB | 21.7 ms / 39 MB | n/a | n/a | n/a |
| users-narrow-50k-type-path | 7.52 ms / 5.7 MB | 9.40 ms / 5.7 MB | 21.8 ms / 35 MB | 11.7 ms / 26 MB | 17.1 ms / 30 MB | disagreed | n/a | n/a |
| users-narrow-50k-unique-scores | 20.0 ms / 24 MB | 18.3 ms / 24 MB | 47.0 ms / 36 MB | 19.7 ms / 28 MB | 51.4 ms / 46 MB | 89.5 ms / 187 MB | n/a | n/a |
| users-narrow-100k-all-nonneg | 34.1 ms / 31 MB | 33.5 ms / 31 MB | 60.1 ms / 70 MB | 59.8 ms / 52 MB | 53.4 ms / 62 MB | n/a | n/a | n/a |
| users-narrow-100k-any-high | 21.5 ms / 31 MB | 19.2 ms / 31 MB | 38.7 ms / 70 MB | 21.0 ms / 52 MB | 30.8 ms / 57 MB | n/a | n/a | n/a |
| users-narrow-100k-count | 11.7 ms / 18 MB | 10.6 ms / 18 MB | 38.8 ms / 70 MB | 20.1 ms / 50 MB | 29.4 ms / 54 MB | 95.3 ms / 217 MB | n/a | n/a |
| users-narrow-100k-descent | 24.6 ms / 40 MB | 24.6 ms / 40 MB | 82.9 ms / 89 MB | 35.2 ms / 64 MB | 98.4 ms / 112 MB | 352 ms / 953 MB | n/a | n/a |
| users-narrow-100k-filter-active | 19.6 ms / 18 MB | 19.6 ms / 18 MB | 61.4 ms / 70 MB | 63.0 ms / 50 MB | 45.7 ms / 62 MB | 166 ms / 347 MB | n/a | n/a |
| users-narrow-100k-first-id | 12.4 ms / 6.8 MB | 10.7 ms / 6.9 MB | 38.8 ms / 70 MB | 19.9 ms / 50 MB | 29.3 ms / 54 MB | 96.1 ms / 211 MB | n/a | n/a |
| users-narrow-100k-group-mod | 89.6 ms / 69 MB | 89.0 ms / 69 MB | 159 ms / 79 MB | 57.8 ms / 64 MB | 70.4 ms / 78 MB | 319 ms / 565 MB | n/a | n/a |
| users-narrow-100k-high-score | 32.8 ms / 46 MB | 32.9 ms / 46 MB | 65.3 ms / 70 MB | 47.9 ms / 53 MB | 51.0 ms / 72 MB | 242 ms / 470 MB | n/a | n/a |
| users-narrow-100k-identity | 11.8 ms / 6.7 MB | 7.47 ms / 6.7 MB | 85.3 ms / 76 MB | 36.8 ms / 50 MB | 42.7 ms / 70 MB | 223 ms / 312 MB | n/a | n/a |
| users-narrow-100k-ids | 14.5 ms / 17 MB | 12.7 ms / 17 MB | 57.2 ms / 72 MB | 28.6 ms / 53 MB | 38.5 ms / 71 MB | 183 ms / 377 MB | n/a | n/a |
| users-narrow-100k-keys-len | 12.3 ms / 7.0 MB | 11.8 ms / 7.0 MB | 38.9 ms / 70 MB | 20.2 ms / 50 MB | 29.4 ms / 54 MB | 95.8 ms / 218 MB | n/a | n/a |
| users-narrow-100k-keys-publish | 13.1 ms / 7.0 MB | 10.9 ms / 7.0 MB | 38.7 ms / 70 MB | 20.4 ms / 50 MB | 29.5 ms / 54 MB | 95.4 ms / 214 MB | n/a | n/a |
| users-narrow-100k-max-score | 27.4 ms / 37 MB | 26.8 ms / 37 MB | 54.9 ms / 72 MB | 39.6 ms / 53 MB | 40.7 ms / 73 MB | 174 ms / 374 MB | n/a | n/a |
| users-narrow-100k-nested-dept | 12.5 ms / 6.9 MB | 11.1 ms / 6.9 MB | 38.5 ms / 70 MB | 19.9 ms / 50 MB | 29.3 ms / 54 MB | 96.3 ms / 220 MB | n/a | n/a |
| users-narrow-100k-project-names | 13.8 ms / 17 MB | 12.1 ms / 17 MB | 54.7 ms / 72 MB | 39.8 ms / 53 MB | 38.8 ms / 72 MB | 197 ms / 420 MB | n/a | n/a |
| users-narrow-100k-project-pair | 26.8 ms / 41 MB | 27.9 ms / 41 MB | 117 ms / 115 MB | 90.8 ms / 52 MB | 65.0 ms / 116 MB | 1280 ms / 1260 MB | n/a | n/a |
| users-narrow-100k-reduce-score | 29.4 ms / 30 MB | 29.0 ms / 30 MB | 57.8 ms / 70 MB | 42.7 ms / 51 MB | 43.7 ms / 64 MB | n/a | n/a | n/a |
| users-narrow-100k-reverse-id | 28.8 ms / 61 MB | 28.6 ms / 61 MB | 62.9 ms / 72 MB | 20.2 ms / 50 MB | 30.1 ms / 56 MB | 146 ms / 378 MB | n/a | n/a |
| users-narrow-100k-select-id-stream | 12.6 ms / 6.9 MB | 10.4 ms / 6.9 MB | 59.8 ms / 70 MB | 65.8 ms / 50 MB | 43.5 ms / 61 MB | n/a | n/a | n/a |
| users-narrow-100k-slice-length | 10.1 ms / 18 MB | 10.2 ms / 18 MB | 38.6 ms / 70 MB | 19.7 ms / 50 MB | 29.0 ms / 54 MB | 101 ms / 230 MB | n/a | n/a |
| users-narrow-100k-sort-last | 41.2 ms / 74 MB | 42.2 ms / 74 MB | 164 ms / 79 MB | 55.3 ms / 59 MB | 113 ms / 88 MB | 376 ms / 437 MB | n/a | n/a |
| users-narrow-100k-sum-score | 29.5 ms / 30 MB | 29.1 ms / 30 MB | 64.5 ms / 72 MB | 36.0 ms / 53 MB | 42.0 ms / 71 MB | n/a | n/a | n/a |
| users-narrow-100k-type-path | 12.8 ms / 6.9 MB | 10.8 ms / 6.9 MB | 39.4 ms / 70 MB | 20.1 ms / 50 MB | 29.6 ms / 54 MB | disagreed | n/a | n/a |
| users-narrow-100k-unique-scores | 34.3 ms / 45 MB | 33.3 ms / 45 MB | 97.2 ms / 73 MB | 34.0 ms / 54 MB | 95.5 ms / 79 MB | 169 ms / 349 MB | n/a | n/a |
| users-narrow-200k-all-nonneg | 63.0 ms / 57 MB | 62.5 ms / 57 MB | 120 ms / 137 MB | 112 ms / 98 MB | 97.8 ms / 117 MB | n/a | n/a | n/a |
| users-narrow-200k-any-high | 34.6 ms / 57 MB | 34.2 ms / 57 MB | 74.1 ms / 137 MB | 38.9 ms / 98 MB | 55.0 ms / 108 MB | n/a | n/a | n/a |
| users-narrow-200k-count | 16.8 ms / 30 MB | 14.9 ms / 30 MB | 73.8 ms / 137 MB | 36.2 ms / 94 MB | 52.9 ms / 100 MB | 190 ms / 401 MB | n/a | n/a |
| users-narrow-200k-descent | 45.6 ms / 78 MB | 45.3 ms / 78 MB | 163 ms / 182 MB | 66.1 ms / 122 MB | 193 ms / 221 MB | 729 ms / 1694 MB | n/a | n/a |
| users-narrow-200k-filter-active | 36.8 ms / 30 MB | 35.5 ms / 30 MB | 118 ms / 137 MB | 124 ms / 94 MB | 84.3 ms / 116 MB | 314 ms / 628 MB | n/a | n/a |
| users-narrow-200k-first-id | 18.4 ms / 9.3 MB | 18.1 ms / 9.3 MB | 72.3 ms / 137 MB | 36.3 ms / 94 MB | 53.9 ms / 100 MB | 185 ms / 404 MB | n/a | n/a |
| users-narrow-200k-group-mod | 176 ms / 117 MB | 179 ms / 117 MB | 325 ms / 154 MB | 114 ms / 130 MB | 149 ms / 149 MB | 628 ms / 1046 MB | n/a | n/a |
| users-narrow-200k-high-score | 60.8 ms / 81 MB | 61.1 ms / 82 MB | 126 ms / 139 MB | 88.5 ms / 100 MB | 95.6 ms / 134 MB | 479 ms / 848 MB | n/a | n/a |
| users-narrow-200k-identity | 10.8 ms / 9.2 MB | 10.6 ms / 9.2 MB | 167 ms / 149 MB | 70.1 ms / 94 MB | 77.4 ms / 134 MB | 431 ms / 685 MB | n/a | n/a |
| users-narrow-200k-ids | 24.5 ms / 37 MB | 24.3 ms / 37 MB | 111 ms / 140 MB | 51.9 ms / 98 MB | 70.5 ms / 136 MB | 346 ms / 670 MB | n/a | n/a |
| users-narrow-200k-keys-len | 19.8 ms / 9.5 MB | 18.5 ms / 9.5 MB | 74.3 ms / 137 MB | 36.7 ms / 94 MB | 53.4 ms / 100 MB | 185 ms / 423 MB | n/a | n/a |
| users-narrow-200k-keys-publish | 19.2 ms / 9.4 MB | 18.8 ms / 9.5 MB | 73.6 ms / 137 MB | 36.1 ms / 94 MB | 53.4 ms / 100 MB | 185 ms / 425 MB | n/a | n/a |
| users-narrow-200k-max-score | 48.2 ms / 63 MB | 48.0 ms / 63 MB | 104 ms / 140 MB | 76.1 ms / 98 MB | 76.7 ms / 138 MB | 357 ms / 739 MB | n/a | n/a |
| users-narrow-200k-nested-dept | 18.6 ms / 9.3 MB | 18.4 ms / 9.3 MB | 80.9 ms / 136 MB | 39.1 ms / 94 MB | 54.9 ms / 100 MB | 187 ms / 425 MB | n/a | n/a |
| users-narrow-200k-project-names | 22.1 ms / 37 MB | 22.0 ms / 37 MB | 104 ms / 140 MB | 74.9 ms / 98 MB | 72.7 ms / 136 MB | 395 ms / 806 MB | n/a | n/a |
| users-narrow-200k-project-pair | 50.5 ms / 80 MB | 50.1 ms / 80 MB | 233 ms / 227 MB | 177 ms / 99 MB | 122 ms / 227 MB | 2601 ms / 2348 MB | n/a | n/a |
| users-narrow-200k-reduce-score | 50.7 ms / 53 MB | 51.1 ms / 53 MB | 110 ms / 137 MB | 82.0 ms / 97 MB | 81.0 ms / 118 MB | n/a | n/a | n/a |
| users-narrow-200k-reverse-id | 55.7 ms / 116 MB | 54.5 ms / 116 MB | 129 ms / 140 MB | 38.0 ms / 94 MB | 55.7 ms / 103 MB | 294 ms / 777 MB | n/a | n/a |
| users-narrow-200k-select-id-stream | 17.2 ms / 9.3 MB | 16.8 ms / 9.3 MB | 114 ms / 137 MB | 126 ms / 94 MB | 83.2 ms / 116 MB | n/a | n/a | n/a |
| users-narrow-200k-slice-length | 15.6 ms / 30 MB | 15.2 ms / 30 MB | 75.0 ms / 137 MB | 36.5 ms / 95 MB | 53.4 ms / 100 MB | 190 ms / 446 MB | n/a | n/a |
| users-narrow-200k-sort-last | 82.4 ms / 131 MB | 82.5 ms / 131 MB | 356 ms / 153 MB | 114 ms / 113 MB | 254 ms / 174 MB | 766 ms / 827 MB | n/a | n/a |
| users-narrow-200k-sum-score | 51.2 ms / 53 MB | 51.2 ms / 53 MB | 121 ms / 140 MB | 64.5 ms / 98 MB | 73.9 ms / 136 MB | n/a | n/a | n/a |
| users-narrow-200k-type-path | 19.6 ms / 9.4 MB | 17.9 ms / 9.4 MB | 74.4 ms / 137 MB | 36.9 ms / 94 MB | 54.8 ms / 100 MB | disagreed | n/a | n/a |
| users-narrow-200k-unique-scores | 63.9 ms / 78 MB | 63.7 ms / 78 MB | 190 ms / 140 MB | 66.6 ms / 98 MB | 189 ms / 158 MB | 319 ms / 627 MB | n/a | n/a |
| yaml-broad-100-count | 9.39 ms / 5.9 MB | 8.99 ms / 5.9 MB | n/a | 9.62 ms / 5.6 MB | 13.1 ms / 10 MB | 15.6 ms / 22 MB | 15.6 ms / 16 MB | n/a |
| yaml-broad-100-descent | 10.2 ms / 7.8 MB | 9.52 ms / 7.8 MB | n/a | 11.1 ms / 5.9 MB | 15.5 ms / 13 MB | 22.3 ms / 38 MB | n/a | n/a |
| yaml-broad-100-exact-name | 9.50 ms / 5.6 MB | 8.79 ms / 5.7 MB | n/a | 9.62 ms / 5.5 MB | 13.9 ms / 11 MB | 15.2 ms / 22 MB | 15.9 ms / 16 MB | n/a |
| yaml-broad-100-first-id | 9.48 ms / 5.6 MB | 9.59 ms / 5.7 MB | n/a | 9.47 ms / 5.6 MB | 12.8 ms / 10 MB | 15.6 ms / 22 MB | 14.9 ms / 16 MB | n/a |
| yaml-broad-100-identity | 10.4 ms / 7.6 MB | 10.0 ms / 7.6 MB | n/a | 10.7 ms / 5.5 MB | 13.4 ms / 11 MB | 19.5 ms / 34 MB | 18.2 ms / 17 MB | n/a |
| yaml-broad-100-ids | 9.55 ms / 6.1 MB | 9.38 ms / 6.1 MB | n/a | 10.4 ms / 5.6 MB | 13.6 ms / 10 MB | 15.9 ms / 24 MB | n/a | n/a |
| yaml-broad-100-keys-publish | 9.49 ms / 5.8 MB | 8.76 ms / 5.8 MB | n/a | 9.50 ms / 5.7 MB | 13.6 ms / 10 MB | disagreed | n/a | n/a |
| yaml-broad-100-nested-dept | 9.26 ms / 5.7 MB | 9.07 ms / 5.7 MB | n/a | 10.2 ms / 5.5 MB | 14.6 ms / 10 MB | 17.3 ms / 24 MB | 17.4 ms / 16 MB | n/a |
| yaml-broad-100-type-path | 8.73 ms / 5.7 MB | 9.04 ms / 5.7 MB | n/a | 9.64 ms / 5.5 MB | 15.2 ms / 10 MB | disagreed | n/a | n/a |
| yaml-broad-1k-count | 21.7 ms / 14 MB | 21.6 ms / 14 MB | n/a | 28.0 ms / 22 MB | 55.2 ms / 36 MB | 47.9 ms / 66 MB | 62.6 ms / 59 MB | n/a |
| yaml-broad-1k-descent | 30.8 ms / 28 MB | 32.0 ms / 29 MB | n/a | 29.7 ms / 24 MB | 75.3 ms / 50 MB | 100 ms / 209 MB | n/a | n/a |
| yaml-broad-1k-exact-name | 22.4 ms / 12 MB | 21.4 ms / 12 MB | n/a | 27.9 ms / 22 MB | 57.5 ms / 36 MB | 50.2 ms / 66 MB | 60.6 ms / 57 MB | n/a |
| yaml-broad-1k-first-id | 25.0 ms / 12 MB | 21.1 ms / 12 MB | n/a | 26.9 ms / 22 MB | 54.9 ms / 36 MB | 49.3 ms / 66 MB | 62.3 ms / 58 MB | n/a |
| yaml-broad-1k-identity | 29.7 ms / 27 MB | 28.2 ms / 29 MB | n/a | 31.1 ms / 22 MB | disagreed | 80.5 ms / 110 MB | 80.7 ms / 69 MB | n/a |
| yaml-broad-1k-ids | 21.7 ms / 14 MB | 20.8 ms / 14 MB | n/a | 26.3 ms / 22 MB | 57.3 ms / 35 MB | 51.5 ms / 68 MB | n/a | n/a |
| yaml-broad-1k-keys-publish | 22.3 ms / 12 MB | 21.8 ms / 12 MB | n/a | 28.5 ms / 22 MB | 56.3 ms / 35 MB | disagreed | n/a | n/a |
| yaml-broad-1k-nested-dept | 22.4 ms / 12 MB | 20.3 ms / 12 MB | n/a | 26.5 ms / 22 MB | 57.4 ms / 36 MB | 49.8 ms / 66 MB | 63.1 ms / 59 MB | n/a |
| yaml-broad-1k-type-path | 21.8 ms / 12 MB | 20.6 ms / 12 MB | n/a | 28.9 ms / 22 MB | 57.0 ms / 36 MB | disagreed | n/a | n/a |
| yaml-broad-5k-count | 71.2 ms / 48 MB | 71.3 ms / 48 MB | n/a | 100 ms / 96 MB | 256 ms / 147 MB | 185 ms / 259 MB | 249 ms / 237 MB | n/a |
| yaml-broad-5k-descent | 111 ms / 110 MB | 114 ms / 110 MB | n/a | 114 ms / 109 MB | 326 ms / 214 MB | 428 ms / 940 MB | n/a | n/a |
| yaml-broad-5k-exact-name | 73.6 ms / 41 MB | 74.0 ms / 41 MB | n/a | 99.3 ms / 96 MB | 240 ms / 152 MB | 187 ms / 259 MB | 251 ms / 237 MB | n/a |
| yaml-broad-5k-first-id | 71.6 ms / 41 MB | 74.0 ms / 41 MB | n/a | 97.9 ms / 96 MB | 235 ms / 151 MB | 191 ms / 259 MB | 257 ms / 233 MB | n/a |
| yaml-broad-5k-identity | 106 ms / 110 MB | 109 ms / 110 MB | n/a | 120 ms / 96 MB | disagreed | 349 ms / 418 MB | 352 ms / 279 MB | n/a |
| yaml-broad-5k-ids | 71.8 ms / 50 MB | 71.0 ms / 50 MB | n/a | 99.4 ms / 96 MB | 237 ms / 144 MB | 192 ms / 269 MB | n/a | n/a |
| yaml-broad-5k-keys-publish | 74.1 ms / 41 MB | 74.0 ms / 41 MB | n/a | 98.5 ms / 96 MB | 234 ms / 153 MB | disagreed | n/a | n/a |
| yaml-broad-5k-nested-dept | 71.2 ms / 41 MB | 72.3 ms / 41 MB | n/a | 97.9 ms / 96 MB | 238 ms / 151 MB | 191 ms / 258 MB | 252 ms / 237 MB | n/a |
| yaml-broad-5k-type-path | 74.3 ms / 41 MB | 74.4 ms / 41 MB | n/a | 101 ms / 96 MB | 240 ms / 142 MB | disagreed | n/a | n/a |
| yaml-broad-25k-count | 310 ms / 229 MB | 312 ms / 229 MB | n/a | 454 ms / 464 MB | 1149 ms / 691 MB | 873 ms / 1224 MB | 1188 ms / 1111 MB | n/a |
| yaml-broad-25k-descent | 519 ms / 536 MB | 516 ms / 536 MB | n/a | 539 ms / 521 MB | 1544 ms / 1052 MB | 2082 ms / 4729 MB | n/a | n/a |
| yaml-broad-25k-exact-name | 319 ms / 226 MB | 320 ms / 226 MB | n/a | 452 ms / 464 MB | 1112 ms / 719 MB | 882 ms / 1222 MB | 1184 ms / 1111 MB | n/a |
| yaml-broad-25k-first-id | 330 ms / 226 MB | 324 ms / 226 MB | n/a | 454 ms / 464 MB | 1135 ms / 727 MB | 878 ms / 1222 MB | 1203 ms / 1107 MB | n/a |
| yaml-broad-25k-identity | 491 ms / 536 MB | 490 ms / 536 MB | n/a | 556 ms / 464 MB | disagreed | 1643 ms / 2157 MB | 1714 ms / 1321 MB | n/a |
| yaml-broad-25k-ids | 330 ms / 236 MB | 328 ms / 234 MB | n/a | 464 ms / 464 MB | 1137 ms / 728 MB | 917 ms / 1270 MB | n/a | n/a |
| yaml-broad-25k-keys-publish | 323 ms / 226 MB | 328 ms / 226 MB | n/a | 473 ms / 464 MB | 1148 ms / 708 MB | disagreed | n/a | n/a |
| yaml-broad-25k-nested-dept | 321 ms / 226 MB | 328 ms / 226 MB | n/a | 466 ms / 464 MB | 1130 ms / 718 MB | 865 ms / 1221 MB | 1193 ms / 1111 MB | n/a |
| yaml-broad-25k-type-path | 323 ms / 226 MB | 324 ms / 226 MB | n/a | 451 ms / 464 MB | 1122 ms / 698 MB | disagreed | n/a | n/a |
| yaml-broad-50k-count | 623 ms / 455 MB | 613 ms / 454 MB | n/a | 907 ms / 924 MB | 2250 ms / 1475 MB | 1715 ms / 2425 MB | 2440 ms / 2210 MB | n/a |
| yaml-broad-50k-descent | 1018 ms / 975 MB | 1016 ms / 977 MB | n/a | 1061 ms / 1030 MB | 3073 ms / 2131 MB | 4048 ms / 9448 MB | n/a | n/a |
| yaml-broad-50k-exact-name | 645 ms / 447 MB | 646 ms / 446 MB | n/a | 909 ms / 924 MB | 2277 ms / 1476 MB | 1718 ms / 2418 MB | 2414 ms / 2254 MB | n/a |
| yaml-broad-50k-first-id | 646 ms / 446 MB | 645 ms / 447 MB | n/a | 896 ms / 924 MB | 2252 ms / 1477 MB | 1693 ms / 2426 MB | 2401 ms / 2305 MB | n/a |
| yaml-broad-50k-identity | 984 ms / 975 MB | 985 ms / 977 MB | n/a | 1085 ms / 924 MB | disagreed | 3324 ms / 4367 MB | 3382 ms / 2827 MB | n/a |
| yaml-broad-50k-ids | 655 ms / 455 MB | 653 ms / 448 MB | n/a | 903 ms / 924 MB | 2237 ms / 1489 MB | 1833 ms / 2518 MB | n/a | n/a |
| yaml-broad-50k-keys-publish | 635 ms / 447 MB | 646 ms / 447 MB | n/a | 892 ms / 924 MB | 2238 ms / 1474 MB | disagreed | n/a | n/a |
| yaml-broad-50k-nested-dept | 635 ms / 447 MB | 642 ms / 447 MB | n/a | 890 ms / 924 MB | 2257 ms / 1470 MB | 1720 ms / 2426 MB | 2408 ms / 2286 MB | n/a |
| yaml-broad-50k-type-path | 642 ms / 447 MB | 646 ms / 447 MB | n/a | 924 ms / 924 MB | 2285 ms / 1455 MB | disagreed | n/a | n/a |
| yaml-broad-100k-count | 1219 ms / 890 MB | 1240 ms / 891 MB | n/a | 1804 ms / 1845 MB | 4504 ms / 2998 MB | 3445 ms / 4819 MB | 4803 ms / 4410 MB | n/a |
| yaml-broad-100k-descent | 2032 ms / 2071 MB | 2047 ms / 2071 MB | n/a | 2129 ms / 2067 MB | 6155 ms / 4659 MB | 7826 ms / 19192 MB | n/a | n/a |
| yaml-broad-100k-exact-name | 1296 ms / 874 MB | 1289 ms / 874 MB | n/a | 1823 ms / 1845 MB | 4449 ms / 2779 MB | 3434 ms / 4823 MB | 4799 ms / 4436 MB | n/a |
| yaml-broad-100k-first-id | 1272 ms / 874 MB | 1290 ms / 874 MB | n/a | 1787 ms / 1845 MB | 4517 ms / 2819 MB | 3429 ms / 4803 MB | 4828 ms / 4520 MB | n/a |
| yaml-broad-100k-identity | 1983 ms / 2070 MB | 1976 ms / 2071 MB | n/a | 2222 ms / 1845 MB | disagreed | 6574 ms / 9067 MB | 6804 ms / 5634 MB | n/a |
| yaml-broad-100k-ids | 1369 ms / 892 MB | 1365 ms / 891 MB | n/a | 1809 ms / 1845 MB | 4568 ms / 2723 MB | 3601 ms / 4995 MB | n/a | n/a |
| yaml-broad-100k-keys-publish | 1290 ms / 874 MB | 1279 ms / 874 MB | n/a | 1848 ms / 1845 MB | 4523 ms / 2999 MB | disagreed | n/a | n/a |
| yaml-broad-100k-nested-dept | 1276 ms / 874 MB | 1275 ms / 874 MB | n/a | 1795 ms / 1845 MB | 4526 ms / 2962 MB | 3421 ms / 4817 MB | 4813 ms / 4593 MB | n/a |
| yaml-broad-100k-type-path | 1297 ms / 874 MB | 1291 ms / 874 MB | n/a | 1818 ms / 1845 MB | 4512 ms / 2792 MB | disagreed | n/a | n/a |
| yaml-narrow-100-count | 9.99 ms / 4.9 MB | 6.85 ms / 4.9 MB | n/a | 6.59 ms / 4.1 MB | 7.44 ms / 6.0 MB | 10.6 ms / 17 MB | 7.61 ms / 9.7 MB | n/a |
| yaml-narrow-100-descent | 6.89 ms / 5.1 MB | 7.13 ms / 5.1 MB | n/a | 6.55 ms / 4.1 MB | 8.08 ms / 6.2 MB | 11.6 ms / 19 MB | n/a | n/a |
| yaml-narrow-100-exact-name | 6.52 ms / 4.8 MB | 6.69 ms / 4.8 MB | n/a | 6.25 ms / 4.0 MB | 6.81 ms / 5.9 MB | 9.88 ms / 17 MB | error | n/a |
| yaml-narrow-100-first-id | 6.59 ms / 4.8 MB | 7.03 ms / 4.8 MB | n/a | 7.39 ms / 4.0 MB | 7.46 ms / 6.1 MB | 12.2 ms / 19 MB | 9.36 ms / 9.6 MB | n/a |
| yaml-narrow-100-identity | 7.02 ms / 4.9 MB | 7.66 ms / 4.9 MB | n/a | 8.11 ms / 4.0 MB | 7.15 ms / 5.9 MB | 11.2 ms / 20 MB | 7.84 ms / 9.8 MB | n/a |
| yaml-narrow-100-ids | 6.35 ms / 5.1 MB | 6.51 ms / 5.1 MB | n/a | 6.26 ms / 4.0 MB | 6.50 ms / 5.9 MB | 10.7 ms / 22 MB | n/a | n/a |
| yaml-narrow-100-keys-publish | 6.85 ms / 4.9 MB | 7.07 ms / 4.9 MB | n/a | 6.59 ms / 4.2 MB | 7.82 ms / 6.1 MB | 12.4 ms / 18 MB | n/a | n/a |
| yaml-narrow-100-nested-dept | 7.16 ms / 4.8 MB | 6.51 ms / 4.9 MB | n/a | 7.31 ms / 4.0 MB | 7.71 ms / 6.1 MB | 10.5 ms / 21 MB | error | n/a |
| yaml-narrow-100-type-path | 7.59 ms / 4.9 MB | 8.06 ms / 4.9 MB | n/a | 9.54 ms / 4.0 MB | 8.25 ms / 6.3 MB | disagreed | n/a | n/a |
| yaml-narrow-1k-count | 7.74 ms / 5.4 MB | 7.67 ms / 5.4 MB | n/a | 7.19 ms / 4.6 MB | 8.89 ms / 7.5 MB | 13.8 ms / 20 MB | 11.9 ms / 13 MB | n/a |
| yaml-narrow-1k-descent | 7.50 ms / 6.2 MB | 7.46 ms / 6.2 MB | n/a | 7.20 ms / 4.7 MB | 10.0 ms / 9.5 MB | 14.1 ms / 30 MB | n/a | n/a |
| yaml-narrow-1k-exact-name | 7.68 ms / 5.2 MB | 9.37 ms / 5.2 MB | n/a | 7.47 ms / 4.5 MB | 9.17 ms / 7.4 MB | 11.3 ms / 19 MB | error | n/a |
| yaml-narrow-1k-first-id | 7.42 ms / 5.2 MB | 7.17 ms / 5.2 MB | n/a | 7.92 ms / 4.5 MB | 9.08 ms / 7.8 MB | 11.6 ms / 20 MB | 10.3 ms / 13 MB | n/a |
| yaml-narrow-1k-identity | 7.35 ms / 6.0 MB | 7.57 ms / 6.0 MB | n/a | 7.92 ms / 4.5 MB | 9.87 ms / 7.7 MB | 15.5 ms / 29 MB | 13.3 ms / 14 MB | n/a |
| yaml-narrow-1k-ids | 11.6 ms / 5.9 MB | 8.65 ms / 5.9 MB | n/a | 7.94 ms / 4.6 MB | 9.75 ms / 7.7 MB | 13.8 ms / 29 MB | n/a | n/a |
| yaml-narrow-1k-keys-publish | 7.39 ms / 5.3 MB | 7.03 ms / 5.3 MB | n/a | 7.12 ms / 4.7 MB | 9.10 ms / 7.5 MB | 12.1 ms / 20 MB | n/a | n/a |
| yaml-narrow-1k-nested-dept | 7.21 ms / 5.2 MB | 6.93 ms / 5.2 MB | n/a | 7.27 ms / 4.5 MB | 9.47 ms / 7.4 MB | 11.4 ms / 20 MB | error | n/a |
| yaml-narrow-1k-type-path | 7.84 ms / 5.2 MB | 7.61 ms / 5.3 MB | n/a | 7.65 ms / 4.5 MB | 11.3 ms / 7.5 MB | disagreed | n/a | n/a |
| yaml-narrow-5k-count | 9.83 ms / 8.0 MB | 10.1 ms / 8.0 MB | n/a | 12.4 ms / 7.5 MB | 17.6 ms / 14 MB | 20.4 ms / 32 MB | 21.4 ms / 23 MB | n/a |
| yaml-narrow-5k-descent | 12.4 ms / 11 MB | 12.0 ms / 11 MB | n/a | 13.8 ms / 8.0 MB | 23.9 ms / 18 MB | 34.2 ms / 66 MB | n/a | n/a |
| yaml-narrow-5k-exact-name | 10.4 ms / 6.7 MB | 11.7 ms / 6.7 MB | n/a | 11.2 ms / 7.4 MB | 18.4 ms / 14 MB | 20.3 ms / 32 MB | error | n/a |
| yaml-narrow-5k-first-id | 10.8 ms / 6.6 MB | 11.4 ms / 6.7 MB | n/a | 11.2 ms / 7.4 MB | 17.3 ms / 15 MB | 20.4 ms / 32 MB | 20.1 ms / 23 MB | n/a |
| yaml-narrow-5k-identity | 12.2 ms / 11 MB | 11.7 ms / 11 MB | n/a | 13.3 ms / 7.4 MB | 19.0 ms / 15 MB | 27.6 ms / 45 MB | 28.6 ms / 24 MB | n/a |
| yaml-narrow-5k-ids | 12.6 ms / 9.1 MB | 12.6 ms / 9.3 MB | n/a | 12.0 ms / 7.6 MB | 18.0 ms / 15 MB | 25.9 ms / 39 MB | n/a | n/a |
| yaml-narrow-5k-keys-publish | 10.6 ms / 6.7 MB | 10.9 ms / 6.7 MB | n/a | 12.9 ms / 7.6 MB | 18.3 ms / 15 MB | 20.0 ms / 33 MB | n/a | n/a |
| yaml-narrow-5k-nested-dept | 10.4 ms / 6.7 MB | 10.1 ms / 6.7 MB | n/a | 13.0 ms / 7.4 MB | 18.2 ms / 15 MB | 20.0 ms / 31 MB | error | n/a |
| yaml-narrow-5k-type-path | 10.0 ms / 6.7 MB | 10.4 ms / 6.7 MB | n/a | 12.8 ms / 7.4 MB | 19.7 ms / 14 MB | disagreed | n/a | n/a |
| yaml-narrow-25k-count | 25.1 ms / 18 MB | 24.1 ms / 18 MB | n/a | 28.8 ms / 22 MB | 55.9 ms / 42 MB | 50.6 ms / 79 MB | 63.2 ms / 67 MB | n/a |
| yaml-narrow-25k-descent | 31.7 ms / 30 MB | 31.3 ms / 30 MB | n/a | 31.4 ms / 26 MB | 74.4 ms / 62 MB | 111 ms / 254 MB | n/a | n/a |
| yaml-narrow-25k-exact-name | 22.4 ms / 13 MB | 22.9 ms / 13 MB | n/a | 28.7 ms / 22 MB | 56.8 ms / 43 MB | 50.7 ms / 79 MB | error | n/a |
| yaml-narrow-25k-first-id | 24.2 ms / 13 MB | 22.8 ms / 13 MB | n/a | 28.5 ms / 22 MB | 56.6 ms / 42 MB | 50.6 ms / 79 MB | 64.5 ms / 68 MB | n/a |
| yaml-narrow-25k-identity | 31.1 ms / 30 MB | 31.0 ms / 30 MB | n/a | 32.5 ms / 22 MB | 57.0 ms / 47 MB | 82.7 ms / 110 MB | 90.4 ms / 88 MB | n/a |
| yaml-narrow-25k-ids | 37.7 ms / 26 MB | 37.0 ms / 26 MB | n/a | 31.1 ms / 23 MB | 57.1 ms / 46 MB | 70.3 ms / 114 MB | n/a | n/a |
| yaml-narrow-25k-keys-publish | 23.0 ms / 13 MB | 20.9 ms / 13 MB | n/a | 27.5 ms / 22 MB | 55.6 ms / 42 MB | 50.8 ms / 79 MB | n/a | n/a |
| yaml-narrow-25k-nested-dept | 23.0 ms / 13 MB | 23.8 ms / 13 MB | n/a | 28.5 ms / 22 MB | 54.3 ms / 42 MB | 49.0 ms / 79 MB | error | n/a |
| yaml-narrow-25k-type-path | 23.7 ms / 13 MB | 22.9 ms / 13 MB | n/a | 29.1 ms / 22 MB | 54.2 ms / 42 MB | disagreed | n/a | n/a |
| yaml-narrow-50k-count | 41.6 ms / 33 MB | 39.7 ms / 33 MB | n/a | 46.8 ms / 41 MB | 101 ms / 78 MB | 88.7 ms / 140 MB | 116 ms / 116 MB | n/a |
| yaml-narrow-50k-descent | 53.9 ms / 48 MB | 56.5 ms / 53 MB | n/a | 58.4 ms / 48 MB | 137 ms / 113 MB | 208 ms / 502 MB | n/a | n/a |
| yaml-narrow-50k-exact-name | 35.6 ms / 22 MB | 37.6 ms / 22 MB | n/a | 47.7 ms / 41 MB | 97.9 ms / 78 MB | 86.8 ms / 140 MB | error | n/a |
| yaml-narrow-50k-first-id | 35.9 ms / 22 MB | 36.9 ms / 22 MB | n/a | 48.5 ms / 41 MB | 97.6 ms / 79 MB | 87.7 ms / 141 MB | 115 ms / 116 MB | n/a |
| yaml-narrow-50k-identity | 52.5 ms / 47 MB | 51.5 ms / 53 MB | n/a | 56.1 ms / 41 MB | 104 ms / 87 MB | 146 ms / 211 MB | 169 ms / 150 MB | n/a |
| yaml-narrow-50k-ids | 83.7 ms / 43 MB | 83.7 ms / 45 MB | n/a | 53.6 ms / 43 MB | 107 ms / 87 MB | 132 ms / 227 MB | n/a | n/a |
| yaml-narrow-50k-keys-publish | 37.6 ms / 22 MB | 36.1 ms / 22 MB | n/a | 47.2 ms / 41 MB | 101 ms / 78 MB | 85.4 ms / 140 MB | n/a | n/a |
| yaml-narrow-50k-nested-dept | 37.8 ms / 22 MB | 37.6 ms / 22 MB | n/a | 48.7 ms / 41 MB | 102 ms / 78 MB | 88.1 ms / 141 MB | error | n/a |
| yaml-narrow-50k-type-path | 37.7 ms / 22 MB | 37.7 ms / 22 MB | n/a | 47.7 ms / 41 MB | 101 ms / 78 MB | disagreed | n/a | n/a |
| yaml-narrow-100k-count | 70.2 ms / 55 MB | 69.9 ms / 55 MB | n/a | 88.2 ms / 78 MB | 193 ms / 152 MB | 158 ms / 265 MB | 216 ms / 220 MB | n/a |
| yaml-narrow-100k-descent | 102 ms / 96 MB | 98.2 ms / 90 MB | n/a | 102 ms / 91 MB | 266 ms / 251 MB | 409 ms / 1046 MB | n/a | n/a |
| yaml-narrow-100k-exact-name | 62.8 ms / 37 MB | 64.1 ms / 37 MB | n/a | 87.9 ms / 78 MB | 188 ms / 152 MB | 160 ms / 265 MB | error | n/a |
| yaml-narrow-100k-first-id | 66.0 ms / 37 MB | 62.7 ms / 37 MB | n/a | 88.7 ms / 78 MB | 194 ms / 152 MB | 161 ms / 265 MB | 218 ms / 219 MB | n/a |
| yaml-narrow-100k-identity | 95.6 ms / 96 MB | 95.8 ms / 89 MB | n/a | 105 ms / 78 MB | 205 ms / 164 MB | 279 ms / 427 MB | 318 ms / 278 MB | n/a |
| yaml-narrow-100k-ids | 217 ms / 86 MB | 216 ms / 86 MB | n/a | 93.4 ms / 81 MB | 199 ms / 166 MB | 242 ms / 444 MB | n/a | n/a |
| yaml-narrow-100k-keys-publish | 65.9 ms / 37 MB | 65.6 ms / 37 MB | n/a | 84.8 ms / 78 MB | 186 ms / 152 MB | 155 ms / 265 MB | n/a | n/a |
| yaml-narrow-100k-nested-dept | 65.6 ms / 37 MB | 65.6 ms / 37 MB | n/a | 85.6 ms / 78 MB | 189 ms / 152 MB | 161 ms / 264 MB | error | n/a |
| yaml-narrow-100k-type-path | 63.1 ms / 37 MB | 62.9 ms / 37 MB | n/a | 85.1 ms / 78 MB | 191 ms / 151 MB | disagreed | n/a | n/a |

## known disagreements

gojq: gojq writes object keys in sorted order; compact bytes differ, JSON values match

users-broad-100-identity, users-broad-1k-identity, users-broad-5k-identity, users-broad-25k-identity, users-broad-50k-identity, users-broad-100k-identity, users-broad-200k-identity, ndjson-broad-100-identity, ndjson-broad-1k-identity, ndjson-broad-5k-identity, ndjson-broad-25k-identity, ndjson-broad-50k-identity, ndjson-broad-100k-identity, ndjson-broad-200k-identity, ndjson-broad-100-select-score, ndjson-broad-1k-select-score, ndjson-broad-5k-select-score, ndjson-broad-25k-select-score, ndjson-broad-50k-select-score, ndjson-broad-100k-select-score, ndjson-broad-200k-select-score, yaml-broad-100-identity, yaml-broad-1k-identity, yaml-broad-5k-identity, yaml-broad-25k-identity, yaml-broad-50k-identity, yaml-broad-100k-identity

yq: yq emits object keys in insertion order; jq/jqf keys are sorted

users-broad-100-keys-publish, users-broad-1k-keys-publish, users-broad-5k-keys-publish, users-broad-25k-keys-publish, users-broad-50k-keys-publish, users-broad-100k-keys-publish, users-broad-200k-keys-publish, yaml-broad-100-keys-publish, yaml-broad-1k-keys-publish, yaml-broad-5k-keys-publish, yaml-broad-25k-keys-publish, yaml-broad-50k-keys-publish, yaml-broad-100k-keys-publish, toml-broad-100-keys-publish, toml-broad-1k-keys-publish, toml-broad-5k-keys-publish, toml-broad-25k-keys-publish, toml-broad-50k-keys-publish, toml-broad-100k-keys-publish

yq: yq prints YAML type tags (!!seq) instead of jq kinds (array)

users-narrow-100-type-path, users-narrow-1k-type-path, users-narrow-5k-type-path, users-narrow-25k-type-path, users-narrow-50k-type-path, users-narrow-100k-type-path, users-narrow-200k-type-path, yaml-narrow-100-type-path, yaml-narrow-1k-type-path, yaml-narrow-5k-type-path, yaml-narrow-25k-type-path, yaml-narrow-50k-type-path, yaml-narrow-100k-type-path, toml-narrow-100-type-path, toml-narrow-1k-type-path, toml-narrow-5k-type-path, toml-narrow-25k-type-path, toml-narrow-50k-type-path, toml-narrow-100k-type-path, users-broad-100-type-path, users-broad-1k-type-path, users-broad-5k-type-path, users-broad-25k-type-path, users-broad-50k-type-path, users-broad-100k-type-path, users-broad-200k-type-path, yaml-broad-100-type-path, yaml-broad-1k-type-path, yaml-broad-5k-type-path, yaml-broad-25k-type-path, yaml-broad-50k-type-path, yaml-broad-100k-type-path, toml-broad-100-type-path, toml-broad-1k-type-path, toml-broad-5k-type-path, toml-broad-25k-type-path, toml-broad-50k-type-path, toml-broad-100k-type-path

## disagreements

### ndjson-broad-1k-identity · gojq (oracle jq)

expected (1124460 bytes, sha256 76dc496ed0c55af9…):

```
{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (1124460 bytes, sha256 3d266817d83d9689…):

```
{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-5k-identity · gojq (oracle jq)

expected (5636105 bytes, sha256 aad862b609ea98dd…):

```
{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (5636105 bytes, sha256 f8273128221e3cf7…):

```
{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-5k-select-score · gojq (oracle jq)

expected (5067481 bytes, sha256 52cf79bdbd0d1d21…):

```
{"id":3,"name":"user-3","email":"user-3@example.com","age":19,"active":false,"score":111,"tier":"enterprise","country":"JP","tags":["delta","priority","trial","internal"],"bio":"xxxxxxxxxxxxxxxxxxxxxx…
```

got (5067481 bytes, sha256 4034933a9f91ae42…):

```
{"active":false,"age":19,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-25k-identity · gojq (oracle jq)

expected (28239861 bytes, sha256 ae60a76fc0e4f5e2…):

```
{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (28239861 bytes, sha256 732ba9a88ca02808…):

```
{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-25k-select-score · gojq (oracle jq)

expected (25390639 bytes, sha256 e5761ba89a839fbf…):

```
{"id":3,"name":"user-3","email":"user-3@example.com","age":19,"active":false,"score":111,"tier":"enterprise","country":"JP","tags":["delta","priority","trial","internal"],"bio":"xxxxxxxxxxxxxxxxxxxxxx…
```

got (25390639 bytes, sha256 49fea349aecbe2d6…):

```
{"active":false,"age":19,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-50k-identity · gojq (oracle jq)

expected (56513257 bytes, sha256 d8ef6e1f62d7f6e3…):

```
{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (56513257 bytes, sha256 0cde3d3a7cdda255…):

```
{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-50k-select-score · gojq (oracle jq)

expected (50811227 bytes, sha256 a2ddb250ccac9739…):

```
{"id":3,"name":"user-3","email":"user-3@example.com","age":19,"active":false,"score":111,"tier":"enterprise","country":"JP","tags":["delta","priority","trial","internal"],"bio":"xxxxxxxxxxxxxxxxxxxxxx…
```

got (50811227 bytes, sha256 15dc6a73bcc3a4ee…):

```
{"active":false,"age":19,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-100k-identity · gojq (oracle jq)

expected (113060026 bytes, sha256 079410e842c4b427…):

```
{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (113060026 bytes, sha256 fd80a13e25b70bb5…):

```
{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-100k-select-score · gojq (oracle jq)

expected (101652548 bytes, sha256 e3eff84b50af87dc…):

```
{"id":3,"name":"user-3","email":"user-3@example.com","age":19,"active":false,"score":111,"tier":"enterprise","country":"JP","tags":["delta","priority","trial","internal"],"bio":"xxxxxxxxxxxxxxxxxxxxxx…
```

got (101652548 bytes, sha256 cc75c87f8901bae6…):

```
{"active":false,"age":19,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-200k-identity · gojq (oracle jq)

expected (226453559 bytes, sha256 a2290fe218c3c9a4…):

```
{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (226453559 bytes, sha256 c57aae90187ff2df…):

```
{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### ndjson-broad-200k-select-score · gojq (oracle jq)

expected (203604760 bytes, sha256 d3c12ba9b3f8f314…):

```
{"id":3,"name":"user-3","email":"user-3@example.com","age":19,"active":false,"score":111,"tier":"enterprise","country":"JP","tags":["delta","priority","trial","internal"],"bio":"xxxxxxxxxxxxxxxxxxxxxx…
```

got (203604760 bytes, sha256 c81edec726f79d27…):

```
{"active":false,"age":19,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### toml-broad-100-keys-publish · yq (oracle jqf)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### toml-broad-100-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### toml-narrow-100-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### toml-narrow-1k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-broad-100-keys-publish · yq (oracle jq)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### users-broad-100-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-broad-1k-identity · gojq (oracle jq)

expected (1124472 bytes, sha256 46472d4e7f90ef0d…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (1124472 bytes, sha256 6663c24d76b6acc6…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### users-broad-1k-keys-publish · yq (oracle jq)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### users-broad-1k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-broad-5k-identity · gojq (oracle jq)

expected (5636117 bytes, sha256 0e773c30377cc19c…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (5636117 bytes, sha256 5c46eb128f360f2b…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### users-broad-5k-keys-publish · yq (oracle jq)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### users-broad-5k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-broad-25k-identity · gojq (oracle jq)

expected (28239873 bytes, sha256 ae3a073cca202d94…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (28239873 bytes, sha256 cf784eb246a3a014…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### users-broad-25k-keys-publish · yq (oracle jq)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### users-broad-25k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-broad-50k-identity · gojq (oracle jq)

expected (56513269 bytes, sha256 26e0a508015bfb37…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (56513269 bytes, sha256 a7351843103d1c82…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### users-broad-50k-keys-publish · yq (oracle jq)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### users-broad-50k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-broad-100k-identity · gojq (oracle jq)

expected (113060038 bytes, sha256 e51349aa3398962e…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (113060038 bytes, sha256 a3f58a9a92850cad…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### users-broad-100k-keys-publish · yq (oracle jq)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### users-broad-100k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-broad-200k-identity · gojq (oracle jq)

expected (226453571 bytes, sha256 4bae361243e88839…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (226453571 bytes, sha256 c45d2e36f224829c…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### users-broad-200k-keys-publish · yq (oracle jq)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### users-broad-200k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-narrow-100-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-narrow-1k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-narrow-5k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-narrow-25k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-narrow-50k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-narrow-100k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### users-narrow-200k-type-path · yq (oracle jq)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-broad-100-keys-publish · yq (oracle jqf)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### yaml-broad-100-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-broad-1k-identity · gojq (oracle jqf)

expected (1124472 bytes, sha256 46472d4e7f90ef0d…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (1124472 bytes, sha256 6663c24d76b6acc6…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### yaml-broad-1k-keys-publish · yq (oracle jqf)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### yaml-broad-1k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-broad-5k-identity · gojq (oracle jqf)

expected (5636117 bytes, sha256 0e773c30377cc19c…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (5636117 bytes, sha256 5c46eb128f360f2b…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### yaml-broad-5k-keys-publish · yq (oracle jqf)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### yaml-broad-5k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-broad-25k-identity · gojq (oracle jqf)

expected (28239873 bytes, sha256 ae3a073cca202d94…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (28239873 bytes, sha256 cf784eb246a3a014…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### yaml-broad-25k-keys-publish · yq (oracle jqf)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### yaml-broad-25k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-broad-50k-identity · gojq (oracle jqf)

expected (56513269 bytes, sha256 26e0a508015bfb37…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (56513269 bytes, sha256 a7351843103d1c82…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### yaml-broad-50k-keys-publish · yq (oracle jqf)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### yaml-broad-50k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-broad-100k-identity · gojq (oracle jqf)

expected (113060038 bytes, sha256 e51349aa3398962e…):

```
{"users":[{"id":0,"name":"user-0","email":"user-0@example.com","age":16,"active":false,"score":0,"tier":"free","country":"US","tags":["alpha"],"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

got (113060038 bytes, sha256 a3f58a9a92850cad…):

```
{"users":[{"active":false,"age":16,"bio":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…
```

### yaml-broad-100k-keys-publish · yq (oracle jqf)

expected (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

got (287 bytes, sha256 1e402dc94afcc4ba…):

```
["id","name","email","age","active","score","tier","country","tags","bio","profile","metrics","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17…
```

### yaml-broad-100k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-narrow-100-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-narrow-1k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-narrow-5k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-narrow-25k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-narrow-50k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```

### yaml-narrow-100k-type-path · yq (oracle jqf)

expected (8 bytes, sha256 474728f5ad5e7f48…):

```
"array"

```

got (8 bytes, sha256 a2c06cd5295df0ec…):

```
"!!seq"

```
