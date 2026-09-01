# jqf benchmark

These numbers are a local snapshot for guidance, not a published result.

- jqf: pgo · `12293e9be9eef7f96dcee5b1d47a9c61869a7b4a`
- time: 2026-09-01T06:45:47Z
- diagnostics: `jqf: build=pgo profile=e0e45a21.970d2302.aarch64-apple-darwin.a2de6ca8 allocator=mimalloc platform=aarch64-macos pcores=6 ecores=12 pcore_source=detected`
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
| jqf-serial | 1.09× (median 1.00×) | 0.96× (median 1.00×) | 678 |
| jq | 2.65× (median 2.61×) | 1.48× (median 1.27×) | 406 |
| jaq | 1.54× (median 1.36×) | 1.74× (median 1.25×) | 598 |
| gojq | 2.22× (median 2.25×) | 2.30× (median 2.28×) | 492 |
| yq | 4.67× (median 3.97×) | 10.51× (median 7.71×) | 358 |
| dasel | 2.70× (median 3.21×) | 4.01× (median 3.84×) | 96 |
| mlr | 1.25× (median 1.65×) | 6.78× (median 7.37×) | 56 |

document = json/yaml/toml. streaming = ndjson/csv records.

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 1.00×) | 1.00× (median 1.00×) | 552 | 1.62× (median 1.09×) | 0.83× (median 1.00×) | 126 |
| jq | 2.44× (median 2.40×) | 2.39× (median 2.11×) | 336 | 3.90× (median 3.55×) | 0.15× (median 0.22×) | 70 |
| jaq | 1.44× (median 1.34×) | 1.99× (median 1.56×) | 528 | 2.55× (median 2.59×) | 0.63× (median 0.61×) | 70 |
| gojq | 2.08× (median 2.22×) | 2.65× (median 2.59×) | 433 | 3.57× (median 3.59×) | 0.81× (median 1.19×) | 59 |
| yq | 4.48× (median 3.90×) | 10.11× (median 7.61×) | 328 | 7.36× (median 6.93×) | 16.13× (median 15.08×) | 30 |
| dasel | 2.70× (median 3.21×) | 4.01× (median 3.84×) | 96 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.25× (median 1.65×) | 6.78× (median 7.37×) | 56 |

## geomean vs jqf · 100

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 0.99×) | 1.00× (median 1.00×) | 84 | 1.06× (median 1.03×) | 1.00× (median 1.00×) | 18 |
| jq | 1.06× (median 1.06×) | 0.63× (median 0.58×) | 48 | 1.11× (median 1.15×) | 0.55× (median 0.55×) | 10 |
| jaq | 0.98× (median 0.96×) | 0.89× (median 0.86×) | 80 | 1.02× (median 1.00×) | 0.81× (median 0.81×) | 10 |
| gojq | 1.12× (median 1.06×) | 1.40× (median 1.34×) | 66 | 1.19× (median 1.12×) | 1.38× (median 1.38×) | 10 |
| yq | 2.58× (median 2.20×) | 6.03× (median 6.27×) | 62 | 2.16× (median 1.99×) | 4.73× (median 4.62×) | 6 |
| dasel | 1.39× (median 1.36×) | 2.45× (median 2.59×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.92× (median 1.89×) | 6.70× (median 6.68×) | 8 |

## geomean vs jqf · 1k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.00× (median 0.99×) | 1.00× (median 1.00×) | 84 | 1.07× (median 1.05×) | 0.85× (median 1.00×) | 18 |
| jq | 1.51× (median 1.31×) | 1.06× (median 0.75×) | 48 | 1.60× (median 1.53×) | 0.37× (median 0.41×) | 10 |
| jaq | 1.15× (median 1.13×) | 1.21× (median 0.94×) | 80 | 1.12× (median 1.12×) | 0.61× (median 0.66×) | 10 |
| gojq | 1.50× (median 1.34×) | 1.76× (median 1.48×) | 64 | 1.51× (median 1.38×) | 1.40× (median 1.59×) | 9 |
| yq | 3.96× (median 2.64×) | 7.37× (median 7.26×) | 56 | 5.15× (median 7.03×) | 8.01× (median 8.90×) | 6 |
| dasel | 2.06× (median 2.39×) | 3.46× (median 3.67×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.90× (median 1.93×) | 7.07× (median 7.11×) | 8 |

## geomean vs jqf · 5k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.97× (median 0.97×) | 1.00× (median 1.00×) | 84 | 1.25× (median 1.08×) | 0.83× (median 1.00×) | 18 |
| jq | 2.08× (median 1.70×) | 1.83× (median 1.27×) | 48 | 2.49× (median 2.62×) | 0.27× (median 0.34×) | 10 |
| jaq | 1.32× (median 1.30×) | 1.71× (median 1.27×) | 80 | 1.64× (median 1.74×) | 0.61× (median 0.67×) | 10 |
| gojq | 1.93× (median 1.75×) | 2.29× (median 2.21×) | 64 | 2.07× (median 1.55×) | 1.49× (median 2.28×) | 8 |
| yq | 3.88× (median 3.44×) | 9.88× (median 9.75×) | 50 | 11.59× (median 15.85×) | 15.79× (median 18.35×) | 6 |
| dasel | 2.67× (median 3.17×) | 4.28× (median 4.23×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.77× (median 1.68×) | 7.27× (median 7.12×) | 8 |

## geomean vs jqf · 25k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.00× (median 1.00×) | 0.99× (median 1.00×) | 84 | 1.82× (median 2.21×) | 0.81× (median 0.80×) | 18 |
| jq | 3.01× (median 3.06×) | 3.15× (median 3.62×) | 48 | 5.53× (median 6.33×) | 0.14× (median 0.19×) | 10 |
| jaq | 1.61× (median 1.48×) | 2.42× (median 2.05×) | 80 | 3.27× (median 3.37×) | 0.61× (median 0.58×) | 10 |
| gojq | 2.53× (median 2.56×) | 3.01× (median 3.22×) | 64 | 5.10× (median 4.19×) | 0.90× (median 1.51×) | 8 |
| yq | 5.10× (median 5.05×) | 11.31× (median 11.55×) | 46 | 8.53× (median 6.15×) | 19.56× (median 18.19×) | 3 |
| dasel | 3.47× (median 3.62×) | 4.76× (median 4.59×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.16× (median 1.42×) | 6.94× (median 8.03×) | 8 |

## geomean vs jqf · 50k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.00× (median 1.00×) | 1.00× (median 1.00×) | 84 | 2.02× (median 2.44×) | 0.79× (median 0.89×) | 18 |
| jq | 3.46× (median 3.53×) | 4.14× (median 6.07×) | 48 | 6.84× (median 7.69×) | 0.09× (median 0.14×) | 10 |
| jaq | 1.71× (median 1.59×) | 2.77× (median 2.46×) | 80 | 4.05× (median 4.01×) | 0.59× (median 0.53×) | 10 |
| gojq | 2.73× (median 2.71×) | 3.55× (median 3.72×) | 64 | 6.68× (median 5.44×) | 0.63× (median 1.16×) | 8 |
| yq | 5.52× (median 5.00×) | 12.53× (median 11.44×) | 43 | 11.72× (median 7.06×) | 33.32× (median 30.73×) | 3 |
| dasel | 3.71× (median 3.76×) | 4.80× (median 4.59×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.03× (median 1.37×) | 6.82× (median 8.35×) | 8 |

## geomean vs jqf · 100k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.98× (median 1.00×) | 1.00× (median 1.00×) | 84 | 2.27× (median 2.96×) | 0.77× (median 0.94×) | 18 |
| jq | 3.76× (median 3.73×) | 4.93× (median 8.10×) | 48 | 8.25× (median 9.80×) | 0.06× (median 0.10×) | 10 |
| jaq | 1.80× (median 1.79×) | 3.06× (median 3.04×) | 80 | 4.89× (median 5.02×) | 0.58× (median 0.55×) | 10 |
| gojq | 2.86× (median 2.93×) | 3.92× (median 4.14×) | 64 | 8.30× (median 7.54×) | 0.42× (median 0.82×) | 8 |
| yq | 5.92× (median 5.10×) | 13.73× (median 9.95×) | 43 | 13.89× (median 8.46×) | 56.24× (median 51.52×) | 3 |
| dasel | 3.91× (median 3.96×) | 5.00× (median 4.74×) | 16 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 0.84× (median 1.19×) | 6.59× (median 8.09×) | 8 |

## geomean vs jqf · 200k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 1.00×) | 1.00× (median 1.00×) | 48 | 2.51× (median 4.13×) | 0.78× (median 0.96×) | 18 |
| jq | 4.03× (median 4.06×) | 5.59× (median 8.17×) | 48 | 9.88× (median 10.96×) | 0.04× (median 0.07×) | 10 |
| jaq | 2.00× (median 1.98×) | 4.60× (median 8.17×) | 48 | 5.83× (median 5.43×) | 0.61× (median 0.58×) | 10 |
| gojq | 3.00× (median 3.16×) | 4.68× (median 6.19×) | 47 | 10.24× (median 9.52×) | 0.27× (median 0.60×) | 8 |
| yq | 9.65× (median 9.44×) | 23.24× (median 31.94×) | 28 | 20.15× (median 9.83×) | 90.67× (median 82.98×) | 3 |
| dasel | n/a | n/a | 0 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 0.74× (median 0.98×) | 6.15× (median 7.74×) | 8 |

## results

| case | jqf | jqf-serial | jq | jaq | gojq | yq | dasel | mlr |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| csv-broad-100-count | 10.6 ms / 4.9 MB | 13.3 ms / 5.0 MB | n/a | n/a | n/a | 33.6 ms / 32 MB | n/a | 20.3 ms / 33 MB |
| csv-broad-100-first-id | 14.0 ms / 4.8 MB | 16.5 ms / 4.8 MB | n/a | n/a | n/a | 29.7 ms / 25 MB | n/a | 20.3 ms / 33 MB |
| csv-broad-100-high-count | 11.2 ms / 5.0 MB | 11.6 ms / 5.0 MB | n/a | n/a | n/a | 36.4 ms / 26 MB | n/a | 19.6 ms / 33 MB |
| csv-broad-100-sum-score | 11.5 ms / 5.0 MB | 12.5 ms / 5.0 MB | n/a | n/a | n/a | n/a | n/a | 21.5 ms / 33 MB |
| csv-broad-1k-count | 10.9 ms / 5.5 MB | 12.8 ms / 5.6 MB | n/a | n/a | n/a | 127 ms / 67 MB | n/a | 23.2 ms / 41 MB |
| csv-broad-1k-first-id | 10.4 ms / 5.5 MB | 10.2 ms / 5.5 MB | n/a | n/a | n/a | 127 ms / 63 MB | n/a | 21.7 ms / 41 MB |
| csv-broad-1k-high-count | 12.4 ms / 5.6 MB | 12.9 ms / 5.7 MB | n/a | n/a | n/a | 140 ms / 68 MB | n/a | 22.7 ms / 41 MB |
| csv-broad-1k-sum-score | 11.7 ms / 5.6 MB | 14.1 ms / 5.6 MB | n/a | n/a | n/a | n/a | n/a | 23.5 ms / 41 MB |
| csv-broad-5k-count | 19.0 ms / 8.2 MB | 19.8 ms / 8.2 MB | n/a | n/a | n/a | 505 ms / 229 MB | n/a | 31.8 ms / 68 MB |
| csv-broad-5k-first-id | 11.9 ms / 8.1 MB | 12.1 ms / 8.1 MB | n/a | n/a | n/a | 528 ms / 233 MB | n/a | 25.9 ms / 50 MB |
| csv-broad-5k-high-count | 19.9 ms / 8.3 MB | 23.2 ms / 8.3 MB | n/a | n/a | n/a | 550 ms / 273 MB | n/a | 31.0 ms / 63 MB |
| csv-broad-5k-sum-score | 19.4 ms / 8.2 MB | 18.0 ms / 8.3 MB | n/a | n/a | n/a | n/a | n/a | 32.6 ms / 66 MB |
| csv-broad-25k-count | 40.4 ms / 22 MB | 37.9 ms / 22 MB | n/a | n/a | n/a | excluded | n/a | 71.6 ms / 166 MB |
| csv-broad-25k-first-id | 12.6 ms / 21 MB | 14.6 ms / 21 MB | n/a | n/a | n/a | excluded | n/a | 23.2 ms / 50 MB |
| csv-broad-25k-high-count | 51.9 ms / 22 MB | 52.1 ms / 22 MB | n/a | n/a | n/a | excluded | n/a | 74.1 ms / 204 MB |
| csv-broad-25k-sum-score | 49.9 ms / 22 MB | 50.1 ms / 22 MB | n/a | n/a | n/a | n/a | n/a | 70.0 ms / 162 MB |
| csv-broad-50k-count | 63.8 ms / 38 MB | 63.5 ms / 38 MB | n/a | n/a | n/a | excluded | n/a | 121 ms / 281 MB |
| csv-broad-50k-first-id | 16.9 ms / 38 MB | 16.2 ms / 38 MB | n/a | n/a | n/a | excluded | n/a | 23.5 ms / 50 MB |
| csv-broad-50k-high-count | 90.3 ms / 38 MB | 92.4 ms / 38 MB | n/a | n/a | n/a | excluded | n/a | 123 ms / 341 MB |
| csv-broad-50k-sum-score | 87.8 ms / 38 MB | 87.4 ms / 38 MB | n/a | n/a | n/a | n/a | n/a | 121 ms / 299 MB |
| csv-broad-100k-count | 114 ms / 72 MB | 113 ms / 72 MB | n/a | n/a | n/a | excluded | n/a | 220 ms / 545 MB |
| csv-broad-100k-first-id | 22.0 ms / 72 MB | 20.8 ms / 72 MB | n/a | n/a | n/a | excluded | n/a | 23.7 ms / 50 MB |
| csv-broad-100k-high-count | 169 ms / 72 MB | 168 ms / 72 MB | n/a | n/a | n/a | excluded | n/a | 219 ms / 615 MB |
| csv-broad-100k-sum-score | 162 ms / 72 MB | 162 ms / 72 MB | n/a | n/a | n/a | n/a | n/a | 219 ms / 541 MB |
| csv-broad-200k-count | 214 ms / 139 MB | 217 ms / 139 MB | n/a | n/a | n/a | excluded | n/a | 412 ms / 1052 MB |
| csv-broad-200k-first-id | 35.8 ms / 139 MB | 29.3 ms / 139 MB | n/a | n/a | n/a | excluded | n/a | 25.3 ms / 50 MB |
| csv-broad-200k-high-count | 332 ms / 139 MB | 327 ms / 139 MB | n/a | n/a | n/a | excluded | n/a | 416 ms / 1094 MB |
| csv-broad-200k-sum-score | 312 ms / 139 MB | 313 ms / 139 MB | n/a | n/a | n/a | n/a | n/a | 424 ms / 1051 MB |
| csv-narrow-100-count | 8.56 ms / 4.8 MB | 8.09 ms / 4.8 MB | n/a | n/a | n/a | 13.3 ms / 19 MB | n/a | 19.6 ms / 32 MB |
| csv-narrow-100-first-id | 9.20 ms / 4.7 MB | 8.97 ms / 4.7 MB | n/a | n/a | n/a | 15.0 ms / 19 MB | n/a | 17.6 ms / 32 MB |
| csv-narrow-100-high-count | 9.50 ms / 4.9 MB | 16.3 ms / 4.9 MB | n/a | n/a | n/a | 17.7 ms / 20 MB | n/a | 17.8 ms / 32 MB |
| csv-narrow-100-sum-score | 8.50 ms / 4.9 MB | 9.18 ms / 4.9 MB | n/a | n/a | n/a | n/a | n/a | 21.4 ms / 32 MB |
| csv-narrow-1k-count | 11.1 ms / 4.8 MB | 9.66 ms / 4.9 MB | n/a | n/a | n/a | 21.8 ms / 30 MB | n/a | 16.9 ms / 33 MB |
| csv-narrow-1k-first-id | 10.8 ms / 4.8 MB | 9.14 ms / 4.8 MB | n/a | n/a | n/a | 22.7 ms / 24 MB | n/a | 19.2 ms / 33 MB |
| csv-narrow-1k-high-count | 9.42 ms / 4.9 MB | 10.3 ms / 5.0 MB | n/a | n/a | n/a | 26.8 ms / 25 MB | n/a | 17.5 ms / 33 MB |
| csv-narrow-1k-sum-score | 9.70 ms / 4.9 MB | 10.2 ms / 4.9 MB | n/a | n/a | n/a | n/a | n/a | 20.0 ms / 33 MB |
| csv-narrow-5k-count | 10.9 ms / 4.9 MB | 12.1 ms / 4.9 MB | n/a | n/a | n/a | 42.5 ms / 39 MB | n/a | 17.7 ms / 34 MB |
| csv-narrow-5k-first-id | 8.54 ms / 4.8 MB | 8.76 ms / 4.8 MB | n/a | n/a | n/a | 43.4 ms / 40 MB | n/a | 18.5 ms / 33 MB |
| csv-narrow-5k-high-count | 13.5 ms / 5.0 MB | 15.3 ms / 5.0 MB | n/a | n/a | n/a | 50.4 ms / 44 MB | n/a | 18.7 ms / 35 MB |
| csv-narrow-5k-sum-score | 12.3 ms / 4.9 MB | 12.5 ms / 5.0 MB | n/a | n/a | n/a | n/a | n/a | 25.1 ms / 35 MB |
| csv-narrow-25k-count | 24.9 ms / 5.1 MB | 22.7 ms / 5.1 MB | n/a | n/a | n/a | 146 ms / 93 MB | n/a | 19.2 ms / 43 MB |
| csv-narrow-25k-first-id | 8.41 ms / 5.0 MB | 8.66 ms / 5.0 MB | n/a | n/a | n/a | 144 ms / 90 MB | n/a | 17.3 ms / 33 MB |
| csv-narrow-25k-high-count | 31.5 ms / 5.2 MB | 33.2 ms / 5.2 MB | n/a | n/a | n/a | 194 ms / 118 MB | n/a | 19.4 ms / 46 MB |
| csv-narrow-25k-sum-score | 38.2 ms / 5.1 MB | 30.3 ms / 5.2 MB | n/a | n/a | n/a | n/a | n/a | 19.8 ms / 45 MB |
| csv-narrow-50k-count | 38.3 ms / 5.3 MB | 36.9 ms / 5.4 MB | n/a | n/a | n/a | 270 ms / 164 MB | n/a | 21.2 ms / 52 MB |
| csv-narrow-50k-first-id | 8.58 ms / 5.2 MB | 8.81 ms / 5.3 MB | n/a | n/a | n/a | 288 ms / 158 MB | n/a | 18.2 ms / 33 MB |
| csv-narrow-50k-high-count | 55.9 ms / 5.5 MB | 53.9 ms / 5.5 MB | n/a | n/a | n/a | 380 ms / 219 MB | n/a | 26.4 ms / 60 MB |
| csv-narrow-50k-sum-score | 53.9 ms / 5.4 MB | 53.4 ms / 5.4 MB | n/a | n/a | n/a | n/a | n/a | 25.2 ms / 56 MB |
| csv-narrow-100k-count | 64.0 ms / 5.9 MB | 62.2 ms / 5.9 MB | n/a | n/a | n/a | 542 ms / 286 MB | n/a | 27.4 ms / 66 MB |
| csv-narrow-100k-first-id | 12.3 ms / 5.8 MB | 17.8 ms / 5.8 MB | n/a | n/a | n/a | 547 ms / 299 MB | n/a | 21.2 ms / 33 MB |
| csv-narrow-100k-high-count | 101 ms / 6.0 MB | 101 ms / 6.0 MB | n/a | n/a | n/a | 720 ms / 424 MB | n/a | 29.0 ms / 87 MB |
| csv-narrow-100k-sum-score | 91.6 ms / 5.9 MB | 91.2 ms / 5.9 MB | n/a | n/a | n/a | n/a | n/a | 29.5 ms / 67 MB |
| csv-narrow-200k-count | 111 ms / 7.0 MB | 113 ms / 7.0 MB | n/a | n/a | n/a | 1088 ms / 561 MB | n/a | 33.3 ms / 84 MB |
| csv-narrow-200k-first-id | 9.22 ms / 6.9 MB | 13.0 ms / 6.9 MB | n/a | n/a | n/a | 1033 ms / 573 MB | n/a | 21.7 ms / 33 MB |
| csv-narrow-200k-high-count | 193 ms / 7.1 MB | 198 ms / 7.1 MB | n/a | n/a | n/a | 1434 ms / 799 MB | n/a | 43.8 ms / 113 MB |
| csv-narrow-200k-sum-score | 182 ms / 7.1 MB | 175 ms / 7.1 MB | n/a | n/a | n/a | n/a | n/a | 42.5 ms / 99 MB |
| ndjson-broad-100-first-id | 6.41 ms / 4.8 MB | 6.44 ms / 4.9 MB | 7.84 ms / 2.7 MB | 7.16 ms / 4.0 MB | 7.29 ms / 6.9 MB | n/a | n/a | n/a |
| ndjson-broad-100-identity | 6.68 ms / 4.8 MB | 7.16 ms / 4.8 MB | 9.42 ms / 2.7 MB | 7.07 ms / 4.0 MB | 8.71 ms / 7.3 MB | n/a | n/a | n/a |
| ndjson-broad-100-score | 6.25 ms / 4.9 MB | 6.14 ms / 4.9 MB | 7.20 ms / 2.7 MB | 6.20 ms / 4.0 MB | 8.32 ms / 6.8 MB | n/a | n/a | n/a |
| ndjson-broad-100-select-id | 6.51 ms / 5.0 MB | 6.66 ms / 5.0 MB | 7.65 ms / 2.7 MB | 6.53 ms / 4.0 MB | 8.40 ms / 7.1 MB | n/a | n/a | n/a |
| ndjson-broad-100-select-score | 7.18 ms / 5.0 MB | 7.00 ms / 5.0 MB | 9.36 ms / 2.7 MB | 8.06 ms / 4.0 MB | 12.3 ms / 7.3 MB | n/a | n/a | n/a |
| ndjson-broad-1k-first-id | 7.85 ms / 9.5 MB | 8.99 ms / 5.8 MB | 16.4 ms / 2.7 MB | 10.3 ms / 4.9 MB | 17.0 ms / 11 MB | n/a | n/a | n/a |
| ndjson-broad-1k-identity | 9.30 ms / 12 MB | 10.7 ms / 5.8 MB | 36.9 ms / 2.8 MB | 14.7 ms / 4.9 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-1k-score | 7.54 ms / 9.6 MB | 10.4 ms / 5.8 MB | 16.5 ms / 2.7 MB | 10.2 ms / 4.9 MB | 16.0 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-1k-select-id | 8.07 ms / 10 MB | 10.4 ms / 5.9 MB | 15.9 ms / 2.8 MB | 10.0 ms / 5.0 MB | 14.8 ms / 11 MB | n/a | n/a | n/a |
| ndjson-broad-1k-select-score | 9.74 ms / 12 MB | 14.4 ms / 6.0 MB | 36.1 ms / 2.8 MB | 13.4 ms / 5.0 MB | 20.5 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-5k-first-id | 11.5 ms / 18 MB | 20.5 ms / 10 MB | 50.2 ms / 2.7 MB | 26.4 ms / 9.2 MB | 49.8 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-identity | 13.7 ms / 24 MB | 30.9 ms / 10 MB | 146 ms / 2.8 MB | 45.5 ms / 9.2 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-5k-score | 11.2 ms / 17 MB | 20.3 ms / 10 MB | 50.3 ms / 2.7 MB | 26.5 ms / 9.2 MB | 49.8 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-select-id | 13.3 ms / 19 MB | 23.9 ms / 10 MB | 51.9 ms / 2.7 MB | 26.1 ms / 9.3 MB | 47.6 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-select-score | 15.8 ms / 23 MB | 47.7 ms / 10 MB | 134 ms / 2.8 MB | 44.9 ms / 9.3 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-25k-first-id | 20.7 ms / 40 MB | 68.6 ms / 32 MB | 215 ms / 2.8 MB | 100 ms / 31 MB | 219 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-25k-identity | 26.1 ms / 61 MB | 113 ms / 32 MB | 653 ms / 2.8 MB | 186 ms / 31 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-25k-score | 20.7 ms / 40 MB | 67.8 ms / 32 MB | 212 ms / 2.8 MB | 99.4 ms / 31 MB | 208 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-25k-select-id | 22.2 ms / 41 MB | 81.7 ms / 32 MB | 212 ms / 2.7 MB | 94.2 ms / 31 MB | 193 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-25k-select-score | 39.6 ms / 61 MB | 200 ms / 32 MB | 606 ms / 2.8 MB | 181 ms / 31 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-50k-first-id | 29.3 ms / 66 MB | 125 ms / 59 MB | 412 ms / 2.8 MB | 191 ms / 58 MB | 407 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-identity | 45.9 ms / 110 MB | 229 ms / 59 MB | 1284 ms / 2.8 MB | 367 ms / 58 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-50k-score | 29.7 ms / 66 MB | 125 ms / 59 MB | 413 ms / 2.8 MB | 189 ms / 58 MB | 415 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-50k-select-id | 35.6 ms / 66 MB | 165 ms / 59 MB | 416 ms / 2.7 MB | 187 ms / 58 MB | 411 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-50k-select-score | 72.1 ms / 106 MB | 393 ms / 59 MB | 1217 ms / 2.8 MB | 364 ms / 58 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-100k-first-id | 50.8 ms / 121 MB | 244 ms / 113 MB | 873 ms / 2.7 MB | 383 ms / 112 MB | 878 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-100k-identity | 74.1 ms / 184 MB | 422 ms / 113 MB | 2515 ms / 2.8 MB | 717 ms / 112 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-100k-score | 49.9 ms / 120 MB | 248 ms / 113 MB | 816 ms / 2.8 MB | 369 ms / 112 MB | 847 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-100k-select-id | 57.5 ms / 122 MB | 310 ms / 113 MB | 832 ms / 2.7 MB | 359 ms / 112 MB | 753 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-100k-select-score | 122 ms / 187 MB | 771 ms / 113 MB | 2369 ms / 2.8 MB | 721 ms / 112 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-200k-first-id | 90.1 ms / 230 MB | 511 ms / 221 MB | 1623 ms / 2.7 MB | 736 ms / 220 MB | 1636 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-200k-identity | 136 ms / 317 MB | 839 ms / 221 MB | 4990 ms / 2.8 MB | 1427 ms / 220 MB | disagreed | n/a | n/a | n/a |
| ndjson-broad-200k-score | 90.0 ms / 228 MB | 512 ms / 221 MB | 1699 ms / 2.8 MB | 763 ms / 220 MB | 1663 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-select-id | 106 ms / 230 MB | 602 ms / 221 MB | 1624 ms / 2.7 MB | 698 ms / 220 MB | 1468 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-select-score | 243 ms / 357 MB | 1566 ms / 221 MB | 4703 ms / 2.8 MB | 1389 ms / 220 MB | disagreed | n/a | n/a | n/a |
| ndjson-narrow-100-first-id | 6.00 ms / 4.8 MB | 6.30 ms / 4.7 MB | 6.88 ms / 2.6 MB | 7.74 ms / 3.8 MB | 6.37 ms / 6.3 MB | n/a | n/a | n/a |
| ndjson-narrow-100-identity | 6.04 ms / 4.5 MB | 6.20 ms / 4.5 MB | 6.25 ms / 2.6 MB | 6.00 ms / 3.8 MB | 6.60 ms / 6.2 MB | n/a | n/a | n/a |
| ndjson-narrow-100-score | 6.33 ms / 4.8 MB | 5.96 ms / 4.8 MB | 5.82 ms / 2.6 MB | 5.65 ms / 3.8 MB | 6.38 ms / 6.1 MB | n/a | n/a | n/a |
| ndjson-narrow-100-select-id | 6.33 ms / 4.8 MB | 6.62 ms / 4.8 MB | 6.32 ms / 2.6 MB | 6.01 ms / 3.9 MB | 6.24 ms / 6.4 MB | n/a | n/a | n/a |
| ndjson-narrow-100-select-score | 7.16 ms / 4.8 MB | 6.28 ms / 4.8 MB | 6.20 ms / 2.6 MB | 5.99 ms / 3.9 MB | 7.86 ms / 6.2 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-first-id | 7.03 ms / 4.8 MB | 6.54 ms / 4.8 MB | 6.52 ms / 2.6 MB | 6.55 ms / 3.8 MB | 8.98 ms / 7.8 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-identity | 8.55 ms / 4.6 MB | 7.48 ms / 4.6 MB | 7.50 ms / 2.6 MB | 7.02 ms / 3.8 MB | 8.07 ms / 7.8 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-score | 6.93 ms / 4.8 MB | 6.91 ms / 4.8 MB | 6.64 ms / 2.6 MB | 6.29 ms / 3.8 MB | 7.43 ms / 7.6 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-select-id | 7.31 ms / 4.8 MB | 7.55 ms / 4.8 MB | 7.18 ms / 2.6 MB | 6.64 ms / 3.9 MB | 9.36 ms / 8.4 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-select-score | 6.69 ms / 4.8 MB | 6.51 ms / 4.8 MB | 7.27 ms / 2.6 MB | 6.70 ms / 3.9 MB | 9.26 ms / 8.6 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-first-id | 9.91 ms / 4.9 MB | 10.0 ms / 4.9 MB | 9.62 ms / 2.6 MB | 15.1 ms / 3.9 MB | 13.6 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-identity | 8.35 ms / 4.8 MB | 8.38 ms / 4.8 MB | 11.2 ms / 2.7 MB | 9.91 ms / 3.9 MB | 14.1 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-score | 10.1 ms / 4.9 MB | 11.2 ms / 4.9 MB | 10.1 ms / 2.6 MB | 9.55 ms / 3.9 MB | 14.2 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-select-id | 9.81 ms / 4.9 MB | 9.94 ms / 4.9 MB | 9.08 ms / 2.6 MB | 9.17 ms / 4.0 MB | 11.1 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-select-score | 11.7 ms / 5.0 MB | 10.2 ms / 5.0 MB | 12.6 ms / 2.7 MB | 10.4 ms / 4.0 MB | 15.8 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-first-id | 11.7 ms / 7.6 MB | 27.6 ms / 5.4 MB | 22.2 ms / 2.7 MB | 24.0 ms / 4.4 MB | 41.3 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-identity | 9.79 ms / 8.1 MB | 20.6 ms / 5.2 MB | 30.2 ms / 2.7 MB | 24.4 ms / 4.4 MB | 44.3 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-score | 11.4 ms / 7.6 MB | 26.4 ms / 5.4 MB | 22.7 ms / 2.7 MB | 24.3 ms / 4.4 MB | 40.9 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-select-id | 11.4 ms / 7.3 MB | 26.6 ms / 5.4 MB | 23.6 ms / 2.6 MB | 18.9 ms / 4.5 MB | 25.6 ms / 14 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-select-score | 12.4 ms / 8.4 MB | 29.2 ms / 5.4 MB | 35.0 ms / 2.7 MB | 29.4 ms / 4.5 MB | 47.7 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-first-id | 14.1 ms / 10 MB | 44.3 ms / 6.0 MB | 37.7 ms / 2.7 MB | 41.7 ms / 5.0 MB | 73.6 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-identity | 14.8 ms / 11 MB | 31.9 ms / 5.8 MB | 54.7 ms / 2.7 MB | 41.7 ms / 5.0 MB | 83.5 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-score | 15.4 ms / 10 MB | 44.4 ms / 6.0 MB | 38.0 ms / 2.7 MB | 41.5 ms / 5.0 MB | 72.9 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-select-id | 16.2 ms / 9.4 MB | 44.1 ms / 6.0 MB | 40.8 ms / 2.6 MB | 33.3 ms / 5.0 MB | 43.0 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-select-score | 17.9 ms / 12 MB | 49.7 ms / 6.0 MB | 61.1 ms / 2.7 MB | 52.8 ms / 5.0 MB | 86.0 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-first-id | 27.7 ms / 14 MB | 80.2 ms / 7.1 MB | 67.9 ms / 2.7 MB | 76.0 ms / 6.2 MB | 139 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-identity | 18.4 ms / 16 MB | 55.6 ms / 7.0 MB | 94.2 ms / 2.7 MB | 75.7 ms / 6.2 MB | 147 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-score | 22.1 ms / 14 MB | 81.3 ms / 7.1 MB | 68.2 ms / 2.7 MB | 78.4 ms / 6.2 MB | 140 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-select-id | 23.6 ms / 13 MB | 79.9 ms / 7.1 MB | 70.7 ms / 2.7 MB | 58.3 ms / 6.2 MB | 77.0 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-select-score | 24.0 ms / 16 MB | 91.5 ms / 7.2 MB | 112 ms / 2.7 MB | 95.9 ms / 6.2 MB | 169 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-first-id | 32.4 ms / 20 MB | 152 ms / 9.6 MB | 127 ms / 2.7 MB | 143 ms / 8.7 MB | 263 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-identity | 27.6 ms / 21 MB | 102 ms / 9.4 MB | 184 ms / 2.7 MB | 142 ms / 8.7 MB | 284 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-score | 32.3 ms / 18 MB | 152 ms / 9.6 MB | 125 ms / 2.7 MB | 143 ms / 8.7 MB | 262 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-select-id | 32.7 ms / 16 MB | 149 ms / 9.6 MB | 132 ms / 2.7 MB | 109 ms / 8.7 MB | 144 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-select-score | 35.6 ms / 22 MB | 172 ms / 9.6 MB | 209 ms / 2.7 MB | 180 ms / 8.7 MB | 311 ms / 13 MB | n/a | n/a | n/a |
| toml-broad-100-count | 8.41 ms / 4.9 MB | 10.0 ms / 4.9 MB | n/a | 16.3 ms / 5.3 MB | n/a | 143 ms / 39 MB | 11.4 ms / 14 MB | n/a |
| toml-broad-100-descent | 8.94 ms / 6.3 MB | 9.40 ms / 6.3 MB | n/a | 10.3 ms / 5.5 MB | n/a | 149 ms / 40 MB | n/a | n/a |
| toml-broad-100-exact-name | 8.57 ms / 4.8 MB | 9.57 ms / 4.8 MB | n/a | 10.5 ms / 5.2 MB | n/a | 148 ms / 44 MB | 12.1 ms / 13 MB | n/a |
| toml-broad-100-first-id | 8.32 ms / 4.8 MB | 8.06 ms / 4.8 MB | n/a | 9.78 ms / 5.3 MB | n/a | 144 ms / 39 MB | 11.8 ms / 13 MB | n/a |
| toml-broad-100-identity | 9.13 ms / 6.1 MB | 9.68 ms / 6.2 MB | n/a | 10.6 ms / 5.2 MB | n/a | 148 ms / 44 MB | 13.9 ms / 15 MB | n/a |
| toml-broad-100-ids | 8.20 ms / 5.1 MB | 10.1 ms / 5.1 MB | n/a | disagreed | n/a | disagreed | n/a | n/a |
| toml-broad-100-keys-publish | 8.41 ms / 5.0 MB | 8.40 ms / 5.0 MB | n/a | disagreed | n/a | disagreed | n/a | n/a |
| toml-broad-100-nested-dept | 8.59 ms / 4.8 MB | 8.87 ms / 4.8 MB | n/a | 9.25 ms / 5.3 MB | n/a | 144 ms / 43 MB | 11.3 ms / 14 MB | n/a |
| toml-broad-100-type-path | 11.3 ms / 6.9 MB | 9.84 ms / 6.9 MB | n/a | 10.5 ms / 5.2 MB | n/a | disagreed | n/a | n/a |
| toml-broad-1k-count | 13.1 ms / 7.2 MB | 12.6 ms / 7.3 MB | n/a | 26.9 ms / 15 MB | n/a | excluded | 33.0 ms / 32 MB | n/a |
| toml-broad-1k-descent | 19.7 ms / 21 MB | 19.0 ms / 21 MB | n/a | 27.7 ms / 17 MB | n/a | excluded | n/a | n/a |
| toml-broad-1k-exact-name | 14.0 ms / 6.5 MB | 15.0 ms / 6.5 MB | n/a | 24.4 ms / 15 MB | n/a | excluded | 32.0 ms / 32 MB | n/a |
| toml-broad-1k-first-id | 12.9 ms / 6.5 MB | 12.8 ms / 6.5 MB | n/a | 28.5 ms / 15 MB | n/a | excluded | 35.1 ms / 32 MB | n/a |
| toml-broad-1k-identity | 21.5 ms / 21 MB | 22.0 ms / 21 MB | n/a | 28.9 ms / 15 MB | n/a | excluded | 53.7 ms / 43 MB | n/a |
| toml-broad-1k-ids | 14.7 ms / 7.3 MB | 14.8 ms / 7.4 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-1k-keys-publish | 23.0 ms / 6.6 MB | 21.9 ms / 6.6 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-1k-nested-dept | 15.7 ms / 6.5 MB | 15.5 ms / 6.5 MB | n/a | 26.6 ms / 15 MB | n/a | excluded | 32.8 ms / 32 MB | n/a |
| toml-broad-1k-type-path | 25.2 ms / 26 MB | 29.1 ms / 26 MB | n/a | 25.9 ms / 15 MB | n/a | excluded | n/a | n/a |
| toml-broad-5k-count | 37.4 ms / 17 MB | 34.7 ms / 17 MB | n/a | 85.4 ms / 70 MB | n/a | excluded | 105 ms / 107 MB | n/a |
| toml-broad-5k-descent | 61.0 ms / 78 MB | 59.4 ms / 78 MB | n/a | 101 ms / 74 MB | n/a | excluded | n/a | n/a |
| toml-broad-5k-exact-name | 33.9 ms / 14 MB | 33.9 ms / 14 MB | n/a | 84.5 ms / 70 MB | n/a | excluded | 107 ms / 107 MB | n/a |
| toml-broad-5k-first-id | 33.1 ms / 14 MB | 34.3 ms / 14 MB | n/a | 87.7 ms / 70 MB | n/a | excluded | 107 ms / 107 MB | n/a |
| toml-broad-5k-identity | 61.9 ms / 78 MB | 56.2 ms / 78 MB | n/a | 103 ms / 70 MB | n/a | excluded | 206 ms / 157 MB | n/a |
| toml-broad-5k-ids | 34.6 ms / 17 MB | 34.8 ms / 17 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-5k-keys-publish | 34.9 ms / 14 MB | 32.9 ms / 14 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-5k-nested-dept | 32.8 ms / 14 MB | 35.6 ms / 14 MB | n/a | 85.5 ms / 70 MB | n/a | excluded | 105 ms / 107 MB | n/a |
| toml-broad-5k-type-path | 79.3 ms / 111 MB | 79.5 ms / 113 MB | n/a | 86.8 ms / 70 MB | n/a | excluded | n/a | n/a |
| toml-broad-25k-count | 122 ms / 72 MB | 124 ms / 68 MB | n/a | 375 ms / 285 MB | n/a | excluded | 457 ms / 509 MB | n/a |
| toml-broad-25k-descent | 240 ms / 321 MB | 241 ms / 305 MB | n/a | 442 ms / 332 MB | n/a | excluded | n/a | n/a |
| toml-broad-25k-exact-name | 116 ms / 50 MB | 118 ms / 49 MB | n/a | 372 ms / 284 MB | n/a | excluded | 480 ms / 509 MB | n/a |
| toml-broad-25k-first-id | 120 ms / 50 MB | 120 ms / 49 MB | n/a | 374 ms / 284 MB | n/a | excluded | 484 ms / 516 MB | n/a |
| toml-broad-25k-identity | 230 ms / 321 MB | 230 ms / 304 MB | n/a | 464 ms / 285 MB | n/a | excluded | 978 ms / 810 MB | n/a |
| toml-broad-25k-ids | 119 ms / 72 MB | 120 ms / 68 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-25k-keys-publish | 133 ms / 51 MB | 138 ms / 49 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-25k-nested-dept | 117 ms / 50 MB | 119 ms / 49 MB | n/a | 373 ms / 284 MB | n/a | excluded | 456 ms / 507 MB | n/a |
| toml-broad-25k-type-path | 372 ms / 441 MB | 372 ms / 425 MB | n/a | 377 ms / 285 MB | n/a | excluded | n/a | n/a |
| toml-broad-50k-count | 254 ms / 134 MB | 242 ms / 134 MB | n/a | 786 ms / 545 MB | n/a | excluded | 1003 ms / 961 MB | n/a |
| toml-broad-50k-descent | 479 ms / 678 MB | 504 ms / 678 MB | n/a | 939 ms / 630 MB | n/a | excluded | n/a | n/a |
| toml-broad-50k-exact-name | 234 ms / 93 MB | 228 ms / 93 MB | n/a | 751 ms / 545 MB | n/a | excluded | 958 ms / 964 MB | n/a |
| toml-broad-50k-first-id | 241 ms / 93 MB | 242 ms / 92 MB | n/a | 805 ms / 545 MB | n/a | excluded | 937 ms / 965 MB | n/a |
| toml-broad-50k-identity | 496 ms / 678 MB | 485 ms / 678 MB | n/a | 956 ms / 545 MB | n/a | excluded | 2031 ms / 1497 MB | n/a |
| toml-broad-50k-ids | 238 ms / 134 MB | 235 ms / 134 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-50k-keys-publish | 236 ms / 93 MB | 246 ms / 93 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-50k-nested-dept | 229 ms / 93 MB | 227 ms / 93 MB | n/a | 753 ms / 545 MB | n/a | excluded | 919 ms / 964 MB | n/a |
| toml-broad-50k-type-path | 684 ms / 903 MB | 709 ms / 903 MB | n/a | 776 ms / 545 MB | n/a | excluded | n/a | n/a |
| toml-broad-100k-count | 478 ms / 254 MB | 468 ms / 254 MB | n/a | 1525 ms / 1087 MB | n/a | excluded | 1887 ms / 1912 MB | n/a |
| toml-broad-100k-descent | 1002 ms / 1324 MB | 953 ms / 1324 MB | n/a | 1776 ms / 1273 MB | n/a | excluded | n/a | n/a |
| toml-broad-100k-exact-name | 445 ms / 180 MB | 435 ms / 180 MB | n/a | 1513 ms / 1086 MB | n/a | excluded | 1960 ms / 1919 MB | n/a |
| toml-broad-100k-first-id | 440 ms / 180 MB | 438 ms / 180 MB | n/a | 1486 ms / 1086 MB | n/a | excluded | 1881 ms / 1914 MB | n/a |
| toml-broad-100k-identity | 930 ms / 1325 MB | 945 ms / 1325 MB | n/a | 1875 ms / 1086 MB | n/a | excluded | 3970 ms / 3152 MB | n/a |
| toml-broad-100k-ids | 499 ms / 254 MB | 506 ms / 254 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-100k-keys-publish | 471 ms / 181 MB | 452 ms / 181 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-broad-100k-nested-dept | 472 ms / 180 MB | 468 ms / 180 MB | n/a | 1507 ms / 1086 MB | n/a | excluded | 1916 ms / 1906 MB | n/a |
| toml-broad-100k-type-path | 1426 ms / 1747 MB | 1391 ms / 1746 MB | n/a | 1463 ms / 1086 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100-count | 9.72 ms / 4.7 MB | 8.65 ms / 4.7 MB | n/a | 8.31 ms / 4.2 MB | n/a | 28.5 ms / 29 MB | 10.7 ms / 9.7 MB | n/a |
| toml-narrow-100-descent | 11.9 ms / 5.0 MB | 12.5 ms / 5.0 MB | n/a | 13.5 ms / 4.2 MB | n/a | 25.2 ms / 29 MB | n/a | n/a |
| toml-narrow-100-exact-name | 9.77 ms / 4.7 MB | 9.85 ms / 4.7 MB | n/a | 11.5 ms / 4.1 MB | n/a | 27.1 ms / 30 MB | error | n/a |
| toml-narrow-100-first-id | 9.15 ms / 4.7 MB | 9.34 ms / 4.7 MB | n/a | 9.52 ms / 4.1 MB | n/a | 23.5 ms / 29 MB | 11.4 ms / 9.8 MB | n/a |
| toml-narrow-100-identity | 11.3 ms / 4.8 MB | 11.5 ms / 4.8 MB | n/a | 13.3 ms / 4.1 MB | n/a | 26.5 ms / 29 MB | 14.8 ms / 10 MB | n/a |
| toml-narrow-100-ids | 11.3 ms / 4.8 MB | 9.79 ms / 4.8 MB | n/a | disagreed | n/a | disagreed | n/a | n/a |
| toml-narrow-100-keys-publish | 9.53 ms / 4.9 MB | 9.72 ms / 4.9 MB | n/a | disagreed | n/a | disagreed | n/a | n/a |
| toml-narrow-100-nested-dept | 9.61 ms / 4.7 MB | 8.57 ms / 4.7 MB | n/a | 8.99 ms / 4.1 MB | n/a | 25.5 ms / 29 MB | error | n/a |
| toml-narrow-100-type-path | 9.73 ms / 4.8 MB | 9.57 ms / 4.8 MB | n/a | 9.97 ms / 4.1 MB | n/a | disagreed | n/a | n/a |
| toml-narrow-1k-count | 11.3 ms / 5.0 MB | 15.3 ms / 5.1 MB | n/a | 13.3 ms / 5.8 MB | n/a | 802 ms / 37 MB | 14.8 ms / 13 MB | n/a |
| toml-narrow-1k-descent | 11.5 ms / 5.8 MB | 12.4 ms / 5.8 MB | n/a | 13.0 ms / 5.8 MB | n/a | 732 ms / 37 MB | n/a | n/a |
| toml-narrow-1k-exact-name | 10.2 ms / 5.0 MB | 9.28 ms / 5.0 MB | n/a | 10.4 ms / 5.6 MB | n/a | 729 ms / 36 MB | error | n/a |
| toml-narrow-1k-first-id | 9.53 ms / 5.0 MB | 9.32 ms / 5.0 MB | n/a | 9.78 ms / 5.7 MB | n/a | 823 ms / 38 MB | 14.9 ms / 13 MB | n/a |
| toml-narrow-1k-identity | 10.9 ms / 5.6 MB | 11.2 ms / 5.6 MB | n/a | 16.0 ms / 5.7 MB | n/a | 725 ms / 36 MB | 13.5 ms / 14 MB | n/a |
| toml-narrow-1k-ids | 12.9 ms / 5.2 MB | 14.8 ms / 5.2 MB | n/a | disagreed | n/a | disagreed | n/a | n/a |
| toml-narrow-1k-keys-publish | 10.4 ms / 5.2 MB | 11.4 ms / 5.2 MB | n/a | disagreed | n/a | disagreed | n/a | n/a |
| toml-narrow-1k-nested-dept | 9.51 ms / 5.0 MB | 8.46 ms / 5.0 MB | n/a | 9.79 ms / 5.5 MB | n/a | 707 ms / 37 MB | error | n/a |
| toml-narrow-1k-type-path | 11.2 ms / 5.7 MB | 12.3 ms / 5.7 MB | n/a | 11.3 ms / 5.7 MB | n/a | disagreed | n/a | n/a |
| toml-narrow-5k-count | 12.2 ms / 6.7 MB | 11.5 ms / 6.7 MB | n/a | 15.6 ms / 11 MB | n/a | excluded | 21.2 ms / 19 MB | n/a |
| toml-narrow-5k-descent | 13.6 ms / 9.9 MB | 13.8 ms / 9.9 MB | n/a | 15.9 ms / 11 MB | n/a | excluded | n/a | n/a |
| toml-narrow-5k-exact-name | 11.2 ms / 6.6 MB | 10.4 ms / 6.6 MB | n/a | 14.5 ms / 11 MB | n/a | excluded | error | n/a |
| toml-narrow-5k-first-id | 12.3 ms / 6.6 MB | 10.3 ms / 6.6 MB | n/a | 15.9 ms / 11 MB | n/a | excluded | 18.9 ms / 19 MB | n/a |
| toml-narrow-5k-identity | 12.3 ms / 9.8 MB | 13.3 ms / 9.7 MB | n/a | 15.8 ms / 11 MB | n/a | excluded | 26.7 ms / 23 MB | n/a |
| toml-narrow-5k-ids | 13.5 ms / 6.9 MB | 13.3 ms / 6.9 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-5k-keys-publish | 10.1 ms / 6.8 MB | 9.70 ms / 6.8 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-5k-nested-dept | 15.3 ms / 6.6 MB | 13.5 ms / 6.6 MB | n/a | 16.1 ms / 11 MB | n/a | excluded | error | n/a |
| toml-narrow-5k-type-path | 12.5 ms / 11 MB | 12.0 ms / 11 MB | n/a | 13.1 ms / 11 MB | n/a | excluded | n/a | n/a |
| toml-narrow-25k-count | 16.8 ms / 14 MB | 16.8 ms / 14 MB | n/a | 34.3 ms / 50 MB | n/a | excluded | 47.8 ms / 46 MB | n/a |
| toml-narrow-25k-descent | 25.4 ms / 27 MB | 26.7 ms / 27 MB | n/a | 38.0 ms / 51 MB | n/a | excluded | n/a | n/a |
| toml-narrow-25k-exact-name | 13.4 ms / 14 MB | 15.9 ms / 14 MB | n/a | 31.2 ms / 50 MB | n/a | excluded | error | n/a |
| toml-narrow-25k-first-id | 15.8 ms / 14 MB | 16.9 ms / 14 MB | n/a | 34.9 ms / 50 MB | n/a | excluded | 50.6 ms / 47 MB | n/a |
| toml-narrow-25k-identity | 21.8 ms / 26 MB | 24.0 ms / 27 MB | n/a | 37.0 ms / 50 MB | n/a | excluded | 77.3 ms / 68 MB | n/a |
| toml-narrow-25k-ids | 20.9 ms / 14 MB | 17.6 ms / 14 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-25k-keys-publish | 13.7 ms / 14 MB | 13.4 ms / 14 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-25k-nested-dept | 13.8 ms / 14 MB | 16.7 ms / 14 MB | n/a | 31.9 ms / 50 MB | n/a | excluded | error | n/a |
| toml-narrow-25k-type-path | 26.7 ms / 32 MB | 30.2 ms / 32 MB | n/a | 33.2 ms / 50 MB | n/a | excluded | n/a | n/a |
| toml-narrow-50k-count | 22.3 ms / 24 MB | 24.8 ms / 24 MB | n/a | 51.4 ms / 80 MB | n/a | excluded | 79.4 ms / 81 MB | n/a |
| toml-narrow-50k-descent | 39.2 ms / 55 MB | 37.8 ms / 55 MB | n/a | 59.8 ms / 80 MB | n/a | excluded | n/a | n/a |
| toml-narrow-50k-exact-name | 29.7 ms / 24 MB | 21.6 ms / 24 MB | n/a | 52.5 ms / 80 MB | n/a | excluded | error | n/a |
| toml-narrow-50k-first-id | 22.2 ms / 24 MB | 23.8 ms / 24 MB | n/a | 51.5 ms / 80 MB | n/a | excluded | 82.9 ms / 83 MB | n/a |
| toml-narrow-50k-identity | 35.5 ms / 55 MB | 33.9 ms / 55 MB | n/a | 61.2 ms / 80 MB | n/a | excluded | 139 ms / 121 MB | n/a |
| toml-narrow-50k-ids | 22.9 ms / 24 MB | 22.5 ms / 24 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-50k-keys-publish | 22.4 ms / 24 MB | 21.7 ms / 24 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-50k-nested-dept | 21.7 ms / 24 MB | 23.5 ms / 24 MB | n/a | 54.0 ms / 80 MB | n/a | excluded | error | n/a |
| toml-narrow-50k-type-path | 44.7 ms / 64 MB | 43.2 ms / 64 MB | n/a | 52.1 ms / 80 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100k-count | 33.9 ms / 43 MB | 32.8 ms / 43 MB | n/a | 94.0 ms / 139 MB | n/a | excluded | 143 ms / 160 MB | n/a |
| toml-narrow-100k-descent | 59.5 ms / 94 MB | 59.8 ms / 94 MB | n/a | 107 ms / 139 MB | n/a | excluded | n/a | n/a |
| toml-narrow-100k-exact-name | 32.3 ms / 42 MB | 30.0 ms / 42 MB | n/a | 90.3 ms / 139 MB | n/a | excluded | error | n/a |
| toml-narrow-100k-first-id | 32.1 ms / 42 MB | 31.0 ms / 42 MB | n/a | 93.8 ms / 139 MB | n/a | excluded | 150 ms / 157 MB | n/a |
| toml-narrow-100k-identity | 55.1 ms / 94 MB | 53.9 ms / 94 MB | n/a | 111 ms / 139 MB | n/a | excluded | 254 ms / 225 MB | n/a |
| toml-narrow-100k-ids | 32.7 ms / 43 MB | 31.1 ms / 43 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-100k-keys-publish | 31.3 ms / 42 MB | 29.9 ms / 42 MB | n/a | disagreed | n/a | excluded | n/a | n/a |
| toml-narrow-100k-nested-dept | 30.5 ms / 42 MB | 30.5 ms / 42 MB | n/a | 93.4 ms / 139 MB | n/a | excluded | error | n/a |
| toml-narrow-100k-type-path | 72.2 ms / 126 MB | 73.4 ms / 126 MB | n/a | 91.3 ms / 139 MB | n/a | excluded | n/a | n/a |
| users-broad-100-all-nonneg | 4.95 ms / 4.9 MB | 4.95 ms / 4.9 MB | 5.54 ms / 3.6 MB | 4.64 ms / 5.0 MB | 5.30 ms / 7.1 MB | n/a | n/a | n/a |
| users-broad-100-any-high | 4.57 ms / 4.9 MB | 4.74 ms / 4.9 MB | 5.37 ms / 3.6 MB | 4.68 ms / 5.0 MB | 5.35 ms / 7.2 MB | n/a | n/a | n/a |
| users-broad-100-count | 4.82 ms / 4.6 MB | 4.74 ms / 4.6 MB | 5.39 ms / 3.6 MB | 4.70 ms / 4.8 MB | 5.24 ms / 7.1 MB | 11.3 ms / 23 MB | n/a | n/a |
| users-broad-100-descent | 5.22 ms / 6.0 MB | 5.23 ms / 6.0 MB | 6.17 ms / 4.1 MB | 5.06 ms / 5.0 MB | 7.96 ms / 10 MB | 16.1 ms / 53 MB | n/a | n/a |
| users-broad-100-filter-active | 5.04 ms / 4.8 MB | 5.52 ms / 4.8 MB | 5.89 ms / 3.6 MB | 4.78 ms / 4.8 MB | 5.53 ms / 7.0 MB | 12.0 ms / 40 MB | n/a | n/a |
| users-broad-100-first-id | 4.71 ms / 4.7 MB | 4.45 ms / 4.7 MB | 5.22 ms / 3.6 MB | 6.16 ms / 4.7 MB | 6.75 ms / 7.0 MB | 10.2 ms / 37 MB | n/a | n/a |
| users-broad-100-group-mod | 5.35 ms / 6.4 MB | 5.18 ms / 6.4 MB | 5.47 ms / 3.7 MB | 4.61 ms / 5.0 MB | 5.48 ms / 7.1 MB | 12.9 ms / 53 MB | n/a | n/a |
| users-broad-100-high-score | 4.74 ms / 4.8 MB | 4.58 ms / 4.8 MB | 5.46 ms / 3.6 MB | 4.91 ms / 4.8 MB | 5.28 ms / 7.1 MB | 13.2 ms / 41 MB | n/a | n/a |
| users-broad-100-identity | 5.56 ms / 6.5 MB | 5.51 ms / 6.4 MB | 7.76 ms / 3.7 MB | 5.11 ms / 4.7 MB | 7.07 ms / 7.2 MB | 14.0 ms / 42 MB | n/a | n/a |
| users-broad-100-ids | 5.42 ms / 4.8 MB | 4.90 ms / 4.8 MB | 5.84 ms / 3.6 MB | 4.67 ms / 4.7 MB | 5.43 ms / 7.1 MB | 11.1 ms / 37 MB | n/a | n/a |
| users-broad-100-keys-len | 5.08 ms / 4.9 MB | 4.81 ms / 4.9 MB | 5.50 ms / 3.6 MB | 4.91 ms / 4.8 MB | 5.71 ms / 7.1 MB | 11.5 ms / 37 MB | n/a | n/a |
| users-broad-100-keys-publish | 5.14 ms / 4.8 MB | 5.12 ms / 4.8 MB | 5.48 ms / 3.6 MB | 4.85 ms / 4.8 MB | 5.62 ms / 7.1 MB | disagreed | n/a | n/a |
| users-broad-100-max-score | 5.07 ms / 5.1 MB | 4.99 ms / 5.1 MB | 6.20 ms / 3.6 MB | 4.96 ms / 4.9 MB | 5.29 ms / 7.1 MB | 11.3 ms / 37 MB | n/a | n/a |
| users-broad-100-nested-dept | 4.66 ms / 4.7 MB | 4.49 ms / 4.7 MB | 5.58 ms / 3.6 MB | 4.66 ms / 4.7 MB | 5.18 ms / 7.1 MB | 10.4 ms / 37 MB | n/a | n/a |
| users-broad-100-project-names | 4.72 ms / 4.8 MB | 5.23 ms / 4.8 MB | 6.10 ms / 3.6 MB | 4.97 ms / 4.7 MB | 5.10 ms / 7.2 MB | 10.6 ms / 37 MB | n/a | n/a |
| users-broad-100-project-pair | 5.84 ms / 4.9 MB | 5.58 ms / 4.9 MB | 5.50 ms / 3.6 MB | 4.69 ms / 4.7 MB | 5.60 ms / 7.1 MB | 12.0 ms / 28 MB | n/a | n/a |
| users-broad-100-reduce-score | 5.07 ms / 5.0 MB | 4.94 ms / 5.0 MB | 5.76 ms / 3.6 MB | 4.87 ms / 4.8 MB | 5.18 ms / 7.2 MB | n/a | n/a | n/a |
| users-broad-100-reverse-id | 5.25 ms / 6.3 MB | 5.06 ms / 6.3 MB | 5.44 ms / 3.6 MB | 5.44 ms / 4.8 MB | 5.48 ms / 7.0 MB | 12.5 ms / 41 MB | n/a | n/a |
| users-broad-100-select-id-stream | 5.03 ms / 4.8 MB | 4.86 ms / 4.8 MB | 5.89 ms / 3.6 MB | 5.01 ms / 4.7 MB | 5.22 ms / 7.1 MB | n/a | n/a | n/a |
| users-broad-100-slice-length | 4.82 ms / 4.7 MB | 4.38 ms / 4.7 MB | 5.39 ms / 3.6 MB | 4.84 ms / 4.8 MB | 5.48 ms / 7.1 MB | 15.7 ms / 37 MB | n/a | n/a |
| users-broad-100-sort-last | 5.81 ms / 6.4 MB | 5.18 ms / 6.3 MB | 5.45 ms / 3.7 MB | 4.68 ms / 5.0 MB | 5.42 ms / 7.0 MB | 11.0 ms / 41 MB | n/a | n/a |
| users-broad-100-sum-score | 4.97 ms / 5.0 MB | 4.97 ms / 5.0 MB | 5.58 ms / 3.6 MB | 4.68 ms / 4.7 MB | 5.25 ms / 7.2 MB | n/a | n/a | n/a |
| users-broad-100-type-path | 7.70 ms / 4.7 MB | 4.87 ms / 4.7 MB | 5.56 ms / 3.6 MB | 4.76 ms / 4.7 MB | 5.58 ms / 7.0 MB | disagreed | n/a | n/a |
| users-broad-100-unique-scores | 4.86 ms / 6.1 MB | 5.26 ms / 6.1 MB | 5.50 ms / 3.6 MB | 4.66 ms / 4.9 MB | 5.56 ms / 7.1 MB | 12.0 ms / 37 MB | n/a | n/a |
| users-broad-1k-all-nonneg | 6.75 ms / 5.8 MB | 6.71 ms / 5.8 MB | 15.5 ms / 12 MB | 9.64 ms / 13 MB | 13.6 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-any-high | 6.82 ms / 5.8 MB | 6.50 ms / 5.8 MB | 14.6 ms / 12 MB | 9.30 ms / 13 MB | 12.9 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-count | 6.03 ms / 5.6 MB | 5.95 ms / 5.6 MB | 14.2 ms / 12 MB | 9.02 ms / 13 MB | 13.4 ms / 15 MB | 28.2 ms / 72 MB | n/a | n/a |
| users-broad-1k-descent | 10.3 ms / 13 MB | 9.94 ms / 13 MB | 23.0 ms / 16 MB | 12.7 ms / 15 MB | 30.8 ms / 24 MB | 71.8 ms / 220 MB | n/a | n/a |
| users-broad-1k-filter-active | 6.64 ms / 5.8 MB | 6.18 ms / 5.8 MB | 14.3 ms / 12 MB | 10.2 ms / 13 MB | 13.2 ms / 15 MB | 34.7 ms / 100 MB | n/a | n/a |
| users-broad-1k-first-id | 5.94 ms / 5.6 MB | 5.86 ms / 5.7 MB | 13.9 ms / 12 MB | 8.86 ms / 13 MB | 13.1 ms / 15 MB | 28.0 ms / 70 MB | n/a | n/a |
| users-broad-1k-group-mod | 11.9 ms / 16 MB | 11.7 ms / 16 MB | 15.8 ms / 12 MB | 9.56 ms / 13 MB | 13.7 ms / 15 MB | 44.9 ms / 129 MB | n/a | n/a |
| users-broad-1k-high-score | 6.26 ms / 5.8 MB | 6.14 ms / 5.8 MB | 15.6 ms / 12 MB | 9.21 ms / 13 MB | 13.2 ms / 15 MB | 37.4 ms / 110 MB | n/a | n/a |
| users-broad-1k-identity | 14.5 ms / 18 MB | 15.0 ms / 18 MB | 34.4 ms / 13 MB | 12.9 ms / 13 MB | disagreed | 59.6 ms / 103 MB | n/a | n/a |
| users-broad-1k-ids | 6.66 ms / 5.9 MB | 6.43 ms / 5.8 MB | 14.9 ms / 12 MB | 8.92 ms / 13 MB | 13.2 ms / 15 MB | 29.6 ms / 72 MB | n/a | n/a |
| users-broad-1k-keys-len | 6.36 ms / 5.9 MB | 6.27 ms / 5.9 MB | 14.2 ms / 12 MB | 9.47 ms / 13 MB | 13.2 ms / 15 MB | 28.3 ms / 70 MB | n/a | n/a |
| users-broad-1k-keys-publish | 6.74 ms / 5.7 MB | 6.40 ms / 5.7 MB | 14.2 ms / 12 MB | 8.94 ms / 13 MB | 12.8 ms / 15 MB | disagreed | n/a | n/a |
| users-broad-1k-max-score | 7.37 ms / 6.4 MB | 7.54 ms / 6.4 MB | 15.3 ms / 12 MB | 9.24 ms / 13 MB | 13.1 ms / 15 MB | 29.6 ms / 72 MB | n/a | n/a |
| users-broad-1k-nested-dept | 6.48 ms / 5.6 MB | 6.13 ms / 5.6 MB | 14.3 ms / 12 MB | 9.30 ms / 13 MB | 13.1 ms / 15 MB | 28.2 ms / 70 MB | n/a | n/a |
| users-broad-1k-project-names | 6.51 ms / 5.9 MB | 6.85 ms / 5.9 MB | 14.9 ms / 12 MB | 9.79 ms / 13 MB | 13.2 ms / 15 MB | 29.7 ms / 72 MB | n/a | n/a |
| users-broad-1k-project-pair | 6.88 ms / 6.1 MB | 6.86 ms / 6.1 MB | 16.1 ms / 13 MB | 10.1 ms / 13 MB | 13.1 ms / 15 MB | 42.3 ms / 109 MB | n/a | n/a |
| users-broad-1k-reduce-score | 5.92 ms / 6.3 MB | 6.03 ms / 6.3 MB | 14.4 ms / 12 MB | 9.38 ms / 13 MB | 13.3 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-reverse-id | 11.3 ms / 16 MB | 11.7 ms / 16 MB | 15.5 ms / 12 MB | 9.34 ms / 13 MB | 12.8 ms / 15 MB | 36.8 ms / 101 MB | n/a | n/a |
| users-broad-1k-select-id-stream | 7.58 ms / 5.9 MB | 7.94 ms / 5.9 MB | 15.7 ms / 12 MB | 10.1 ms / 13 MB | 14.0 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-slice-length | 6.63 ms / 5.7 MB | 7.45 ms / 5.7 MB | 14.8 ms / 12 MB | 8.88 ms / 13 MB | 12.7 ms / 15 MB | 28.7 ms / 74 MB | n/a | n/a |
| users-broad-1k-sort-last | 11.7 ms / 16 MB | 11.1 ms / 15 MB | 21.2 ms / 12 MB | 9.32 ms / 13 MB | 13.7 ms / 15 MB | 37.3 ms / 112 MB | n/a | n/a |
| users-broad-1k-sum-score | 5.94 ms / 6.3 MB | 6.20 ms / 6.3 MB | 14.7 ms / 12 MB | 9.24 ms / 13 MB | 13.3 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-type-path | 6.39 ms / 5.6 MB | 6.29 ms / 5.6 MB | 14.3 ms / 12 MB | 9.11 ms / 13 MB | 13.1 ms / 15 MB | disagreed | n/a | n/a |
| users-broad-1k-unique-scores | 9.76 ms / 14 MB | 9.82 ms / 14 MB | 17.0 ms / 12 MB | 9.07 ms / 13 MB | 13.2 ms / 15 MB | 29.6 ms / 73 MB | n/a | n/a |
| users-broad-5k-all-nonneg | 15.2 ms / 10 MB | 14.0 ms / 10 MB | 54.3 ms / 50 MB | 29.5 ms / 51 MB | 43.7 ms / 42 MB | n/a | n/a | n/a |
| users-broad-5k-any-high | 13.9 ms / 10 MB | 13.8 ms / 10 MB | 51.8 ms / 50 MB | 25.7 ms / 51 MB | 43.1 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-count | 14.7 ms / 10.0 MB | 13.3 ms / 9.9 MB | 52.9 ms / 50 MB | 25.7 ms / 51 MB | 42.6 ms / 41 MB | 98.4 ms / 218 MB | n/a | n/a |
| users-broad-5k-descent | 30.2 ms / 43 MB | 30.0 ms / 43 MB | 90.2 ms / 74 MB | 41.0 ms / 63 MB | 122 ms / 78 MB | 327 ms / 914 MB | n/a | n/a |
| users-broad-5k-filter-active | 15.6 ms / 10 MB | 13.9 ms / 10 MB | 54.4 ms / 50 MB | 29.5 ms / 51 MB | 44.4 ms / 42 MB | 133 ms / 334 MB | n/a | n/a |
| users-broad-5k-first-id | 14.0 ms / 9.9 MB | 12.8 ms / 10.0 MB | 51.6 ms / 50 MB | 25.7 ms / 50 MB | 42.2 ms / 41 MB | 96.4 ms / 217 MB | n/a | n/a |
| users-broad-5k-group-mod | 40.3 ms / 64 MB | 40.0 ms / 64 MB | 57.6 ms / 50 MB | 32.0 ms / 52 MB | 45.1 ms / 42 MB | 185 ms / 488 MB | n/a | n/a |
| users-broad-5k-high-score | 14.7 ms / 10 MB | 14.0 ms / 10 MB | 55.7 ms / 50 MB | 28.4 ms / 51 MB | 44.6 ms / 41 MB | 139 ms / 377 MB | n/a | n/a |
| users-broad-5k-identity | 49.3 ms / 66 MB | 48.3 ms / 66 MB | 152 ms / 55 MB | 45.6 ms / 50 MB | disagreed | 248 ms / 374 MB | n/a | n/a |
| users-broad-5k-ids | 16.8 ms / 11 MB | 15.7 ms / 11 MB | 52.9 ms / 50 MB | 26.2 ms / 51 MB | 43.6 ms / 41 MB | 106 ms / 228 MB | n/a | n/a |
| users-broad-5k-keys-len | 14.0 ms / 10 MB | 13.1 ms / 10 MB | 51.5 ms / 50 MB | 25.9 ms / 51 MB | 42.5 ms / 42 MB | 97.2 ms / 218 MB | n/a | n/a |
| users-broad-5k-keys-publish | 14.5 ms / 10 MB | 13.3 ms / 10 MB | 51.8 ms / 50 MB | 25.8 ms / 51 MB | 43.2 ms / 41 MB | disagreed | n/a | n/a |
| users-broad-5k-max-score | 19.8 ms / 12 MB | 18.6 ms / 13 MB | 54.1 ms / 50 MB | 26.9 ms / 51 MB | 44.1 ms / 42 MB | 104 ms / 233 MB | n/a | n/a |
| users-broad-5k-nested-dept | 13.9 ms / 10.0 MB | 13.4 ms / 10.0 MB | 51.6 ms / 50 MB | 25.9 ms / 50 MB | 42.8 ms / 41 MB | 95.3 ms / 222 MB | n/a | n/a |
| users-broad-5k-project-names | 16.1 ms / 11 MB | 15.1 ms / 11 MB | 54.7 ms / 50 MB | 27.3 ms / 51 MB | 45.2 ms / 42 MB | 107 ms / 229 MB | n/a | n/a |
| users-broad-5k-project-pair | 16.9 ms / 12 MB | 16.5 ms / 12 MB | 59.1 ms / 52 MB | 30.0 ms / 51 MB | 44.7 ms / 43 MB | 164 ms / 338 MB | n/a | n/a |
| users-broad-5k-reduce-score | 13.0 ms / 12 MB | 13.0 ms / 12 MB | 53.8 ms / 50 MB | 27.6 ms / 51 MB | 45.2 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-reverse-id | 37.4 ms / 63 MB | 35.3 ms / 63 MB | 52.9 ms / 50 MB | 26.6 ms / 51 MB | 42.3 ms / 42 MB | 136 ms / 393 MB | n/a | n/a |
| users-broad-5k-select-id-stream | 19.5 ms / 10 MB | 17.9 ms / 10 MB | 53.8 ms / 50 MB | 30.3 ms / 51 MB | 47.0 ms / 42 MB | n/a | n/a | n/a |
| users-broad-5k-slice-length | 14.2 ms / 10.0 MB | 13.0 ms / 10.0 MB | 52.0 ms / 50 MB | 25.5 ms / 51 MB | 41.6 ms / 41 MB | 102 ms / 242 MB | n/a | n/a |
| users-broad-5k-sort-last | 37.1 ms / 63 MB | 37.0 ms / 63 MB | 59.1 ms / 50 MB | 30.1 ms / 51 MB | 47.2 ms / 42 MB | 148 ms / 379 MB | n/a | n/a |
| users-broad-5k-sum-score | 13.4 ms / 12 MB | 12.5 ms / 12 MB | 54.3 ms / 50 MB | 26.7 ms / 51 MB | 43.3 ms / 42 MB | n/a | n/a | n/a |
| users-broad-5k-type-path | 13.6 ms / 9.9 MB | 13.2 ms / 10.0 MB | 51.9 ms / 50 MB | 26.1 ms / 51 MB | 43.0 ms / 41 MB | disagreed | n/a | n/a |
| users-broad-5k-unique-scores | 30.5 ms / 46 MB | 29.5 ms / 46 MB | 54.8 ms / 50 MB | 27.1 ms / 51 MB | 46.4 ms / 42 MB | 106 ms / 229 MB | n/a | n/a |
| users-broad-25k-all-nonneg | 45.7 ms / 32 MB | 45.4 ms / 32 MB | 239 ms / 237 MB | 121 ms / 239 MB | 185 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-any-high | 46.4 ms / 32 MB | 46.3 ms / 32 MB | 229 ms / 237 MB | 104 ms / 239 MB | 176 ms / 180 MB | n/a | n/a | n/a |
| users-broad-25k-count | 43.6 ms / 31 MB | 43.2 ms / 31 MB | 228 ms / 237 MB | 102 ms / 239 MB | 180 ms / 179 MB | 437 ms / 946 MB | n/a | n/a |
| users-broad-25k-descent | 121 ms / 191 MB | 123 ms / 189 MB | 422 ms / 371 MB | 177 ms / 299 MB | 577 ms / 387 MB | excluded | n/a | n/a |
| users-broad-25k-filter-active | 45.3 ms / 32 MB | 45.0 ms / 32 MB | 244 ms / 237 MB | 120 ms / 239 MB | 182 ms / 182 MB | excluded | n/a | n/a |
| users-broad-25k-first-id | 43.0 ms / 32 MB | 42.9 ms / 32 MB | 227 ms / 237 MB | 102 ms / 238 MB | 178 ms / 179 MB | 447 ms / 941 MB | n/a | n/a |
| users-broad-25k-group-mod | 177 ms / 268 MB | 176 ms / 237 MB | 262 ms / 239 MB | 133 ms / 243 MB | 193 ms / 187 MB | 838 ms / 2193 MB | n/a | n/a |
| users-broad-25k-high-score | 45.6 ms / 32 MB | 45.3 ms / 32 MB | 242 ms / 237 MB | 119 ms / 239 MB | 189 ms / 183 MB | excluded | n/a | n/a |
| users-broad-25k-identity | 212 ms / 284 MB | 210 ms / 257 MB | 672 ms / 266 MB | 196 ms / 238 MB | disagreed | excluded | n/a | n/a |
| users-broad-25k-ids | 48.1 ms / 34 MB | 48.0 ms / 34 MB | 235 ms / 237 MB | 105 ms / 239 MB | 182 ms / 183 MB | 474 ms / 990 MB | n/a | n/a |
| users-broad-25k-keys-len | 43.5 ms / 32 MB | 43.2 ms / 32 MB | 229 ms / 237 MB | 106 ms / 239 MB | 177 ms / 179 MB | 437 ms / 942 MB | n/a | n/a |
| users-broad-25k-keys-publish | 44.0 ms / 32 MB | 44.1 ms / 32 MB | 227 ms / 237 MB | 104 ms / 239 MB | 175 ms / 179 MB | disagreed | n/a | n/a |
| users-broad-25k-max-score | 68.5 ms / 41 MB | 68.3 ms / 41 MB | 235 ms / 237 MB | 108 ms / 239 MB | 183 ms / 183 MB | 469 ms / 1011 MB | n/a | n/a |
| users-broad-25k-nested-dept | 43.2 ms / 32 MB | 43.2 ms / 32 MB | 228 ms / 237 MB | 104 ms / 238 MB | 180 ms / 179 MB | 444 ms / 948 MB | n/a | n/a |
| users-broad-25k-project-names | 48.4 ms / 35 MB | 48.6 ms / 35 MB | 240 ms / 237 MB | 109 ms / 239 MB | 184 ms / 183 MB | 479 ms / 997 MB | n/a | n/a |
| users-broad-25k-project-pair | 58.0 ms / 39 MB | 57.9 ms / 39 MB | 268 ms / 248 MB | 123 ms / 239 MB | 189 ms / 196 MB | 758 ms / 1376 MB | n/a | n/a |
| users-broad-25k-reduce-score | 38.1 ms / 37 MB | 37.5 ms / 38 MB | 235 ms / 237 MB | 109 ms / 239 MB | 185 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-reverse-id | 147 ms / 264 MB | 147 ms / 250 MB | 232 ms / 238 MB | 107 ms / 239 MB | 176 ms / 179 MB | 620 ms / 1831 MB | n/a | n/a |
| users-broad-25k-select-id-stream | 66.6 ms / 34 MB | 66.5 ms / 34 MB | 240 ms / 238 MB | 125 ms / 239 MB | 205 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-slice-length | 44.2 ms / 32 MB | 44.0 ms / 32 MB | 230 ms / 237 MB | 103 ms / 239 MB | 177 ms / 179 MB | 456 ms / 1042 MB | n/a | n/a |
| users-broad-25k-sort-last | 167 ms / 265 MB | 165 ms / 238 MB | 266 ms / 239 MB | 133 ms / 241 MB | 206 ms / 190 MB | 714 ms / 1792 MB | n/a | n/a |
| users-broad-25k-sum-score | 38.0 ms / 38 MB | 37.5 ms / 38 MB | 237 ms / 237 MB | 106 ms / 239 MB | 183 ms / 183 MB | n/a | n/a | n/a |
| users-broad-25k-type-path | 43.2 ms / 32 MB | 43.2 ms / 32 MB | 224 ms / 237 MB | 101 ms / 238 MB | 174 ms / 179 MB | disagreed | n/a | n/a |
| users-broad-25k-unique-scores | 122 ms / 192 MB | 121 ms / 191 MB | 245 ms / 238 MB | 106 ms / 239 MB | 198 ms / 188 MB | 474 ms / 987 MB | n/a | n/a |
| users-broad-50k-all-nonneg | 83.6 ms / 59 MB | 83.7 ms / 59 MB | 478 ms / 472 MB | 236 ms / 475 MB | 367 ms / 354 MB | n/a | n/a | n/a |
| users-broad-50k-any-high | 84.2 ms / 59 MB | 83.7 ms / 59 MB | 443 ms / 472 MB | 200 ms / 474 MB | 343 ms / 351 MB | n/a | n/a | n/a |
| users-broad-50k-count | 78.9 ms / 58 MB | 79.4 ms / 58 MB | 445 ms / 472 MB | 199 ms / 473 MB | 347 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-descent | 233 ms / 366 MB | 235 ms / 369 MB | 848 ms / 764 MB | 349 ms / 591 MB | 1140 ms / 723 MB | excluded | n/a | n/a |
| users-broad-50k-filter-active | 84.1 ms / 59 MB | 83.8 ms / 59 MB | 479 ms / 473 MB | 233 ms / 475 MB | 361 ms / 356 MB | excluded | n/a | n/a |
| users-broad-50k-first-id | 79.0 ms / 58 MB | 79.1 ms / 58 MB | 453 ms / 472 MB | 197 ms / 473 MB | 343 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-group-mod | 344 ms / 492 MB | 343 ms / 492 MB | 520 ms / 476 MB | 258 ms / 481 MB | 370 ms / 366 MB | 1718 ms / 4106 MB | n/a | n/a |
| users-broad-50k-high-score | 83.9 ms / 59 MB | 84.6 ms / 59 MB | 476 ms / 473 MB | 230 ms / 475 MB | 359 ms / 358 MB | excluded | n/a | n/a |
| users-broad-50k-identity | 412 ms / 499 MB | 411 ms / 499 MB | 1322 ms / 529 MB | 398 ms / 473 MB | disagreed | excluded | n/a | n/a |
| users-broad-50k-ids | 88.0 ms / 64 MB | 88.0 ms / 64 MB | 460 ms / 473 MB | 202 ms / 474 MB | 349 ms / 358 MB | 947 ms / 1924 MB | n/a | n/a |
| users-broad-50k-keys-len | 79.1 ms / 59 MB | 79.5 ms / 59 MB | 451 ms / 472 MB | 198 ms / 474 MB | 339 ms / 350 MB | 871 ms / 1829 MB | n/a | n/a |
| users-broad-50k-keys-publish | 79.4 ms / 59 MB | 79.9 ms / 59 MB | 445 ms / 472 MB | 199 ms / 474 MB | 346 ms / 349 MB | disagreed | n/a | n/a |
| users-broad-50k-max-score | 133 ms / 78 MB | 130 ms / 78 MB | 464 ms / 473 MB | 210 ms / 474 MB | 359 ms / 359 MB | 948 ms / 1988 MB | n/a | n/a |
| users-broad-50k-nested-dept | 80.1 ms / 58 MB | 80.2 ms / 58 MB | 453 ms / 472 MB | 198 ms / 473 MB | 344 ms / 349 MB | 870 ms / 1831 MB | n/a | n/a |
| users-broad-50k-project-names | 89.7 ms / 65 MB | 89.6 ms / 65 MB | 474 ms / 473 MB | 209 ms / 474 MB | 358 ms / 358 MB | 935 ms / 1933 MB | n/a | n/a |
| users-broad-50k-project-pair | 108 ms / 72 MB | 107 ms / 72 MB | 508 ms / 494 MB | 237 ms / 474 MB | 371 ms / 384 MB | 1533 ms / 2720 MB | n/a | n/a |
| users-broad-50k-reduce-score | 68.2 ms / 70 MB | 68.2 ms / 70 MB | 472 ms / 472 MB | 210 ms / 474 MB | 356 ms / 354 MB | n/a | n/a | n/a |
| users-broad-50k-reverse-id | 288 ms / 486 MB | 288 ms / 486 MB | 458 ms / 473 MB | 210 ms / 474 MB | 348 ms / 350 MB | 1255 ms / 3462 MB | n/a | n/a |
| users-broad-50k-select-id-stream | 122 ms / 63 MB | 122 ms / 63 MB | 474 ms / 473 MB | 243 ms / 473 MB | 384 ms / 354 MB | n/a | n/a | n/a |
| users-broad-50k-slice-length | 78.7 ms / 59 MB | 78.9 ms / 59 MB | 448 ms / 472 MB | 200 ms / 474 MB | 340 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-sort-last | 332 ms / 487 MB | 329 ms / 487 MB | 548 ms / 477 MB | 259 ms / 478 MB | 411 ms / 372 MB | 1404 ms / 3487 MB | n/a | n/a |
| users-broad-50k-sum-score | 70.1 ms / 70 MB | 71.2 ms / 70 MB | 471 ms / 473 MB | 209 ms / 474 MB | 357 ms / 358 MB | n/a | n/a | n/a |
| users-broad-50k-type-path | 79.2 ms / 58 MB | 79.0 ms / 58 MB | 450 ms / 472 MB | 196 ms / 473 MB | 337 ms / 349 MB | disagreed | n/a | n/a |
| users-broad-50k-unique-scores | 235 ms / 379 MB | 234 ms / 379 MB | 486 ms / 473 MB | 205 ms / 474 MB | 398 ms / 369 MB | 950 ms / 1928 MB | n/a | n/a |
| users-broad-100k-all-nonneg | 159 ms / 113 MB | 158 ms / 113 MB | 935 ms / 943 MB | 466 ms / 945 MB | 699 ms / 697 MB | n/a | n/a | n/a |
| users-broad-100k-any-high | 160 ms / 113 MB | 159 ms / 113 MB | 903 ms / 943 MB | 392 ms / 945 MB | 673 ms / 693 MB | n/a | n/a | n/a |
| users-broad-100k-count | 150 ms / 112 MB | 150 ms / 112 MB | 888 ms / 943 MB | 393 ms / 943 MB | 677 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-descent | 459 ms / 699 MB | 456 ms / 699 MB | 1706 ms / 1583 MB | 693 ms / 1180 MB | 2296 ms / 1485 MB | excluded | n/a | n/a |
| users-broad-100k-filter-active | 160 ms / 113 MB | 158 ms / 113 MB | 950 ms / 943 MB | 461 ms / 946 MB | 694 ms / 704 MB | excluded | n/a | n/a |
| users-broad-100k-first-id | 150 ms / 112 MB | 150 ms / 112 MB | 883 ms / 943 MB | 385 ms / 943 MB | 667 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-group-mod | 677 ms / 913 MB | 676 ms / 913 MB | 1034 ms / 951 MB | 510 ms / 959 MB | 750 ms / 722 MB | 3452 ms / 8712 MB | n/a | n/a |
| users-broad-100k-high-score | 159 ms / 113 MB | 159 ms / 113 MB | 936 ms / 943 MB | 453 ms / 946 MB | 717 ms / 707 MB | excluded | n/a | n/a |
| users-broad-100k-identity | 814 ms / 977 MB | 815 ms / 977 MB | 2596 ms / 1057 MB | 754 ms / 943 MB | disagreed | excluded | n/a | n/a |
| users-broad-100k-ids | 168 ms / 123 MB | 169 ms / 123 MB | 942 ms / 944 MB | 394 ms / 943 MB | 690 ms / 706 MB | 1898 ms / 3852 MB | n/a | n/a |
| users-broad-100k-keys-len | 151 ms / 113 MB | 159 ms / 113 MB | 890 ms / 943 MB | 392 ms / 943 MB | 673 ms / 689 MB | 1728 ms / 3653 MB | n/a | n/a |
| users-broad-100k-keys-publish | 152 ms / 112 MB | 151 ms / 113 MB | 896 ms / 943 MB | 390 ms / 943 MB | 677 ms / 689 MB | disagreed | n/a | n/a |
| users-broad-100k-max-score | 250 ms / 150 MB | 250 ms / 150 MB | 936 ms / 944 MB | 409 ms / 944 MB | 703 ms / 708 MB | 1895 ms / 3914 MB | n/a | n/a |
| users-broad-100k-nested-dept | 151 ms / 112 MB | 150 ms / 112 MB | 882 ms / 943 MB | 389 ms / 943 MB | 675 ms / 689 MB | 1704 ms / 3642 MB | n/a | n/a |
| users-broad-100k-project-names | 171 ms / 123 MB | 170 ms / 123 MB | 949 ms / 944 MB | 413 ms / 943 MB | 694 ms / 706 MB | 1902 ms / 3938 MB | n/a | n/a |
| users-broad-100k-project-pair | 210 ms / 140 MB | 207 ms / 140 MB | 1015 ms / 988 MB | 465 ms / 944 MB | 735 ms / 758 MB | 3050 ms / 5371 MB | n/a | n/a |
| users-broad-100k-reduce-score | 129 ms / 136 MB | 130 ms / 136 MB | 952 ms / 943 MB | 418 ms / 945 MB | 721 ms / 698 MB | n/a | n/a | n/a |
| users-broad-100k-reverse-id | 566 ms / 906 MB | 566 ms / 906 MB | 925 ms / 944 MB | 410 ms / 943 MB | 673 ms / 690 MB | 2428 ms / 6963 MB | n/a | n/a |
| users-broad-100k-select-id-stream | 236 ms / 121 MB | 236 ms / 121 MB | 937 ms / 945 MB | 480 ms / 943 MB | 761 ms / 697 MB | n/a | n/a | n/a |
| users-broad-100k-slice-length | 149 ms / 112 MB | 150 ms / 112 MB | 886 ms / 943 MB | 389 ms / 944 MB | 665 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-sort-last | 645 ms / 915 MB | 641 ms / 915 MB | 1102 ms / 951 MB | 512 ms / 953 MB | 829 ms / 734 MB | 2810 ms / 7036 MB | n/a | n/a |
| users-broad-100k-sum-score | 130 ms / 136 MB | 131 ms / 136 MB | 942 ms / 944 MB | 401 ms / 944 MB | 684 ms / 707 MB | n/a | n/a | n/a |
| users-broad-100k-type-path | 150 ms / 112 MB | 150 ms / 112 MB | 881 ms / 943 MB | 390 ms / 943 MB | 686 ms / 689 MB | disagreed | n/a | n/a |
| users-broad-100k-unique-scores | 459 ms / 724 MB | 459 ms / 724 MB | 981 ms / 946 MB | 404 ms / 944 MB | 808 ms / 729 MB | 1868 ms / 3844 MB | n/a | n/a |
| users-broad-200k-all-nonneg | 308 ms / 221 MB | 307 ms / 221 MB | 1840 ms / 1882 MB | 919 ms / 1887 MB | 1396 ms / 1385 MB | n/a | n/a | n/a |
| users-broad-200k-any-high | 311 ms / 221 MB | 308 ms / 221 MB | 1779 ms / 1882 MB | 775 ms / 1887 MB | 1344 ms / 1376 MB | n/a | n/a | n/a |
| users-broad-200k-count | 289 ms / 221 MB | 290 ms / 221 MB | 1783 ms / 1882 MB | 781 ms / 1884 MB | 1342 ms / 1368 MB | excluded | n/a | n/a |
| users-broad-200k-descent | 902 ms / 1379 MB | 908 ms / 1379 MB | 3368 ms / 2923 MB | 1379 ms / 2357 MB | 4537 ms / 2861 MB | excluded | n/a | n/a |
| users-broad-200k-filter-active | 304 ms / 221 MB | 303 ms / 221 MB | 1922 ms / 1882 MB | 903 ms / 1889 MB | 1388 ms / 1398 MB | excluded | n/a | n/a |
| users-broad-200k-first-id | 291 ms / 221 MB | 291 ms / 221 MB | 1761 ms / 1882 MB | 768 ms / 1884 MB | 1334 ms / 1369 MB | excluded | n/a | n/a |
| users-broad-200k-group-mod | 1344 ms / 1803 MB | 1347 ms / 1803 MB | 2083 ms / 1900 MB | 1019 ms / 1913 MB | 1444 ms / 1440 MB | 6778 ms / 17290 MB | n/a | n/a |
| users-broad-200k-high-score | 306 ms / 221 MB | 305 ms / 221 MB | 1874 ms / 1884 MB | 903 ms / 1889 MB | 1441 ms / 1403 MB | excluded | n/a | n/a |
| users-broad-200k-identity | 1637 ms / 1934 MB | 1631 ms / 1934 MB | 5229 ms / 2110 MB | 1501 ms / 1883 MB | disagreed | excluded | n/a | n/a |
| users-broad-200k-ids | 333 ms / 241 MB | 330 ms / 241 MB | 1863 ms / 1885 MB | 786 ms / 1884 MB | 1374 ms / 1402 MB | 3734 ms / 7670 MB | n/a | n/a |
| users-broad-200k-keys-len | 295 ms / 221 MB | 292 ms / 221 MB | 1763 ms / 1882 MB | 774 ms / 1884 MB | 1322 ms / 1368 MB | 3408 ms / 7311 MB | n/a | n/a |
| users-broad-200k-keys-publish | 294 ms / 221 MB | 297 ms / 221 MB | 1789 ms / 1882 MB | 777 ms / 1884 MB | 1328 ms / 1369 MB | disagreed | n/a | n/a |
| users-broad-200k-max-score | 497 ms / 286 MB | 497 ms / 286 MB | 1831 ms / 1885 MB | 818 ms / 1884 MB | 1384 ms / 1406 MB | 3741 ms / 7806 MB | n/a | n/a |
| users-broad-200k-nested-dept | 291 ms / 221 MB | 292 ms / 221 MB | 1751 ms / 1882 MB | 769 ms / 1884 MB | 1327 ms / 1369 MB | 3415 ms / 7287 MB | n/a | n/a |
| users-broad-200k-project-names | 336 ms / 241 MB | 338 ms / 241 MB | 1889 ms / 1885 MB | 809 ms / 1884 MB | 1401 ms / 1402 MB | 3741 ms / 7728 MB | n/a | n/a |
| users-broad-200k-project-pair | 404 ms / 280 MB | 406 ms / 280 MB | 2050 ms / 1972 MB | 925 ms / 1887 MB | 1425 ms / 1506 MB | 6065 ms / 11217 MB | n/a | n/a |
| users-broad-200k-reduce-score | 251 ms / 263 MB | 250 ms / 263 MB | 1845 ms / 1882 MB | 820 ms / 1887 MB | 1394 ms / 1387 MB | n/a | n/a | n/a |
| users-broad-200k-reverse-id | 1129 ms / 1803 MB | 1126 ms / 1803 MB | 1813 ms / 1885 MB | 811 ms / 1884 MB | 1310 ms / 1371 MB | 4856 ms / 13904 MB | n/a | n/a |
| users-broad-200k-select-id-stream | 463 ms / 244 MB | 466 ms / 244 MB | 1874 ms / 1886 MB | 948 ms / 1884 MB | 1520 ms / 1385 MB | n/a | n/a | n/a |
| users-broad-200k-slice-length | 293 ms / 221 MB | 293 ms / 221 MB | 1773 ms / 1882 MB | 773 ms / 1884 MB | 1333 ms / 1369 MB | excluded | n/a | n/a |
| users-broad-200k-sort-last | 1287 ms / 1828 MB | 1282 ms / 1828 MB | 2246 ms / 1899 MB | 1025 ms / 1902 MB | 1763 ms / 1459 MB | 5702 ms / 14107 MB | n/a | n/a |
| users-broad-200k-sum-score | 253 ms / 263 MB | 254 ms / 263 MB | 1884 ms / 1885 MB | 798 ms / 1884 MB | 1384 ms / 1405 MB | n/a | n/a | n/a |
| users-broad-200k-type-path | 290 ms / 221 MB | 290 ms / 221 MB | 1776 ms / 1882 MB | 769 ms / 1884 MB | 1343 ms / 1368 MB | disagreed | n/a | n/a |
| users-broad-200k-unique-scores | 915 ms / 1446 MB | 911 ms / 1446 MB | 1987 ms / 1885 MB | 796 ms / 1884 MB | 1656 ms / 1450 MB | 3693 ms / 7649 MB | n/a | n/a |
| users-narrow-100-all-nonneg | 3.26 ms / 4.8 MB | 3.44 ms / 4.8 MB | 3.66 ms / 2.7 MB | 3.05 ms / 4.2 MB | 3.44 ms / 6.3 MB | n/a | n/a | n/a |
| users-narrow-100-any-high | 3.10 ms / 4.8 MB | 3.94 ms / 4.8 MB | 3.42 ms / 2.6 MB | 3.07 ms / 4.2 MB | 3.62 ms / 6.3 MB | n/a | n/a | n/a |
| users-narrow-100-count | 3.01 ms / 4.5 MB | 3.07 ms / 4.5 MB | 3.21 ms / 2.7 MB | 2.76 ms / 4.0 MB | 3.24 ms / 6.1 MB | 6.34 ms / 24 MB | n/a | n/a |
| users-narrow-100-descent | 3.44 ms / 4.7 MB | 3.55 ms / 4.8 MB | 3.36 ms / 2.7 MB | 3.05 ms / 4.0 MB | 3.39 ms / 6.4 MB | 7.31 ms / 30 MB | n/a | n/a |
| users-narrow-100-filter-active | 3.19 ms / 4.7 MB | 3.66 ms / 4.7 MB | 5.31 ms / 2.7 MB | 3.99 ms / 4.0 MB | 3.44 ms / 6.3 MB | 6.96 ms / 27 MB | n/a | n/a |
| users-narrow-100-first-id | 2.99 ms / 4.5 MB | 2.94 ms / 4.5 MB | 2.87 ms / 2.6 MB | 2.95 ms / 3.9 MB | 3.11 ms / 6.0 MB | 6.54 ms / 27 MB | n/a | n/a |
| users-narrow-100-group-mod | 3.42 ms / 5.0 MB | 3.80 ms / 5.0 MB | 3.46 ms / 2.7 MB | 2.98 ms / 4.2 MB | 3.55 ms / 6.3 MB | 7.23 ms / 34 MB | n/a | n/a |
| users-narrow-100-high-score | 3.42 ms / 4.7 MB | 3.31 ms / 4.7 MB | 3.36 ms / 2.7 MB | 2.92 ms / 4.0 MB | 3.29 ms / 6.3 MB | 7.69 ms / 33 MB | n/a | n/a |
| users-narrow-100-identity | 3.61 ms / 4.4 MB | 3.42 ms / 4.4 MB | 3.69 ms / 2.7 MB | 3.16 ms / 3.9 MB | 3.63 ms / 6.2 MB | 7.01 ms / 29 MB | n/a | n/a |
| users-narrow-100-ids | 3.15 ms / 4.7 MB | 2.92 ms / 4.7 MB | 3.11 ms / 2.7 MB | 3.36 ms / 3.9 MB | 3.33 ms / 6.0 MB | 6.52 ms / 33 MB | n/a | n/a |
| users-narrow-100-keys-len | 3.49 ms / 4.8 MB | 3.42 ms / 4.8 MB | 3.45 ms / 2.7 MB | 2.92 ms / 4.0 MB | 3.59 ms / 6.0 MB | 6.68 ms / 26 MB | n/a | n/a |
| users-narrow-100-keys-publish | 2.95 ms / 4.6 MB | 3.33 ms / 4.6 MB | 3.10 ms / 2.7 MB | 3.17 ms / 4.0 MB | 6.07 ms / 6.3 MB | 7.10 ms / 29 MB | n/a | n/a |
| users-narrow-100-max-score | 3.55 ms / 5.0 MB | 3.15 ms / 5.0 MB | 3.31 ms / 2.7 MB | 3.24 ms / 4.1 MB | 3.76 ms / 6.2 MB | 6.88 ms / 29 MB | n/a | n/a |
| users-narrow-100-nested-dept | 3.56 ms / 4.6 MB | 3.68 ms / 4.6 MB | 3.21 ms / 2.6 MB | 3.32 ms / 3.9 MB | 3.14 ms / 6.1 MB | 6.62 ms / 31 MB | n/a | n/a |
| users-narrow-100-project-names | 3.46 ms / 4.7 MB | 3.32 ms / 4.7 MB | 3.36 ms / 2.7 MB | 2.98 ms / 3.9 MB | 3.16 ms / 6.2 MB | 6.20 ms / 25 MB | n/a | n/a |
| users-narrow-100-project-pair | 3.14 ms / 4.8 MB | 3.18 ms / 4.8 MB | 3.28 ms / 2.7 MB | 4.05 ms / 3.9 MB | 3.49 ms / 6.1 MB | 8.39 ms / 36 MB | n/a | n/a |
| users-narrow-100-reduce-score | 3.77 ms / 4.8 MB | 3.65 ms / 4.9 MB | 3.08 ms / 2.6 MB | 3.24 ms / 3.9 MB | 3.08 ms / 6.0 MB | n/a | n/a | n/a |
| users-narrow-100-reverse-id | 3.14 ms / 4.8 MB | 3.19 ms / 4.9 MB | 3.49 ms / 2.7 MB | 3.40 ms / 4.0 MB | 3.16 ms / 6.1 MB | 6.60 ms / 30 MB | n/a | n/a |
| users-narrow-100-select-id-stream | 3.45 ms / 4.6 MB | 3.86 ms / 4.6 MB | 3.36 ms / 2.6 MB | 2.88 ms / 3.9 MB | 3.16 ms / 6.1 MB | n/a | n/a | n/a |
| users-narrow-100-slice-length | 3.30 ms / 4.6 MB | 3.02 ms / 4.6 MB | 2.93 ms / 2.7 MB | 3.00 ms / 4.0 MB | 3.08 ms / 6.2 MB | 6.74 ms / 30 MB | n/a | n/a |
| users-narrow-100-sort-last | 3.05 ms / 4.9 MB | 3.34 ms / 4.9 MB | 3.62 ms / 2.7 MB | 2.92 ms / 4.2 MB | 3.22 ms / 6.3 MB | 6.76 ms / 32 MB | n/a | n/a |
| users-narrow-100-sum-score | 3.41 ms / 4.8 MB | 3.18 ms / 4.8 MB | 3.09 ms / 2.7 MB | 2.96 ms / 3.9 MB | 3.25 ms / 6.1 MB | n/a | n/a | n/a |
| users-narrow-100-type-path | 3.67 ms / 4.6 MB | 3.40 ms / 4.5 MB | 3.12 ms / 2.6 MB | 2.96 ms / 3.9 MB | 3.14 ms / 6.1 MB | disagreed | n/a | n/a |
| users-narrow-100-unique-scores | 3.13 ms / 5.0 MB | 3.00 ms / 5.0 MB | 2.98 ms / 2.7 MB | 2.88 ms / 4.1 MB | 3.79 ms / 6.2 MB | 7.16 ms / 31 MB | n/a | n/a |
| users-narrow-1k-all-nonneg | 3.28 ms / 4.8 MB | 3.52 ms / 4.8 MB | 4.00 ms / 3.3 MB | 3.56 ms / 4.5 MB | 4.38 ms / 6.8 MB | n/a | n/a | n/a |
| users-narrow-1k-any-high | 3.50 ms / 4.8 MB | 3.35 ms / 4.8 MB | 3.59 ms / 3.4 MB | 3.40 ms / 4.5 MB | 3.63 ms / 6.9 MB | n/a | n/a | n/a |
| users-narrow-1k-count | 3.32 ms / 4.6 MB | 3.55 ms / 4.5 MB | 3.77 ms / 3.3 MB | 3.26 ms / 4.2 MB | 3.79 ms / 6.6 MB | 7.08 ms / 19 MB | n/a | n/a |
| users-narrow-1k-descent | 3.64 ms / 5.3 MB | 3.93 ms / 5.4 MB | 4.10 ms / 3.5 MB | 3.58 ms / 4.3 MB | 4.39 ms / 7.7 MB | 10.1 ms / 46 MB | n/a | n/a |
| users-narrow-1k-filter-active | 3.82 ms / 4.7 MB | 3.30 ms / 4.7 MB | 3.95 ms / 3.3 MB | 3.72 ms / 4.3 MB | 3.64 ms / 6.8 MB | 8.31 ms / 37 MB | n/a | n/a |
| users-narrow-1k-first-id | 3.25 ms / 4.6 MB | 3.48 ms / 4.6 MB | 3.77 ms / 3.3 MB | 3.14 ms / 4.2 MB | 3.56 ms / 6.7 MB | 7.53 ms / 35 MB | n/a | n/a |
| users-narrow-1k-group-mod | 5.02 ms / 6.0 MB | 4.26 ms / 6.0 MB | 4.61 ms / 3.7 MB | 3.84 ms / 4.7 MB | 4.39 ms / 7.1 MB | 10.2 ms / 43 MB | n/a | n/a |
| users-narrow-1k-high-score | 3.72 ms / 4.7 MB | 3.41 ms / 4.7 MB | 3.87 ms / 3.4 MB | 3.70 ms / 4.3 MB | 3.69 ms / 6.7 MB | 8.67 ms / 40 MB | n/a | n/a |
| users-narrow-1k-identity | 3.36 ms / 4.4 MB | 3.39 ms / 4.4 MB | 4.19 ms / 3.4 MB | 3.93 ms / 4.2 MB | 3.88 ms / 6.7 MB | 9.17 ms / 36 MB | n/a | n/a |
| users-narrow-1k-ids | 3.17 ms / 4.8 MB | 3.45 ms / 4.8 MB | 3.61 ms / 3.3 MB | 5.86 ms / 4.2 MB | 5.55 ms / 6.7 MB | 8.43 ms / 21 MB | n/a | n/a |
| users-narrow-1k-keys-len | 3.75 ms / 4.8 MB | 3.54 ms / 4.8 MB | 3.62 ms / 3.3 MB | 3.55 ms / 4.3 MB | 3.91 ms / 6.6 MB | 7.99 ms / 35 MB | n/a | n/a |
| users-narrow-1k-keys-publish | 3.38 ms / 4.6 MB | 3.78 ms / 4.6 MB | 3.66 ms / 3.3 MB | 3.89 ms / 4.3 MB | 3.65 ms / 6.6 MB | 7.52 ms / 35 MB | n/a | n/a |
| users-narrow-1k-max-score | 3.73 ms / 5.4 MB | 3.42 ms / 5.4 MB | 4.07 ms / 3.4 MB | 3.54 ms / 4.4 MB | 4.24 ms / 6.8 MB | 9.35 ms / 38 MB | n/a | n/a |
| users-narrow-1k-nested-dept | 3.99 ms / 4.6 MB | 3.48 ms / 4.6 MB | 3.67 ms / 3.3 MB | 3.80 ms / 4.2 MB | 3.67 ms / 6.7 MB | 7.45 ms / 35 MB | n/a | n/a |
| users-narrow-1k-project-names | 3.49 ms / 4.8 MB | 3.49 ms / 4.8 MB | 3.57 ms / 3.4 MB | 3.96 ms / 4.2 MB | 3.61 ms / 6.8 MB | 8.55 ms / 38 MB | n/a | n/a |
| users-narrow-1k-project-pair | 3.68 ms / 5.1 MB | 3.51 ms / 5.1 MB | 4.74 ms / 3.8 MB | 4.54 ms / 4.3 MB | 4.03 ms / 7.5 MB | 20.5 ms / 64 MB | n/a | n/a |
| users-narrow-1k-reduce-score | 3.90 ms / 5.2 MB | 3.73 ms / 5.2 MB | 3.95 ms / 3.3 MB | 3.46 ms / 4.2 MB | 3.83 ms / 6.9 MB | n/a | n/a | n/a |
| users-narrow-1k-reverse-id | 3.99 ms / 5.8 MB | 3.80 ms / 5.8 MB | 4.36 ms / 3.4 MB | 3.39 ms / 4.3 MB | 3.77 ms / 6.5 MB | 8.45 ms / 37 MB | n/a | n/a |
| users-narrow-1k-select-id-stream | 3.36 ms / 4.7 MB | 3.70 ms / 4.7 MB | 3.69 ms / 3.3 MB | 3.87 ms / 4.2 MB | 3.73 ms / 6.7 MB | n/a | n/a | n/a |
| users-narrow-1k-slice-length | 3.51 ms / 4.6 MB | 3.96 ms / 4.6 MB | 3.66 ms / 3.3 MB | 3.30 ms / 4.3 MB | 3.67 ms / 6.7 MB | 7.50 ms / 35 MB | n/a | n/a |
| users-narrow-1k-sort-last | 3.95 ms / 5.9 MB | 4.01 ms / 5.9 MB | 4.32 ms / 3.5 MB | 3.36 ms / 4.5 MB | 4.20 ms / 6.9 MB | 9.22 ms / 38 MB | n/a | n/a |
| users-narrow-1k-sum-score | 3.70 ms / 5.2 MB | 3.77 ms / 5.2 MB | 3.86 ms / 3.4 MB | 3.64 ms / 4.2 MB | 3.66 ms / 6.6 MB | n/a | n/a | n/a |
| users-narrow-1k-type-path | 4.51 ms / 4.6 MB | 3.41 ms / 4.6 MB | 3.71 ms / 3.3 MB | 3.24 ms / 4.2 MB | 3.77 ms / 6.6 MB | disagreed | n/a | n/a |
| users-narrow-1k-unique-scores | 3.46 ms / 5.8 MB | 3.86 ms / 5.8 MB | 3.90 ms / 3.4 MB | 3.90 ms / 4.5 MB | 3.91 ms / 7.1 MB | 8.40 ms / 38 MB | n/a | n/a |
| users-narrow-5k-all-nonneg | 3.99 ms / 4.9 MB | 3.92 ms / 4.9 MB | 6.71 ms / 6.0 MB | 6.45 ms / 6.3 MB | 6.64 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-any-high | 3.94 ms / 4.9 MB | 4.08 ms / 4.9 MB | 5.28 ms / 6.0 MB | 4.13 ms / 6.3 MB | 4.92 ms / 9.6 MB | n/a | n/a | n/a |
| users-narrow-5k-count | 3.72 ms / 4.7 MB | 4.01 ms / 4.6 MB | 5.71 ms / 6.0 MB | 4.06 ms / 5.9 MB | 4.89 ms / 8.7 MB | 12.7 ms / 53 MB | n/a | n/a |
| users-narrow-5k-descent | 5.03 ms / 7.2 MB | 4.75 ms / 7.2 MB | 7.42 ms / 6.9 MB | 5.03 ms / 6.5 MB | 10.2 ms / 14 MB | 25.1 ms / 80 MB | n/a | n/a |
| users-narrow-5k-filter-active | 3.95 ms / 4.8 MB | 3.69 ms / 4.8 MB | 6.21 ms / 6.0 MB | 6.29 ms / 6.0 MB | 6.20 ms / 10 MB | 15.4 ms / 59 MB | n/a | n/a |
| users-narrow-5k-first-id | 4.78 ms / 4.7 MB | 3.97 ms / 4.7 MB | 5.07 ms / 6.0 MB | 4.24 ms / 5.8 MB | 4.58 ms / 8.5 MB | 12.5 ms / 46 MB | n/a | n/a |
| users-narrow-5k-group-mod | 8.14 ms / 9.9 MB | 8.05 ms / 9.8 MB | 10.1 ms / 6.6 MB | 5.94 ms / 7.3 MB | 7.39 ms / 11 MB | 21.7 ms / 65 MB | n/a | n/a |
| users-narrow-5k-high-score | 4.08 ms / 4.8 MB | 3.78 ms / 4.8 MB | 6.44 ms / 6.1 MB | 5.74 ms / 6.1 MB | 6.52 ms / 11 MB | 19.3 ms / 65 MB | n/a | n/a |
| users-narrow-5k-identity | 3.97 ms / 4.5 MB | 3.65 ms / 4.5 MB | 10.8 ms / 6.4 MB | 5.41 ms / 5.8 MB | 6.95 ms / 10 MB | 19.2 ms / 57 MB | n/a | n/a |
| users-narrow-5k-ids | 4.18 ms / 5.4 MB | 3.91 ms / 5.4 MB | 5.82 ms / 6.1 MB | 4.70 ms / 6.0 MB | 5.70 ms / 11 MB | 16.6 ms / 52 MB | n/a | n/a |
| users-narrow-5k-keys-len | 3.98 ms / 4.9 MB | 4.04 ms / 4.9 MB | 5.09 ms / 6.0 MB | 4.15 ms / 6.0 MB | 4.80 ms / 8.5 MB | 12.4 ms / 54 MB | n/a | n/a |
| users-narrow-5k-keys-publish | 4.10 ms / 4.7 MB | 4.03 ms / 4.7 MB | 5.10 ms / 6.0 MB | 4.01 ms / 6.0 MB | 4.83 ms / 8.6 MB | 12.4 ms / 54 MB | n/a | n/a |
| users-narrow-5k-max-score | 5.13 ms / 7.2 MB | 4.99 ms / 7.2 MB | 5.83 ms / 6.0 MB | 5.31 ms / 6.2 MB | 6.02 ms / 10 MB | 15.9 ms / 52 MB | n/a | n/a |
| users-narrow-5k-nested-dept | 4.29 ms / 4.7 MB | 4.00 ms / 4.7 MB | 5.19 ms / 6.0 MB | 4.35 ms / 5.8 MB | 4.93 ms / 8.5 MB | 12.9 ms / 54 MB | n/a | n/a |
| users-narrow-5k-project-names | 4.11 ms / 5.4 MB | 3.87 ms / 5.4 MB | 5.76 ms / 6.1 MB | 5.19 ms / 6.0 MB | 5.74 ms / 10 MB | 16.9 ms / 53 MB | n/a | n/a |
| users-narrow-5k-project-pair | 5.04 ms / 6.5 MB | 4.86 ms / 6.5 MB | 9.25 ms / 8.2 MB | 7.63 ms / 6.0 MB | 7.32 ms / 13 MB | 69.7 ms / 108 MB | n/a | n/a |
| users-narrow-5k-reduce-score | 4.78 ms / 7.0 MB | 4.83 ms / 7.0 MB | 6.33 ms / 6.0 MB | 5.36 ms / 6.0 MB | 5.93 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-reverse-id | 4.99 ms / 9.0 MB | 5.02 ms / 8.9 MB | 6.07 ms / 6.1 MB | 4.04 ms / 6.0 MB | 5.05 ms / 8.8 MB | 15.4 ms / 52 MB | n/a | n/a |
| users-narrow-5k-select-id-stream | 4.23 ms / 4.8 MB | 3.96 ms / 4.8 MB | 6.14 ms / 6.0 MB | 6.64 ms / 5.8 MB | 6.06 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-slice-length | 3.83 ms / 4.7 MB | 4.05 ms / 4.7 MB | 4.92 ms / 6.0 MB | 4.04 ms / 5.9 MB | 4.89 ms / 8.5 MB | 12.4 ms / 50 MB | n/a | n/a |
| users-narrow-5k-sort-last | 5.80 ms / 10.0 MB | 5.82 ms / 10 MB | 10.00 ms / 6.5 MB | 5.33 ms / 6.6 MB | 8.41 ms / 11 MB | 19.3 ms / 57 MB | n/a | n/a |
| users-narrow-5k-sum-score | 4.67 ms / 7.0 MB | 4.87 ms / 7.0 MB | 6.43 ms / 6.1 MB | 4.92 ms / 6.0 MB | 5.48 ms / 11 MB | n/a | n/a | n/a |
| users-narrow-5k-type-path | 4.14 ms / 4.7 MB | 3.86 ms / 4.7 MB | 5.14 ms / 6.0 MB | 3.96 ms / 5.8 MB | 4.71 ms / 8.5 MB | disagreed | n/a | n/a |
| users-narrow-5k-unique-scores | 5.28 ms / 8.8 MB | 5.27 ms / 8.8 MB | 7.24 ms / 6.1 MB | 4.85 ms / 6.8 MB | 8.22 ms / 11 MB | 15.3 ms / 52 MB | n/a | n/a |
| users-narrow-25k-all-nonneg | 5.85 ms / 5.4 MB | 5.79 ms / 5.3 MB | 17.7 ms / 19 MB | 17.9 ms / 16 MB | 17.0 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-any-high | 6.16 ms / 5.4 MB | 6.02 ms / 5.4 MB | 12.2 ms / 19 MB | 7.72 ms / 16 MB | 11.0 ms / 20 MB | n/a | n/a | n/a |
| users-narrow-25k-count | 5.57 ms / 5.1 MB | 5.25 ms / 5.1 MB | 11.2 ms / 19 MB | 7.92 ms / 15 MB | 11.1 ms / 19 MB | 31.1 ms / 82 MB | n/a | n/a |
| users-narrow-25k-descent | 9.92 ms / 20 MB | 9.79 ms / 20 MB | 23.8 ms / 24 MB | 11.7 ms / 18 MB | 29.2 ms / 34 MB | 93.6 ms / 254 MB | n/a | n/a |
| users-narrow-25k-filter-active | 5.79 ms / 5.3 MB | 5.66 ms / 5.3 MB | 17.9 ms / 19 MB | 18.8 ms / 15 MB | 14.8 ms / 21 MB | 45.6 ms / 119 MB | n/a | n/a |
| users-narrow-25k-first-id | 5.31 ms / 5.1 MB | 5.48 ms / 5.1 MB | 10.9 ms / 19 MB | 8.11 ms / 15 MB | 11.3 ms / 19 MB | 29.9 ms / 83 MB | n/a | n/a |
| users-narrow-25k-group-mod | 26.7 ms / 27 MB | 25.7 ms / 27 MB | 38.4 ms / 21 MB | 15.0 ms / 20 MB | 21.2 ms / 27 MB | 82.3 ms / 184 MB | n/a | n/a |
| users-narrow-25k-high-score | 6.05 ms / 5.3 MB | 5.70 ms / 5.3 MB | 18.8 ms / 19 MB | 16.0 ms / 16 MB | 16.8 ms / 24 MB | 69.1 ms / 146 MB | n/a | n/a |
| users-narrow-25k-identity | 4.83 ms / 5.0 MB | 4.75 ms / 5.0 MB | 24.4 ms / 20 MB | 12.0 ms / 15 MB | 14.5 ms / 23 MB | 62.1 ms / 115 MB | n/a | n/a |
| users-narrow-25k-ids | 7.09 ms / 7.8 MB | 6.80 ms / 7.9 MB | 17.0 ms / 19 MB | 10.1 ms / 16 MB | 13.3 ms / 23 MB | 50.6 ms / 121 MB | n/a | n/a |
| users-narrow-25k-keys-len | 6.07 ms / 5.3 MB | 5.42 ms / 5.3 MB | 11.5 ms / 19 MB | 8.45 ms / 15 MB | 11.2 ms / 19 MB | 30.5 ms / 85 MB | n/a | n/a |
| users-narrow-25k-keys-publish | 5.94 ms / 5.2 MB | 5.45 ms / 5.2 MB | 11.6 ms / 19 MB | 8.36 ms / 15 MB | 10.9 ms / 19 MB | 31.1 ms / 82 MB | n/a | n/a |
| users-narrow-25k-max-score | 10.1 ms / 16 MB | 9.87 ms / 16 MB | 16.1 ms / 19 MB | 12.9 ms / 16 MB | 14.2 ms / 24 MB | 51.1 ms / 131 MB | n/a | n/a |
| users-narrow-25k-nested-dept | 5.83 ms / 5.2 MB | 5.37 ms / 5.2 MB | 12.2 ms / 19 MB | 7.58 ms / 15 MB | 10.9 ms / 19 MB | 30.9 ms / 82 MB | n/a | n/a |
| users-narrow-25k-project-names | 6.44 ms / 7.8 MB | 5.98 ms / 7.9 MB | 15.6 ms / 19 MB | 12.9 ms / 16 MB | 13.7 ms / 24 MB | 55.0 ms / 122 MB | n/a | n/a |
| users-narrow-25k-project-pair | 9.72 ms / 13 MB | 9.86 ms / 13 MB | 32.0 ms / 30 MB | 26.0 ms / 16 MB | 19.6 ms / 35 MB | 321 ms / 339 MB | n/a | n/a |
| users-narrow-25k-reduce-score | 10.4 ms / 15 MB | 10.8 ms / 15 MB | 16.8 ms / 19 MB | 13.9 ms / 15 MB | 14.7 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-reverse-id | 11.1 ms / 25 MB | 10.5 ms / 25 MB | 16.7 ms / 19 MB | 7.95 ms / 15 MB | 10.7 ms / 19 MB | 43.3 ms / 129 MB | n/a | n/a |
| users-narrow-25k-select-id-stream | 6.48 ms / 5.2 MB | 6.16 ms / 5.2 MB | 17.7 ms / 19 MB | 19.6 ms / 15 MB | 14.6 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-slice-length | 5.51 ms / 5.2 MB | 5.76 ms / 5.2 MB | 12.1 ms / 19 MB | 8.03 ms / 15 MB | 10.7 ms / 19 MB | 31.7 ms / 89 MB | n/a | n/a |
| users-narrow-25k-sort-last | 13.9 ms / 26 MB | 13.1 ms / 26 MB | 38.4 ms / 21 MB | 14.1 ms / 17 MB | 29.7 ms / 28 MB | 81.9 ms / 135 MB | n/a | n/a |
| users-narrow-25k-sum-score | 10.7 ms / 15 MB | 14.2 ms / 15 MB | 18.1 ms / 19 MB | 11.2 ms / 16 MB | 13.3 ms / 24 MB | n/a | n/a | n/a |
| users-narrow-25k-type-path | 5.63 ms / 5.1 MB | 5.69 ms / 5.1 MB | 11.3 ms / 19 MB | 8.70 ms / 15 MB | 10.8 ms / 19 MB | disagreed | n/a | n/a |
| users-narrow-25k-unique-scores | 11.9 ms / 21 MB | 11.3 ms / 21 MB | 24.1 ms / 20 MB | 11.1 ms / 16 MB | 26.2 ms / 26 MB | 47.8 ms / 114 MB | n/a | n/a |
| users-narrow-50k-all-nonneg | 8.58 ms / 5.9 MB | 8.78 ms / 6.0 MB | 32.0 ms / 35 MB | 31.8 ms / 27 MB | 29.5 ms / 34 MB | n/a | n/a | n/a |
| users-narrow-50k-any-high | 10.3 ms / 5.9 MB | 10.5 ms / 5.9 MB | 20.7 ms / 35 MB | 12.6 ms / 27 MB | 17.7 ms / 32 MB | n/a | n/a | n/a |
| users-narrow-50k-count | 7.68 ms / 5.7 MB | 7.10 ms / 5.7 MB | 20.4 ms / 35 MB | 12.0 ms / 26 MB | 17.6 ms / 30 MB | 52.3 ms / 134 MB | n/a | n/a |
| users-narrow-50k-descent | 15.6 ms / 23 MB | 14.6 ms / 23 MB | 42.9 ms / 47 MB | 19.8 ms / 33 MB | 51.7 ms / 59 MB | 183 ms / 480 MB | n/a | n/a |
| users-narrow-50k-filter-active | 7.57 ms / 5.9 MB | 8.56 ms / 5.9 MB | 32.6 ms / 35 MB | 34.4 ms / 26 MB | 26.3 ms / 34 MB | 88.3 ms / 212 MB | n/a | n/a |
| users-narrow-50k-first-id | 7.45 ms / 5.7 MB | 6.95 ms / 5.8 MB | 20.3 ms / 35 MB | 11.9 ms / 26 MB | 17.4 ms / 31 MB | 51.4 ms / 136 MB | n/a | n/a |
| users-narrow-50k-group-mod | 46.8 ms / 32 MB | 46.7 ms / 32 MB | 76.6 ms / 39 MB | 30.2 ms / 34 MB | 38.3 ms / 47 MB | 160 ms / 305 MB | n/a | n/a |
| users-narrow-50k-high-score | 8.45 ms / 5.9 MB | 9.56 ms / 5.9 MB | 33.8 ms / 36 MB | 26.3 ms / 28 MB | 27.9 ms / 39 MB | 125 ms / 242 MB | n/a | n/a |
| users-narrow-50k-identity | 5.86 ms / 5.6 MB | 6.32 ms / 5.6 MB | 44.9 ms / 38 MB | 21.1 ms / 26 MB | 24.8 ms / 39 MB | 114 ms / 203 MB | n/a | n/a |
| users-narrow-50k-ids | 10.9 ms / 11 MB | 10.9 ms / 11 MB | 29.8 ms / 36 MB | 16.4 ms / 27 MB | 22.3 ms / 39 MB | 94.8 ms / 209 MB | n/a | n/a |
| users-narrow-50k-keys-len | 7.75 ms / 5.9 MB | 8.02 ms / 5.9 MB | 20.5 ms / 35 MB | 12.0 ms / 26 MB | 17.7 ms / 31 MB | 51.0 ms / 135 MB | n/a | n/a |
| users-narrow-50k-keys-publish | 7.47 ms / 5.8 MB | 7.39 ms / 5.8 MB | 20.7 ms / 35 MB | 12.4 ms / 26 MB | 17.8 ms / 30 MB | 53.5 ms / 135 MB | n/a | n/a |
| users-narrow-50k-max-score | 15.4 ms / 19 MB | 15.3 ms / 19 MB | 28.6 ms / 36 MB | 24.0 ms / 27 MB | 23.3 ms / 40 MB | 89.1 ms / 217 MB | n/a | n/a |
| users-narrow-50k-nested-dept | 7.44 ms / 5.8 MB | 9.24 ms / 5.8 MB | 20.4 ms / 35 MB | 12.1 ms / 26 MB | 17.9 ms / 31 MB | 51.3 ms / 135 MB | n/a | n/a |
| users-narrow-50k-project-names | 9.74 ms / 11 MB | 9.87 ms / 11 MB | 29.3 ms / 36 MB | 22.9 ms / 27 MB | 23.3 ms / 39 MB | 104 ms / 218 MB | n/a | n/a |
| users-narrow-50k-project-pair | 16.6 ms / 20 MB | 16.2 ms / 20 MB | 66.4 ms / 58 MB | 48.3 ms / 28 MB | 35.4 ms / 61 MB | 631 ms / 619 MB | n/a | n/a |
| users-narrow-50k-reduce-score | 17.5 ms / 18 MB | 17.2 ms / 18 MB | 31.9 ms / 35 MB | 24.6 ms / 27 MB | 25.2 ms / 35 MB | n/a | n/a | n/a |
| users-narrow-50k-reverse-id | 16.2 ms / 28 MB | 16.4 ms / 28 MB | 30.0 ms / 36 MB | 12.0 ms / 26 MB | 17.8 ms / 32 MB | 77.1 ms / 216 MB | n/a | n/a |
| users-narrow-50k-select-id-stream | 9.10 ms / 5.8 MB | 9.76 ms / 5.8 MB | 31.3 ms / 35 MB | 34.8 ms / 26 MB | 25.3 ms / 34 MB | n/a | n/a | n/a |
| users-narrow-50k-slice-length | 7.33 ms / 5.8 MB | 7.44 ms / 5.8 MB | 20.3 ms / 35 MB | 11.9 ms / 26 MB | 17.8 ms / 30 MB | 55.7 ms / 131 MB | n/a | n/a |
| users-narrow-50k-sort-last | 22.6 ms / 33 MB | 22.5 ms / 33 MB | 80.5 ms / 40 MB | 28.6 ms / 31 MB | 58.4 ms / 48 MB | 178 ms / 230 MB | n/a | n/a |
| users-narrow-50k-sum-score | 17.6 ms / 18 MB | 16.7 ms / 18 MB | 32.5 ms / 36 MB | 19.3 ms / 27 MB | 22.3 ms / 40 MB | n/a | n/a | n/a |
| users-narrow-50k-type-path | 7.34 ms / 5.7 MB | 7.40 ms / 5.7 MB | 20.1 ms / 35 MB | 12.5 ms / 26 MB | 17.8 ms / 31 MB | disagreed | n/a | n/a |
| users-narrow-50k-unique-scores | 22.2 ms / 24 MB | 19.1 ms / 24 MB | 47.2 ms / 36 MB | 18.8 ms / 27 MB | 49.5 ms / 46 MB | 87.3 ms / 208 MB | n/a | n/a |
| users-narrow-100k-all-nonneg | 18.4 ms / 7.1 MB | 13.1 ms / 7.1 MB | 58.8 ms / 70 MB | 59.8 ms / 52 MB | 52.3 ms / 61 MB | n/a | n/a | n/a |
| users-narrow-100k-any-high | 13.7 ms / 7.2 MB | 13.5 ms / 7.1 MB | 38.4 ms / 70 MB | 22.3 ms / 52 MB | 31.2 ms / 57 MB | n/a | n/a | n/a |
| users-narrow-100k-count | 11.1 ms / 6.9 MB | 11.4 ms / 6.9 MB | 36.8 ms / 70 MB | 20.5 ms / 50 MB | 29.9 ms / 55 MB | 94.4 ms / 217 MB | n/a | n/a |
| users-narrow-100k-descent | 26.0 ms / 39 MB | 25.5 ms / 39 MB | 80.6 ms / 89 MB | 35.3 ms / 62 MB | 98.1 ms / 114 MB | 341 ms / 967 MB | n/a | n/a |
| users-narrow-100k-filter-active | 12.8 ms / 7.1 MB | 11.8 ms / 7.1 MB | 60.0 ms / 70 MB | 63.9 ms / 50 MB | 45.3 ms / 62 MB | 159 ms / 342 MB | n/a | n/a |
| users-narrow-100k-first-id | 11.0 ms / 6.9 MB | 10.9 ms / 6.9 MB | 37.1 ms / 70 MB | 20.2 ms / 50 MB | 29.7 ms / 53 MB | 95.4 ms / 218 MB | n/a | n/a |
| users-narrow-100k-group-mod | 91.4 ms / 69 MB | 89.8 ms / 69 MB | 156 ms / 79 MB | 60.0 ms / 64 MB | 70.2 ms / 78 MB | 313 ms / 554 MB | n/a | n/a |
| users-narrow-100k-high-score | 13.8 ms / 7.1 MB | 13.0 ms / 7.1 MB | 63.7 ms / 70 MB | 48.1 ms / 52 MB | 50.8 ms / 72 MB | 240 ms / 454 MB | n/a | n/a |
| users-narrow-100k-identity | 8.77 ms / 6.8 MB | 7.74 ms / 6.8 MB | 80.8 ms / 76 MB | 36.8 ms / 50 MB | 43.2 ms / 71 MB | 217 ms / 369 MB | n/a | n/a |
| users-narrow-100k-ids | 21.6 ms / 17 MB | 16.5 ms / 17 MB | 55.6 ms / 72 MB | 28.8 ms / 53 MB | 38.0 ms / 72 MB | 174 ms / 342 MB | n/a | n/a |
| users-narrow-100k-keys-len | 11.4 ms / 7.1 MB | 11.4 ms / 7.1 MB | 37.4 ms / 70 MB | 20.6 ms / 50 MB | 29.9 ms / 54 MB | 95.5 ms / 221 MB | n/a | n/a |
| users-narrow-100k-keys-publish | 11.6 ms / 7.0 MB | 11.3 ms / 7.0 MB | 37.2 ms / 70 MB | 20.7 ms / 50 MB | 30.1 ms / 54 MB | 95.0 ms / 218 MB | n/a | n/a |
| users-narrow-100k-max-score | 27.0 ms / 37 MB | 26.7 ms / 37 MB | 53.6 ms / 72 MB | 41.3 ms / 53 MB | 40.5 ms / 73 MB | 171 ms / 373 MB | n/a | n/a |
| users-narrow-100k-nested-dept | 11.2 ms / 7.0 MB | 12.0 ms / 7.0 MB | 38.0 ms / 70 MB | 20.6 ms / 50 MB | 29.7 ms / 54 MB | 94.8 ms / 211 MB | n/a | n/a |
| users-narrow-100k-project-names | 16.0 ms / 17 MB | 14.5 ms / 17 MB | 53.3 ms / 72 MB | 43.9 ms / 52 MB | 39.0 ms / 71 MB | 194 ms / 392 MB | n/a | n/a |
| users-narrow-100k-project-pair | 29.0 ms / 35 MB | 28.9 ms / 35 MB | 115 ms / 115 MB | 92.8 ms / 52 MB | 65.4 ms / 116 MB | 1271 ms / 1248 MB | n/a | n/a |
| users-narrow-100k-reduce-score | 29.1 ms / 30 MB | 28.4 ms / 30 MB | 56.2 ms / 70 MB | 44.6 ms / 51 MB | 42.9 ms / 64 MB | n/a | n/a | n/a |
| users-narrow-100k-reverse-id | 28.6 ms / 62 MB | 28.3 ms / 62 MB | 65.4 ms / 72 MB | 20.7 ms / 50 MB | 30.2 ms / 56 MB | 142 ms / 402 MB | n/a | n/a |
| users-narrow-100k-select-id-stream | 15.6 ms / 7.0 MB | 14.1 ms / 7.0 MB | 58.5 ms / 70 MB | 66.0 ms / 50 MB | 44.5 ms / 63 MB | n/a | n/a | n/a |
| users-narrow-100k-slice-length | 11.2 ms / 7.0 MB | 11.7 ms / 7.0 MB | 37.2 ms / 70 MB | 20.5 ms / 50 MB | 29.7 ms / 53 MB | 100 ms / 236 MB | n/a | n/a |
| users-narrow-100k-sort-last | 40.7 ms / 74 MB | 41.0 ms / 74 MB | 165 ms / 79 MB | 58.2 ms / 59 MB | 115 ms / 88 MB | 377 ms / 426 MB | n/a | n/a |
| users-narrow-100k-sum-score | 29.5 ms / 30 MB | 29.0 ms / 30 MB | 61.2 ms / 72 MB | 34.9 ms / 53 MB | 39.5 ms / 73 MB | n/a | n/a | n/a |
| users-narrow-100k-type-path | 12.0 ms / 6.9 MB | 11.0 ms / 6.9 MB | 37.6 ms / 70 MB | 20.8 ms / 50 MB | 30.2 ms / 54 MB | disagreed | n/a | n/a |
| users-narrow-100k-unique-scores | 33.5 ms / 45 MB | 33.3 ms / 45 MB | 92.6 ms / 73 MB | 33.4 ms / 53 MB | 94.5 ms / 77 MB | 161 ms / 325 MB | n/a | n/a |
| users-narrow-200k-all-nonneg | 22.8 ms / 9.6 MB | 23.3 ms / 9.6 MB | 113 ms / 137 MB | 113 ms / 98 MB | 99.7 ms / 117 MB | n/a | n/a | n/a |
| users-narrow-200k-any-high | 23.4 ms / 9.6 MB | 22.2 ms / 9.6 MB | 69.1 ms / 137 MB | 38.3 ms / 98 MB | 55.0 ms / 108 MB | n/a | n/a | n/a |
| users-narrow-200k-count | 19.3 ms / 9.4 MB | 18.9 ms / 9.4 MB | 69.2 ms / 137 MB | 36.4 ms / 94 MB | 52.5 ms / 100 MB | 179 ms / 398 MB | n/a | n/a |
| users-narrow-200k-descent | 46.8 ms / 78 MB | 46.8 ms / 78 MB | 158 ms / 182 MB | 66.3 ms / 121 MB | 190 ms / 212 MB | 713 ms / 1796 MB | n/a | n/a |
| users-narrow-200k-filter-active | 20.5 ms / 9.5 MB | 20.2 ms / 9.5 MB | 112 ms / 137 MB | 121 ms / 94 MB | 85.1 ms / 116 MB | 307 ms / 640 MB | n/a | n/a |
| users-narrow-200k-first-id | 19.0 ms / 9.4 MB | 18.1 ms / 9.4 MB | 69.2 ms / 137 MB | 36.3 ms / 94 MB | 54.3 ms / 100 MB | 179 ms / 401 MB | n/a | n/a |
| users-narrow-200k-group-mod | 178 ms / 117 MB | 176 ms / 117 MB | 320 ms / 154 MB | 112 ms / 130 MB | 140 ms / 149 MB | 621 ms / 1083 MB | n/a | n/a |
| users-narrow-200k-high-score | 22.4 ms / 9.5 MB | 22.5 ms / 9.5 MB | 120 ms / 139 MB | 88.5 ms / 100 MB | 94.8 ms / 136 MB | 471 ms / 839 MB | n/a | n/a |
| users-narrow-200k-identity | 11.9 ms / 9.2 MB | 12.0 ms / 9.2 MB | 157 ms / 149 MB | 68.4 ms / 94 MB | 78.6 ms / 134 MB | 430 ms / 596 MB | n/a | n/a |
| users-narrow-200k-ids | 29.1 ms / 30 MB | 29.1 ms / 30 MB | 103 ms / 140 MB | 51.4 ms / 98 MB | 69.4 ms / 136 MB | 337 ms / 712 MB | n/a | n/a |
| users-narrow-200k-keys-len | 20.9 ms / 9.6 MB | 19.1 ms / 9.6 MB | 72.5 ms / 137 MB | 37.6 ms / 94 MB | 54.4 ms / 100 MB | 183 ms / 405 MB | n/a | n/a |
| users-narrow-200k-keys-publish | 20.6 ms / 9.5 MB | 19.7 ms / 9.5 MB | 70.9 ms / 137 MB | 37.3 ms / 94 MB | 54.3 ms / 100 MB | 181 ms / 408 MB | n/a | n/a |
| users-narrow-200k-max-score | 48.9 ms / 63 MB | 48.6 ms / 63 MB | 103 ms / 140 MB | 76.2 ms / 98 MB | 77.7 ms / 136 MB | 348 ms / 742 MB | n/a | n/a |
| users-narrow-200k-nested-dept | 22.0 ms / 9.4 MB | 18.9 ms / 9.4 MB | 70.1 ms / 137 MB | 37.2 ms / 94 MB | 53.6 ms / 100 MB | 179 ms / 419 MB | n/a | n/a |
| users-narrow-200k-project-names | 24.5 ms / 30 MB | 23.9 ms / 30 MB | 99.9 ms / 140 MB | 73.1 ms / 97 MB | 71.5 ms / 134 MB | 372 ms / 756 MB | n/a | n/a |
| users-narrow-200k-project-pair | 53.0 ms / 68 MB | 52.8 ms / 68 MB | 218 ms / 227 MB | 176 ms / 99 MB | 120 ms / 227 MB | 2600 ms / 2464 MB | n/a | n/a |
| users-narrow-200k-reduce-score | 51.7 ms / 53 MB | 51.4 ms / 53 MB | 105 ms / 137 MB | 82.7 ms / 97 MB | 79.6 ms / 119 MB | n/a | n/a | n/a |
| users-narrow-200k-reverse-id | 52.7 ms / 116 MB | 52.5 ms / 116 MB | 125 ms / 140 MB | 37.9 ms / 94 MB | 54.6 ms / 103 MB | 290 ms / 758 MB | n/a | n/a |
| users-narrow-200k-select-id-stream | 26.3 ms / 9.5 MB | 25.8 ms / 9.5 MB | 112 ms / 137 MB | 126 ms / 94 MB | 83.1 ms / 116 MB | n/a | n/a | n/a |
| users-narrow-200k-slice-length | 19.5 ms / 9.4 MB | 19.3 ms / 9.4 MB | 71.0 ms / 137 MB | 36.8 ms / 95 MB | 53.5 ms / 100 MB | 186 ms / 448 MB | n/a | n/a |
| users-narrow-200k-sort-last | 81.2 ms / 131 MB | 80.5 ms / 131 MB | 352 ms / 153 MB | 113 ms / 113 MB | 237 ms / 175 MB | 769 ms / 819 MB | n/a | n/a |
| users-narrow-200k-sum-score | 52.5 ms / 53 MB | 52.5 ms / 53 MB | 125 ms / 140 MB | 65.8 ms / 98 MB | 74.7 ms / 136 MB | n/a | n/a | n/a |
| users-narrow-200k-type-path | 19.5 ms / 9.4 MB | 19.2 ms / 9.4 MB | 72.0 ms / 137 MB | 37.1 ms / 94 MB | 55.2 ms / 100 MB | disagreed | n/a | n/a |
| users-narrow-200k-unique-scores | 64.4 ms / 78 MB | 64.0 ms / 78 MB | 197 ms / 140 MB | 64.2 ms / 98 MB | 190 ms / 140 MB | 317 ms / 645 MB | n/a | n/a |
| yaml-broad-100-count | 8.98 ms / 5.8 MB | 8.27 ms / 5.8 MB | n/a | 10.1 ms / 5.6 MB | 14.8 ms / 11 MB | 15.0 ms / 30 MB | 14.4 ms / 16 MB | n/a |
| yaml-broad-100-descent | 9.07 ms / 7.1 MB | 10.2 ms / 7.1 MB | n/a | 10.0 ms / 5.8 MB | 15.8 ms / 13 MB | 21.1 ms / 43 MB | n/a | n/a |
| yaml-broad-100-exact-name | 8.35 ms / 5.6 MB | 8.59 ms / 5.6 MB | n/a | 8.69 ms / 5.5 MB | 13.8 ms / 11 MB | 22.4 ms / 24 MB | 14.6 ms / 16 MB | n/a |
| yaml-broad-100-first-id | 10.1 ms / 5.6 MB | 9.64 ms / 5.6 MB | n/a | 9.73 ms / 5.5 MB | 13.1 ms / 11 MB | 14.1 ms / 22 MB | 13.9 ms / 16 MB | n/a |
| yaml-broad-100-identity | 9.64 ms / 7.0 MB | 9.19 ms / 6.8 MB | n/a | 9.22 ms / 5.5 MB | 13.4 ms / 10 MB | 18.6 ms / 35 MB | 16.7 ms / 17 MB | n/a |
| yaml-broad-100-ids | 8.55 ms / 6.0 MB | 8.79 ms / 6.0 MB | n/a | 8.66 ms / 5.5 MB | 14.2 ms / 11 MB | 14.9 ms / 24 MB | n/a | n/a |
| yaml-broad-100-keys-publish | 9.16 ms / 5.7 MB | 8.61 ms / 5.7 MB | n/a | 8.83 ms / 5.7 MB | 13.5 ms / 11 MB | disagreed | n/a | n/a |
| yaml-broad-100-nested-dept | 8.40 ms / 5.6 MB | 8.47 ms / 5.6 MB | n/a | 8.75 ms / 5.5 MB | 13.2 ms / 11 MB | 14.5 ms / 24 MB | 14.2 ms / 16 MB | n/a |
| yaml-broad-100-type-path | 8.58 ms / 5.6 MB | 8.39 ms / 5.6 MB | n/a | 8.64 ms / 5.5 MB | 14.9 ms / 11 MB | disagreed | n/a | n/a |
| yaml-broad-1k-count | 24.1 ms / 14 MB | 23.0 ms / 14 MB | n/a | 26.3 ms / 22 MB | 55.2 ms / 35 MB | 47.7 ms / 65 MB | 61.2 ms / 60 MB | n/a |
| yaml-broad-1k-descent | 29.8 ms / 22 MB | 29.0 ms / 21 MB | n/a | 29.2 ms / 24 MB | 75.8 ms / 53 MB | 96.4 ms / 210 MB | n/a | n/a |
| yaml-broad-1k-exact-name | 23.0 ms / 12 MB | 22.0 ms / 12 MB | n/a | 26.7 ms / 22 MB | 56.2 ms / 36 MB | 48.8 ms / 65 MB | 61.2 ms / 57 MB | n/a |
| yaml-broad-1k-first-id | 19.9 ms / 12 MB | 22.4 ms / 12 MB | n/a | 26.3 ms / 22 MB | 55.6 ms / 36 MB | 48.3 ms / 66 MB | 61.1 ms / 60 MB | n/a |
| yaml-broad-1k-identity | 30.1 ms / 22 MB | 29.7 ms / 22 MB | n/a | 30.7 ms / 22 MB | disagreed | 78.7 ms / 108 MB | 80.9 ms / 66 MB | n/a |
| yaml-broad-1k-ids | 23.0 ms / 14 MB | 21.7 ms / 14 MB | n/a | 27.0 ms / 22 MB | 54.8 ms / 36 MB | 49.8 ms / 68 MB | n/a | n/a |
| yaml-broad-1k-keys-publish | 22.8 ms / 12 MB | 22.8 ms / 12 MB | n/a | 27.6 ms / 22 MB | 56.5 ms / 35 MB | disagreed | n/a | n/a |
| yaml-broad-1k-nested-dept | 23.1 ms / 12 MB | 21.1 ms / 12 MB | n/a | 26.1 ms / 22 MB | 55.3 ms / 35 MB | 48.0 ms / 65 MB | 61.0 ms / 60 MB | n/a |
| yaml-broad-1k-type-path | 22.9 ms / 12 MB | 23.2 ms / 12 MB | n/a | 26.9 ms / 22 MB | 56.1 ms / 36 MB | disagreed | n/a | n/a |
| yaml-broad-5k-count | 74.2 ms / 49 MB | 74.9 ms / 49 MB | n/a | 97.1 ms / 96 MB | 237 ms / 148 MB | 183 ms / 259 MB | 250 ms / 236 MB | n/a |
| yaml-broad-5k-descent | 105 ms / 79 MB | 106 ms / 79 MB | n/a | 112 ms / 107 MB | 308 ms / 216 MB | 414 ms / 949 MB | n/a | n/a |
| yaml-broad-5k-exact-name | 71.2 ms / 41 MB | 70.7 ms / 41 MB | n/a | 96.7 ms / 96 MB | 231 ms / 149 MB | 181 ms / 259 MB | 252 ms / 236 MB | n/a |
| yaml-broad-5k-first-id | 72.0 ms / 41 MB | 73.3 ms / 41 MB | n/a | 98.9 ms / 96 MB | 231 ms / 153 MB | 183 ms / 259 MB | 245 ms / 236 MB | n/a |
| yaml-broad-5k-identity | 102 ms / 79 MB | 103 ms / 79 MB | n/a | 117 ms / 96 MB | disagreed | 343 ms / 464 MB | 346 ms / 285 MB | n/a |
| yaml-broad-5k-ids | 75.9 ms / 49 MB | 75.6 ms / 49 MB | n/a | 97.1 ms / 96 MB | 231 ms / 150 MB | 190 ms / 268 MB | n/a | n/a |
| yaml-broad-5k-keys-publish | 74.3 ms / 41 MB | 73.4 ms / 41 MB | n/a | 99.4 ms / 96 MB | 234 ms / 145 MB | disagreed | n/a | n/a |
| yaml-broad-5k-nested-dept | 74.3 ms / 41 MB | 71.8 ms / 41 MB | n/a | 96.8 ms / 96 MB | 230 ms / 148 MB | 187 ms / 259 MB | 245 ms / 236 MB | n/a |
| yaml-broad-5k-type-path | 72.9 ms / 41 MB | 73.2 ms / 41 MB | n/a | 99.4 ms / 96 MB | 235 ms / 147 MB | disagreed | n/a | n/a |
| yaml-broad-25k-count | 334 ms / 259 MB | 334 ms / 259 MB | n/a | 448 ms / 464 MB | 1125 ms / 720 MB | 864 ms / 1222 MB | 1174 ms / 1110 MB | n/a |
| yaml-broad-25k-descent | 486 ms / 380 MB | 484 ms / 380 MB | n/a | 524 ms / 521 MB | 1510 ms / 1055 MB | 1995 ms / 4835 MB | n/a | n/a |
| yaml-broad-25k-exact-name | 317 ms / 226 MB | 318 ms / 226 MB | n/a | 477 ms / 464 MB | 1103 ms / 704 MB | 865 ms / 1222 MB | 1180 ms / 1107 MB | n/a |
| yaml-broad-25k-first-id | 317 ms / 226 MB | 317 ms / 226 MB | n/a | 447 ms / 464 MB | 1116 ms / 706 MB | 862 ms / 1221 MB | 1174 ms / 1110 MB | n/a |
| yaml-broad-25k-identity | 481 ms / 376 MB | 485 ms / 375 MB | n/a | 542 ms / 464 MB | disagreed | 1631 ms / 2272 MB | 1685 ms / 1324 MB | n/a |
| yaml-broad-25k-ids | 347 ms / 256 MB | 345 ms / 256 MB | n/a | 455 ms / 464 MB | 1112 ms / 723 MB | 903 ms / 1269 MB | n/a | n/a |
| yaml-broad-25k-keys-publish | 325 ms / 226 MB | 318 ms / 226 MB | n/a | 450 ms / 464 MB | 1115 ms / 729 MB | disagreed | n/a | n/a |
| yaml-broad-25k-nested-dept | 318 ms / 226 MB | 316 ms / 226 MB | n/a | 446 ms / 464 MB | 1097 ms / 712 MB | 863 ms / 1221 MB | 1176 ms / 1108 MB | n/a |
| yaml-broad-25k-type-path | 319 ms / 226 MB | 319 ms / 226 MB | n/a | 450 ms / 464 MB | 1108 ms / 718 MB | disagreed | n/a | n/a |
| yaml-broad-50k-count | 654 ms / 544 MB | 651 ms / 544 MB | n/a | 906 ms / 924 MB | 2229 ms / 1454 MB | 1721 ms / 2422 MB | 2350 ms / 2253 MB | n/a |
| yaml-broad-50k-descent | 954 ms / 696 MB | 963 ms / 696 MB | n/a | 1030 ms / 1030 MB | 3024 ms / 2155 MB | 4090 ms / 9567 MB | n/a | n/a |
| yaml-broad-50k-exact-name | 630 ms / 446 MB | 630 ms / 446 MB | n/a | 914 ms / 924 MB | 2460 ms / 1480 MB | 2654 ms / 2428 MB | 3436 ms / 2244 MB | n/a |
| yaml-broad-50k-first-id | 633 ms / 446 MB | 638 ms / 446 MB | n/a | 878 ms / 924 MB | 2184 ms / 1490 MB | 1693 ms / 2426 MB | 2377 ms / 2296 MB | n/a |
| yaml-broad-50k-identity | 941 ms / 695 MB | 949 ms / 695 MB | n/a | 1103 ms / 924 MB | disagreed | 3287 ms / 4337 MB | 3372 ms / 2751 MB | n/a |
| yaml-broad-50k-ids | 690 ms / 546 MB | 691 ms / 546 MB | n/a | 1036 ms / 924 MB | 2759 ms / 1460 MB | 1796 ms / 2509 MB | n/a | n/a |
| yaml-broad-50k-keys-publish | 1069 ms / 446 MB | 1071 ms / 446 MB | n/a | 1632 ms / 924 MB | 2217 ms / 1482 MB | disagreed | n/a | n/a |
| yaml-broad-50k-nested-dept | 623 ms / 447 MB | 631 ms / 447 MB | n/a | 886 ms / 924 MB | 2266 ms / 1464 MB | 1707 ms / 2410 MB | 2346 ms / 2295 MB | n/a |
| yaml-broad-50k-type-path | 621 ms / 446 MB | 624 ms / 446 MB | n/a | 885 ms / 924 MB | 2230 ms / 1480 MB | disagreed | n/a | n/a |
| yaml-broad-100k-count | 1351 ms / 1020 MB | 1358 ms / 1020 MB | n/a | 1806 ms / 1845 MB | 4529 ms / 2683 MB | 3365 ms / 4833 MB | 4829 ms / 4466 MB | n/a |
| yaml-broad-100k-descent | 1905 ms / 1357 MB | 2035 ms / 1357 MB | n/a | 2136 ms / 2067 MB | 6142 ms / 4464 MB | 8286 ms / 19138 MB | n/a | n/a |
| yaml-broad-100k-exact-name | 1302 ms / 874 MB | 1342 ms / 874 MB | n/a | 1821 ms / 1845 MB | 4402 ms / 2812 MB | 4886 ms / 4802 MB | 4807 ms / 4505 MB | n/a |
| yaml-broad-100k-first-id | 1249 ms / 874 MB | 1239 ms / 874 MB | n/a | 1817 ms / 1845 MB | 4545 ms / 2946 MB | 3479 ms / 4806 MB | 4950 ms / 4594 MB | n/a |
| yaml-broad-100k-identity | 1868 ms / 1357 MB | 1863 ms / 1357 MB | n/a | 2259 ms / 1845 MB | disagreed | 6758 ms / 9031 MB | 7002 ms / 5498 MB | n/a |
| yaml-broad-100k-ids | 1432 ms / 1024 MB | 1444 ms / 1024 MB | n/a | 1800 ms / 1845 MB | 4464 ms / 2852 MB | 3511 ms / 5009 MB | n/a | n/a |
| yaml-broad-100k-keys-publish | 1246 ms / 874 MB | 1289 ms / 874 MB | n/a | 1788 ms / 1845 MB | 4462 ms / 2714 MB | disagreed | n/a | n/a |
| yaml-broad-100k-nested-dept | 1242 ms / 874 MB | 1245 ms / 874 MB | n/a | 1872 ms / 1845 MB | 4370 ms / 2969 MB | 3332 ms / 4806 MB | 4652 ms / 4455 MB | n/a |
| yaml-broad-100k-type-path | 1297 ms / 874 MB | 1254 ms / 874 MB | n/a | 1756 ms / 1845 MB | 4419 ms / 2774 MB | disagreed | n/a | n/a |
| yaml-narrow-100-count | 6.50 ms / 4.8 MB | 6.92 ms / 4.8 MB | n/a | 6.22 ms / 4.1 MB | 6.69 ms / 6.2 MB | 11.1 ms / 18 MB | 7.98 ms / 9.9 MB | n/a |
| yaml-narrow-100-descent | 6.48 ms / 5.1 MB | 7.02 ms / 5.1 MB | n/a | 6.26 ms / 4.1 MB | 6.54 ms / 6.3 MB | 11.7 ms / 18 MB | n/a | n/a |
| yaml-narrow-100-exact-name | 6.39 ms / 4.8 MB | 6.20 ms / 4.8 MB | n/a | 6.27 ms / 4.0 MB | 6.77 ms / 6.3 MB | 10.1 ms / 17 MB | error | n/a |
| yaml-narrow-100-first-id | 6.72 ms / 4.8 MB | 6.60 ms / 4.8 MB | n/a | 6.36 ms / 4.0 MB | 8.61 ms / 6.3 MB | 10.3 ms / 17 MB | 8.08 ms / 9.9 MB | n/a |
| yaml-narrow-100-identity | 6.66 ms / 4.9 MB | 6.62 ms / 4.9 MB | n/a | 6.17 ms / 4.0 MB | 6.49 ms / 6.2 MB | 12.3 ms / 17 MB | 8.11 ms / 10 MB | n/a |
| yaml-narrow-100-ids | 6.37 ms / 5.0 MB | 6.70 ms / 5.0 MB | n/a | 6.48 ms / 4.0 MB | 8.14 ms / 6.1 MB | 10.8 ms / 23 MB | n/a | n/a |
| yaml-narrow-100-keys-publish | 6.69 ms / 4.8 MB | 6.37 ms / 4.8 MB | n/a | 6.38 ms / 4.2 MB | 6.95 ms / 6.4 MB | 11.4 ms / 23 MB | n/a | n/a |
| yaml-narrow-100-nested-dept | 11.1 ms / 4.8 MB | 8.84 ms / 4.8 MB | n/a | 6.49 ms / 4.0 MB | 8.45 ms / 6.3 MB | 10.2 ms / 17 MB | error | n/a |
| yaml-narrow-100-type-path | 6.70 ms / 4.8 MB | 6.49 ms / 4.8 MB | n/a | 6.37 ms / 4.0 MB | 7.06 ms / 6.2 MB | disagreed | n/a | n/a |
| yaml-narrow-1k-count | 7.11 ms / 5.4 MB | 6.93 ms / 5.4 MB | n/a | 7.73 ms / 4.6 MB | 8.89 ms / 7.8 MB | 11.8 ms / 22 MB | 10.1 ms / 14 MB | n/a |
| yaml-narrow-1k-descent | 7.68 ms / 5.9 MB | 7.30 ms / 5.9 MB | n/a | 7.46 ms / 4.7 MB | 10.1 ms / 8.8 MB | 15.0 ms / 29 MB | n/a | n/a |
| yaml-narrow-1k-exact-name | 7.12 ms / 5.2 MB | 7.09 ms / 5.2 MB | n/a | 7.18 ms / 4.5 MB | 8.94 ms / 7.7 MB | 12.1 ms / 20 MB | error | n/a |
| yaml-narrow-1k-first-id | 7.31 ms / 5.2 MB | 7.62 ms / 5.2 MB | n/a | 7.04 ms / 4.5 MB | 9.67 ms / 7.8 MB | 11.3 ms / 20 MB | 10.3 ms / 13 MB | n/a |
| yaml-narrow-1k-identity | 7.54 ms / 5.7 MB | 7.28 ms / 5.6 MB | n/a | 6.99 ms / 4.5 MB | 10.1 ms / 8.1 MB | 13.4 ms / 29 MB | 12.8 ms / 14 MB | n/a |
| yaml-narrow-1k-ids | 7.48 ms / 5.6 MB | 8.17 ms / 5.7 MB | n/a | 8.11 ms / 4.5 MB | 9.94 ms / 8.1 MB | 13.3 ms / 28 MB | n/a | n/a |
| yaml-narrow-1k-keys-publish | 7.30 ms / 5.2 MB | 6.96 ms / 5.2 MB | n/a | 7.00 ms / 4.7 MB | 10.1 ms / 7.8 MB | 12.2 ms / 25 MB | n/a | n/a |
| yaml-narrow-1k-nested-dept | 7.22 ms / 5.2 MB | 7.27 ms / 5.2 MB | n/a | 7.25 ms / 4.5 MB | 8.90 ms / 7.6 MB | 11.4 ms / 20 MB | error | n/a |
| yaml-narrow-1k-type-path | 7.35 ms / 5.2 MB | 7.50 ms / 5.2 MB | n/a | 9.08 ms / 4.5 MB | 10.2 ms / 7.6 MB | disagreed | n/a | n/a |
| yaml-narrow-5k-count | 9.62 ms / 7.5 MB | 9.68 ms / 7.5 MB | n/a | 11.0 ms / 7.4 MB | 16.9 ms / 15 MB | 20.1 ms / 35 MB | 21.6 ms / 24 MB | n/a |
| yaml-narrow-5k-descent | 12.2 ms / 9.1 MB | 11.6 ms / 9.1 MB | n/a | 14.6 ms / 8.0 MB | 21.8 ms / 18 MB | 33.0 ms / 66 MB | n/a | n/a |
| yaml-narrow-5k-exact-name | 10.2 ms / 6.6 MB | 10.7 ms / 6.7 MB | n/a | 16.4 ms / 7.3 MB | 18.1 ms / 15 MB | 19.3 ms / 32 MB | error | n/a |
| yaml-narrow-5k-first-id | 10.4 ms / 6.6 MB | 10.5 ms / 6.6 MB | n/a | 11.3 ms / 7.3 MB | 17.4 ms / 15 MB | 18.2 ms / 32 MB | 20.2 ms / 23 MB | n/a |
| yaml-narrow-5k-identity | 13.6 ms / 9.0 MB | 11.6 ms / 9.0 MB | n/a | 12.6 ms / 7.3 MB | 18.5 ms / 16 MB | 26.4 ms / 45 MB | 26.8 ms / 28 MB | n/a |
| yaml-narrow-5k-ids | 11.5 ms / 8.4 MB | 11.0 ms / 8.4 MB | n/a | 12.6 ms / 7.5 MB | 18.0 ms / 16 MB | 24.1 ms / 45 MB | n/a | n/a |
| yaml-narrow-5k-keys-publish | 10.1 ms / 6.7 MB | 11.3 ms / 6.7 MB | n/a | 13.2 ms / 7.5 MB | 17.3 ms / 15 MB | 19.6 ms / 37 MB | n/a | n/a |
| yaml-narrow-5k-nested-dept | 9.35 ms / 6.6 MB | 10.6 ms / 6.6 MB | n/a | 12.9 ms / 7.3 MB | 18.0 ms / 15 MB | 18.9 ms / 36 MB | error | n/a |
| yaml-narrow-5k-type-path | 10.2 ms / 6.6 MB | 10.5 ms / 6.6 MB | n/a | 12.3 ms / 7.4 MB | 17.7 ms / 15 MB | disagreed | n/a | n/a |
| yaml-narrow-25k-count | 27.0 ms / 16 MB | 24.2 ms / 16 MB | n/a | 28.6 ms / 22 MB | 55.4 ms / 42 MB | 50.7 ms / 79 MB | 64.5 ms / 65 MB | n/a |
| yaml-narrow-25k-descent | 31.8 ms / 24 MB | 30.4 ms / 24 MB | n/a | 31.6 ms / 26 MB | 72.3 ms / 58 MB | 111 ms / 255 MB | n/a | n/a |
| yaml-narrow-25k-exact-name | 24.0 ms / 13 MB | 22.3 ms / 13 MB | n/a | 28.1 ms / 22 MB | 57.0 ms / 42 MB | 50.1 ms / 79 MB | error | n/a |
| yaml-narrow-25k-first-id | 22.2 ms / 13 MB | 24.8 ms / 13 MB | n/a | 28.0 ms / 22 MB | 56.9 ms / 42 MB | 50.6 ms / 79 MB | 64.4 ms / 65 MB | n/a |
| yaml-narrow-25k-identity | 30.4 ms / 24 MB | 29.2 ms / 24 MB | n/a | 32.0 ms / 22 MB | 57.7 ms / 47 MB | 80.0 ms / 113 MB | 90.4 ms / 87 MB | n/a |
| yaml-narrow-25k-ids | 36.2 ms / 20 MB | 35.9 ms / 20 MB | n/a | 31.4 ms / 23 MB | 60.4 ms / 47 MB | 72.1 ms / 113 MB | n/a | n/a |
| yaml-narrow-25k-keys-publish | 23.5 ms / 13 MB | 22.0 ms / 13 MB | n/a | 27.4 ms / 22 MB | 54.1 ms / 42 MB | 49.0 ms / 79 MB | n/a | n/a |
| yaml-narrow-25k-nested-dept | 23.8 ms / 13 MB | 22.7 ms / 13 MB | n/a | 28.9 ms / 22 MB | 55.3 ms / 42 MB | 49.5 ms / 79 MB | error | n/a |
| yaml-narrow-25k-type-path | 22.6 ms / 13 MB | 22.1 ms / 13 MB | n/a | 28.2 ms / 22 MB | 54.6 ms / 43 MB | disagreed | n/a | n/a |
| yaml-narrow-50k-count | 44.0 ms / 29 MB | 39.5 ms / 29 MB | n/a | 48.0 ms / 41 MB | 98.6 ms / 78 MB | 84.1 ms / 140 MB | 111 ms / 115 MB | n/a |
| yaml-narrow-50k-descent | 53.3 ms / 43 MB | 54.9 ms / 43 MB | n/a | 54.9 ms / 48 MB | 134 ms / 116 MB | 208 ms / 506 MB | n/a | n/a |
| yaml-narrow-50k-exact-name | 37.0 ms / 21 MB | 35.6 ms / 21 MB | n/a | 47.7 ms / 41 MB | 98.3 ms / 78 MB | 85.2 ms / 140 MB | error | n/a |
| yaml-narrow-50k-first-id | 38.5 ms / 21 MB | 38.5 ms / 21 MB | n/a | 48.0 ms / 41 MB | 97.9 ms / 79 MB | 84.4 ms / 140 MB | 112 ms / 120 MB | n/a |
| yaml-narrow-50k-identity | 50.6 ms / 43 MB | 50.2 ms / 43 MB | n/a | 56.2 ms / 41 MB | 106 ms / 87 MB | 145 ms / 193 MB | 167 ms / 151 MB | n/a |
| yaml-narrow-50k-ids | 69.4 ms / 34 MB | 67.6 ms / 34 MB | n/a | 51.7 ms / 43 MB | 102 ms / 86 MB | 127 ms / 222 MB | n/a | n/a |
| yaml-narrow-50k-keys-publish | 36.5 ms / 21 MB | 35.8 ms / 21 MB | n/a | 47.6 ms / 41 MB | 97.6 ms / 78 MB | 84.4 ms / 140 MB | n/a | n/a |
| yaml-narrow-50k-nested-dept | 36.4 ms / 21 MB | 36.3 ms / 21 MB | n/a | 48.0 ms / 41 MB | 97.2 ms / 78 MB | 84.7 ms / 140 MB | error | n/a |
| yaml-narrow-50k-type-path | 36.4 ms / 21 MB | 35.7 ms / 21 MB | n/a | 47.3 ms / 41 MB | 98.7 ms / 78 MB | disagreed | n/a | n/a |
| yaml-narrow-100k-count | 69.1 ms / 52 MB | 69.2 ms / 52 MB | n/a | 85.8 ms / 78 MB | 188 ms / 153 MB | 156 ms / 264 MB | 212 ms / 219 MB | n/a |
| yaml-narrow-100k-descent | 96.6 ms / 78 MB | 97.1 ms / 78 MB | n/a | 101 ms / 90 MB | 253 ms / 248 MB | 390 ms / 1032 MB | n/a | n/a |
| yaml-narrow-100k-exact-name | 63.4 ms / 39 MB | 63.3 ms / 39 MB | n/a | 85.7 ms / 78 MB | 186 ms / 152 MB | 157 ms / 265 MB | error | n/a |
| yaml-narrow-100k-first-id | 63.0 ms / 39 MB | 63.0 ms / 39 MB | n/a | 85.3 ms / 78 MB | 185 ms / 151 MB | 164 ms / 264 MB | 210 ms / 220 MB | n/a |
| yaml-narrow-100k-identity | 91.6 ms / 78 MB | 91.5 ms / 78 MB | n/a | 103 ms / 78 MB | 201 ms / 162 MB | 277 ms / 402 MB | 318 ms / 279 MB | n/a |
| yaml-narrow-100k-ids | 164 ms / 65 MB | 164 ms / 65 MB | n/a | 95.2 ms / 81 MB | 202 ms / 166 MB | 243 ms / 446 MB | n/a | n/a |
| yaml-narrow-100k-keys-publish | 63.5 ms / 39 MB | 63.7 ms / 39 MB | n/a | 86.1 ms / 78 MB | 186 ms / 153 MB | 159 ms / 266 MB | n/a | n/a |
| yaml-narrow-100k-nested-dept | 63.5 ms / 39 MB | 64.3 ms / 39 MB | n/a | 85.4 ms / 78 MB | 186 ms / 152 MB | 154 ms / 264 MB | error | n/a |
| yaml-narrow-100k-type-path | 63.4 ms / 39 MB | 65.7 ms / 39 MB | n/a | 89.0 ms / 78 MB | 186 ms / 152 MB | disagreed | n/a | n/a |

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

### toml-broad-100-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (292 bytes, sha256 9e7ab08959854f8d…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-broad-100-ids · yq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (292 bytes, sha256 9e7ab08959854f8d…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-broad-100-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

### toml-broad-100-keys-publish · yq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

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

### toml-broad-1k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (3892 bytes, sha256 3aec7ea7f52bdb6c…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-broad-1k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

### toml-broad-5k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (23892 bytes, sha256 ea352c0de4a58f43…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-broad-5k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

### toml-broad-25k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (138892 bytes, sha256 69f8df4146980167…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-broad-25k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

### toml-broad-50k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (288892 bytes, sha256 518833f24ad54dae…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-broad-50k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

### toml-broad-100k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (588892 bytes, sha256 bebb12fcbc88d0fb…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-broad-100k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (287 bytes, sha256 942104c6a4296e80…):

```
["active","age","bio","country","email","id","k00","k01","k02","k03","k04","k05","k06","k07","k08","k09","k10","k11","k12","k13","k14","k15","k16","k17","k18","k19","k20","k21","k22","k23","k24","k25"…
```

### toml-narrow-100-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (292 bytes, sha256 9e7ab08959854f8d…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-100-ids · yq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (292 bytes, sha256 9e7ab08959854f8d…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-100-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

```

### toml-narrow-100-keys-publish · yq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

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

### toml-narrow-1k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (3892 bytes, sha256 3aec7ea7f52bdb6c…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-1k-ids · yq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (3892 bytes, sha256 3aec7ea7f52bdb6c…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-1k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

```

### toml-narrow-1k-keys-publish · yq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

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

### toml-narrow-5k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (23892 bytes, sha256 ea352c0de4a58f43…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-5k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

```

### toml-narrow-25k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (138892 bytes, sha256 69f8df4146980167…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-25k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

```

### toml-narrow-50k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (288892 bytes, sha256 518833f24ad54dae…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-50k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

```

### toml-narrow-100k-ids · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (588892 bytes, sha256 bebb12fcbc88d0fb…):

```
[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69…
```

### toml-narrow-100k-keys-publish · jaq (oracle jqf)

expected (3 bytes, sha256 37517e5f3dc66819…):

```
[]

```

got (15 bytes, sha256 f52b7e1b7c670764…):

```
["id","score"]

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
