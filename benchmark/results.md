# jqf benchmark

These numbers are a local snapshot for guidance, not a published result.

- jqf: pgo · `57a38ae9379087940a00fd3aed7b50bd4f1f58b3`
- time: 2026-08-27T22:24:43Z
- diagnostics: `jqf: build=pgo profile=8fc2607b.8d8aaad1.aarch64-apple-darwin.821e72ca allocator=mimalloc platform=aarch64-macos pcores=6 ecores=12 pcore_source=detected`
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
- gpu: Apple M5 Max

## geomean vs jqf

| tool | wall | rss | n |
| --- | --- | --- | --- |
| jqf-serial | 1.07× (median 1.00×) | 0.96× (median 1.00×) | 564 |
| jq | 2.63× (median 2.14×) | 0.97× (median 0.96×) | 364 |
| jaq | 1.65× (median 1.39×) | 1.23× (median 0.96×) | 508 |
| gojq | 2.32× (median 1.86×) | 1.46× (median 1.29×) | 436 |
| yq | 6.27× (median 4.21×) | 9.30× (median 7.20×) | 371 |
| dasel | 2.91× (median 3.46×) | 3.04× (median 2.66×) | 84 |
| mlr | 1.44× (median 2.08×) | 6.73× (median 7.27×) | 56 |

document = json/yaml/toml. streaming = ndjson/csv records.

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.98× (median 0.99×) | 1.00× (median 1.00×) | 438 | 1.48× (median 1.02×) | 0.83× (median 1.00×) | 126 |
| jq | 2.13× (median 1.95×) | 1.51× (median 1.22×) | 294 | 6.33× (median 4.64×) | 0.15× (median 0.22×) | 70 |
| jaq | 1.40× (median 1.27×) | 1.37× (median 1.07×) | 438 | 4.54× (median 4.14×) | 0.63× (median 0.60×) | 70 |
| gojq | 1.92× (median 1.63×) | 1.71× (median 1.33×) | 366 | 6.29× (median 5.33×) | 0.63× (median 1.01×) | 70 |
| yq | 5.31× (median 4.10×) | 8.26× (median 6.99×) | 329 | 22.73× (median 19.17×) | 23.51× (median 29.06×) | 42 |
| dasel | 2.91× (median 3.46×) | 3.04× (median 2.66×) | 84 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.44× (median 2.08×) | 6.73× (median 7.27×) | 56 |

## geomean vs jqf · 100

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.97× (median 0.99×) | 1.00× (median 1.00×) | 66 | 0.96× (median 0.98×) | 1.01× (median 1.01×) | 18 |
| jq | 1.34× (median 1.28×) | 0.60× (median 0.58×) | 42 | 3.49× (median 3.48×) | 0.56× (median 0.56×) | 10 |
| jaq | 1.40× (median 1.27×) | 0.84× (median 0.84×) | 66 | 3.54× (median 3.48×) | 0.83× (median 0.83×) | 10 |
| gojq | 1.53× (median 1.31×) | 1.31× (median 1.26×) | 54 | 3.60× (median 3.53×) | 1.41× (median 1.39×) | 10 |
| yq | 2.90× (median 2.48×) | 5.78× (median 6.08×) | 58 | 2.60× (median 3.27×) | 5.11× (median 5.22×) | 6 |
| dasel | 2.14× (median 2.98×) | 2.28× (median 2.17×) | 14 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 2.37× (median 2.24×) | 6.66× (median 6.63×) | 8 |

## geomean vs jqf · 1k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.92× (median 0.98×) | 1.00× (median 1.00×) | 66 | 0.99× (median 1.01×) | 0.86× (median 1.01×) | 18 |
| jq | 1.53× (median 1.29×) | 0.85× (median 0.76×) | 42 | 4.00× (median 4.25×) | 0.38× (median 0.42×) | 10 |
| jaq | 1.44× (median 1.23×) | 0.99× (median 0.89×) | 66 | 3.62× (median 3.72×) | 0.63× (median 0.68×) | 10 |
| gojq | 1.65× (median 1.41×) | 1.38× (median 1.23×) | 54 | 4.01× (median 4.18×) | 1.40× (median 1.48×) | 10 |
| yq | 7.06× (median 3.69×) | 7.18× (median 6.95×) | 58 | 6.85× (median 10.72×) | 8.47× (median 8.81×) | 6 |
| dasel | 2.97× (median 3.63×) | 2.82× (median 2.51×) | 14 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 2.34× (median 2.30×) | 7.05× (median 7.11×) | 8 |

## geomean vs jqf · 5k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.00× (median 1.00×) | 1.00× (median 1.00×) | 66 | 1.17× (median 1.01×) | 0.82× (median 1.01×) | 18 |
| jq | 1.98× (median 1.78×) | 1.22× (median 1.09×) | 42 | 4.88× (median 4.16×) | 0.27× (median 0.35×) | 10 |
| jaq | 1.36× (median 1.25×) | 1.20× (median 1.08×) | 66 | 3.54× (median 3.53×) | 0.61× (median 0.68×) | 10 |
| gojq | 1.96× (median 1.79×) | 1.50× (median 1.26×) | 54 | 4.31× (median 4.18×) | 1.21× (median 1.49×) | 10 |
| yq | 7.45× (median 4.00×) | 7.67× (median 6.69×) | 52 | 20.36× (median 26.70×) | 14.79× (median 17.76×) | 6 |
| dasel | 2.74× (median 3.29×) | 3.10× (median 2.85×) | 14 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 2.20× (median 2.13×) | 7.28× (median 7.06×) | 8 |

## geomean vs jqf · 25k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 0.99×) | 1.00× (median 1.00×) | 66 | 1.60× (median 1.52×) | 0.81× (median 0.81×) | 18 |
| jq | 2.37× (median 2.06×) | 1.74× (median 1.24×) | 42 | 7.06× (median 7.17×) | 0.14× (median 0.20×) | 10 |
| jaq | 1.36× (median 1.37×) | 1.50× (median 1.25×) | 66 | 4.57× (median 4.67×) | 0.61× (median 0.60×) | 10 |
| gojq | 2.03× (median 1.77×) | 1.69× (median 1.18×) | 54 | 6.78× (median 6.77×) | 0.67× (median 0.92×) | 10 |
| yq | 4.82× (median 4.41×) | 8.49× (median 6.50×) | 45 | 37.99× (median 35.00×) | 31.83× (median 35.58×) | 6 |
| dasel | 3.02× (median 3.57×) | 3.42× (median 3.46×) | 14 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.32× (median 1.26×) | 6.76× (median 7.75×) | 8 |

## geomean vs jqf · 50k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 1.01× (median 1.00×) | 1.00× (median 1.00×) | 66 | 1.86× (median 2.10×) | 0.78× (median 0.89×) | 18 |
| jq | 2.61× (median 2.10×) | 2.31× (median 1.53×) | 42 | 8.49× (median 8.81×) | 0.09× (median 0.14×) | 10 |
| jaq | 1.42× (median 1.44×) | 1.72× (median 1.25×) | 66 | 5.24× (median 5.01×) | 0.58× (median 0.52×) | 10 |
| gojq | 2.15× (median 1.83×) | 2.02× (median 1.65×) | 54 | 8.53× (median 7.97×) | 0.45× (median 0.66×) | 10 |
| yq | 4.89× (median 4.46×) | 9.32× (median 7.81×) | 42 | 50.67× (median 58.90×) | 43.59× (median 47.70×) | 6 |
| dasel | 3.29× (median 3.58×) | 3.34× (median 3.53×) | 14 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 1.14× (median 1.49×) | 6.78× (median 8.29×) | 8 |

## geomean vs jqf · 100k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.99× (median 0.99×) | 1.00× (median 1.00×) | 66 | 2.07× (median 2.58×) | 0.77× (median 0.93×) | 18 |
| jq | 2.75× (median 2.20×) | 2.59× (median 1.74×) | 42 | 9.50× (median 10.12×) | 0.06× (median 0.10×) | 10 |
| jaq | 1.41× (median 1.41×) | 1.85× (median 1.32×) | 66 | 5.73× (median 5.09×) | 0.58× (median 0.53×) | 10 |
| gojq | 2.15× (median 1.78×) | 2.14× (median 1.68×) | 54 | 9.67× (median 8.55×) | 0.29× (median 0.46×) | 10 |
| yq | 5.15× (median 4.77×) | 9.85× (median 7.51×) | 43 | 61.29× (median 83.22×) | 58.29× (median 58.70×) | 6 |
| dasel | 3.48× (median 3.71×) | 3.47× (median 3.75×) | 14 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 0.92× (median 1.40×) | 6.57× (median 8.31×) | 8 |

## geomean vs jqf · 200k

| tool | document wall | document rss | n | streaming wall | streaming rss | n |
| --- | --- | --- | --- | --- | --- | --- |
| jqf-serial | 0.98× (median 0.99×) | 1.00× (median 1.00×) | 42 | 2.28× (median 3.21×) | 0.79× (median 0.96×) | 18 |
| jq | 2.88× (median 2.31×) | 2.80× (median 1.79×) | 42 | 10.50× (median 11.22×) | 0.04× (median 0.07×) | 10 |
| jaq | 1.43× (median 1.25×) | 2.30× (median 1.30×) | 42 | 6.34× (median 5.83×) | 0.62× (median 0.59×) | 10 |
| gojq | 2.10× (median 1.64×) | 2.30× (median 1.74×) | 42 | 11.23× (median 10.75×) | 0.19× (median 0.34×) | 10 |
| yq | 7.40× (median 6.45×) | 15.21× (median 9.67×) | 31 | 73.20× (median 88.87×) | 76.71× (median 77.63×) | 6 |
| dasel | n/a | n/a | 0 | n/a | n/a | 0 |
| mlr | n/a | n/a | 0 | 0.76× (median 1.15×) | 6.08× (median 7.87×) | 8 |

## results

| case | jqf | jqf-serial | jq | jaq | gojq | yq | dasel | mlr |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| csv-broad-100-count | 5.79 ms / 4.9 MB | 6.10 ms / 5.0 MB | n/a | n/a | n/a | 22.7 ms / 32 MB | n/a | 13.0 ms / 33 MB |
| csv-broad-100-first-id | 5.69 ms / 4.8 MB | 5.81 ms / 4.8 MB | n/a | n/a | n/a | 22.5 ms / 32 MB | n/a | 12.9 ms / 33 MB |
| csv-broad-100-high-count | 5.74 ms / 5.0 MB | 5.82 ms / 5.1 MB | n/a | n/a | n/a | 21.9 ms / 32 MB | n/a | 12.7 ms / 33 MB |
| csv-broad-100-sum-score | 6.28 ms / 5.0 MB | 5.66 ms / 5.0 MB | n/a | n/a | n/a | n/a | n/a | 12.7 ms / 33 MB |
| csv-broad-1k-count | 5.80 ms / 5.5 MB | 5.93 ms / 5.6 MB | n/a | n/a | n/a | 113 ms / 63 MB | n/a | 15.4 ms / 41 MB |
| csv-broad-1k-first-id | 5.74 ms / 5.4 MB | 5.69 ms / 5.4 MB | n/a | n/a | n/a | 119 ms / 69 MB | n/a | 15.4 ms / 41 MB |
| csv-broad-1k-high-count | 6.39 ms / 5.7 MB | 5.85 ms / 5.7 MB | n/a | n/a | n/a | 121 ms / 78 MB | n/a | 15.2 ms / 41 MB |
| csv-broad-1k-sum-score | 8.29 ms / 5.6 MB | 5.85 ms / 5.6 MB | n/a | n/a | n/a | n/a | n/a | 17.8 ms / 42 MB |
| csv-broad-5k-count | 10.9 ms / 8.2 MB | 10.5 ms / 8.2 MB | n/a | n/a | n/a | 507 ms / 230 MB | n/a | 22.9 ms / 67 MB |
| csv-broad-5k-first-id | 5.65 ms / 8.0 MB | 5.75 ms / 8.1 MB | n/a | n/a | n/a | 530 ms / 224 MB | n/a | 18.2 ms / 50 MB |
| csv-broad-5k-high-count | 13.5 ms / 8.3 MB | 13.5 ms / 8.3 MB | n/a | n/a | n/a | 568 ms / 261 MB | n/a | 23.5 ms / 67 MB |
| csv-broad-5k-sum-score | 10.9 ms / 8.3 MB | 10.7 ms / 8.3 MB | n/a | n/a | n/a | n/a | n/a | 25.4 ms / 67 MB |
| csv-broad-25k-count | 28.7 ms / 22 MB | 28.6 ms / 22 MB | n/a | n/a | n/a | 2516 ms / 1053 MB | n/a | 65.5 ms / 156 MB |
| csv-broad-25k-first-id | 5.92 ms / 21 MB | 5.78 ms / 21 MB | n/a | n/a | n/a | 2511 ms / 1051 MB | n/a | 18.1 ms / 50 MB |
| csv-broad-25k-high-count | 60.7 ms / 22 MB | 41.3 ms / 22 MB | n/a | n/a | n/a | 2758 ms / 1269 MB | n/a | 63.1 ms / 184 MB |
| csv-broad-25k-sum-score | 42.3 ms / 22 MB | 38.7 ms / 22 MB | n/a | n/a | n/a | n/a | n/a | 62.7 ms / 157 MB |
| csv-broad-50k-count | 53.7 ms / 38 MB | 53.8 ms / 38 MB | n/a | n/a | n/a | 5096 ms / 2064 MB | n/a | 113 ms / 302 MB |
| csv-broad-50k-first-id | 8.68 ms / 38 MB | 8.76 ms / 38 MB | n/a | n/a | n/a | 5048 ms / 2086 MB | n/a | 18.1 ms / 50 MB |
| csv-broad-50k-high-count | 78.7 ms / 38 MB | 76.4 ms / 38 MB | n/a | n/a | n/a | 5544 ms / 2547 MB | n/a | 112 ms / 333 MB |
| csv-broad-50k-sum-score | 73.8 ms / 38 MB | 73.8 ms / 38 MB | n/a | n/a | n/a | n/a | n/a | 114 ms / 270 MB |
| csv-broad-100k-count | 98.9 ms / 72 MB | 99.0 ms / 72 MB | n/a | n/a | n/a | 10155 ms / 4197 MB | n/a | 218 ms / 586 MB |
| csv-broad-100k-first-id | 13.8 ms / 71 MB | 13.5 ms / 72 MB | n/a | n/a | n/a | 10001 ms / 4206 MB | n/a | 18.2 ms / 50 MB |
| csv-broad-100k-high-count | 147 ms / 72 MB | 147 ms / 72 MB | n/a | n/a | n/a | 10965 ms / 4757 MB | n/a | 220 ms / 606 MB |
| csv-broad-100k-sum-score | 146 ms / 72 MB | 139 ms / 72 MB | n/a | n/a | n/a | n/a | n/a | 216 ms / 575 MB |
| csv-broad-200k-count | 194 ms / 139 MB | 194 ms / 139 MB | n/a | n/a | n/a | 20332 ms / 8277 MB | n/a | 403 ms / 1083 MB |
| csv-broad-200k-first-id | 21.5 ms / 138 MB | 21.6 ms / 139 MB | n/a | n/a | n/a | 20623 ms / 8218 MB | n/a | 20.3 ms / 54 MB |
| csv-broad-200k-high-count | 300 ms / 139 MB | 290 ms / 139 MB | n/a | n/a | n/a | 21814 ms / 10372 MB | n/a | 407 ms / 1101 MB |
| csv-broad-200k-sum-score | 275 ms / 139 MB | 275 ms / 139 MB | n/a | n/a | n/a | n/a | n/a | 414 ms / 1019 MB |
| csv-narrow-100-count | 5.63 ms / 4.8 MB | 5.57 ms / 4.9 MB | n/a | n/a | n/a | 8.00 ms / 19 MB | n/a | 10.9 ms / 32 MB |
| csv-narrow-100-first-id | 3.01 ms / 4.7 MB | 5.51 ms / 4.7 MB | n/a | n/a | n/a | 8.22 ms / 19 MB | n/a | 13.0 ms / 32 MB |
| csv-narrow-100-high-count | 5.84 ms / 5.0 MB | 5.66 ms / 5.0 MB | n/a | n/a | n/a | 7.88 ms / 20 MB | n/a | 13.1 ms / 32 MB |
| csv-narrow-100-sum-score | 5.62 ms / 4.9 MB | 3.25 ms / 4.9 MB | n/a | n/a | n/a | n/a | n/a | 13.2 ms / 32 MB |
| csv-narrow-1k-count | 5.64 ms / 4.9 MB | 5.56 ms / 4.9 MB | n/a | n/a | n/a | 12.9 ms / 30 MB | n/a | 13.0 ms / 32 MB |
| csv-narrow-1k-first-id | 5.75 ms / 4.7 MB | 3.11 ms / 4.8 MB | n/a | n/a | n/a | 13.4 ms / 23 MB | n/a | 13.2 ms / 33 MB |
| csv-narrow-1k-high-count | 5.80 ms / 5.0 MB | 5.71 ms / 5.0 MB | n/a | n/a | n/a | 14.9 ms / 31 MB | n/a | 12.5 ms / 33 MB |
| csv-narrow-1k-sum-score | 5.61 ms / 4.9 MB | 5.64 ms / 5.0 MB | n/a | n/a | n/a | n/a | n/a | 12.2 ms / 33 MB |
| csv-narrow-5k-count | 5.74 ms / 4.9 MB | 5.66 ms / 4.9 MB | n/a | n/a | n/a | 35.3 ms / 35 MB | n/a | 12.4 ms / 34 MB |
| csv-narrow-5k-first-id | 3.17 ms / 4.7 MB | 3.09 ms / 4.8 MB | n/a | n/a | n/a | 36.1 ms / 36 MB | n/a | 12.8 ms / 33 MB |
| csv-narrow-5k-high-count | 8.17 ms / 5.0 MB | 8.23 ms / 5.0 MB | n/a | n/a | n/a | 45.3 ms / 39 MB | n/a | 12.6 ms / 35 MB |
| csv-narrow-5k-sum-score | 8.38 ms / 4.9 MB | 8.20 ms / 5.0 MB | n/a | n/a | n/a | n/a | n/a | 12.6 ms / 35 MB |
| csv-narrow-25k-count | 15.8 ms / 5.1 MB | 15.9 ms / 5.1 MB | n/a | n/a | n/a | 146 ms / 88 MB | n/a | 14.4 ms / 42 MB |
| csv-narrow-25k-first-id | 5.87 ms / 4.9 MB | 5.87 ms / 5.0 MB | n/a | n/a | n/a | 144 ms / 95 MB | n/a | 13.0 ms / 33 MB |
| csv-narrow-25k-high-count | 23.2 ms / 5.2 MB | 23.3 ms / 5.2 MB | n/a | n/a | n/a | 182 ms / 116 MB | n/a | 15.1 ms / 46 MB |
| csv-narrow-25k-sum-score | 23.4 ms / 5.1 MB | 23.5 ms / 5.2 MB | n/a | n/a | n/a | n/a | n/a | 15.2 ms / 44 MB |
| csv-narrow-50k-count | 26.1 ms / 5.4 MB | 26.1 ms / 5.4 MB | n/a | n/a | n/a | 286 ms / 152 MB | n/a | 18.1 ms / 52 MB |
| csv-narrow-50k-first-id | 5.74 ms / 5.2 MB | 5.66 ms / 5.2 MB | n/a | n/a | n/a | 272 ms / 156 MB | n/a | 13.1 ms / 33 MB |
| csv-narrow-50k-high-count | 46.3 ms / 5.5 MB | 46.2 ms / 5.5 MB | n/a | n/a | n/a | 389 ms / 227 MB | n/a | 22.8 ms / 60 MB |
| csv-narrow-50k-sum-score | 46.5 ms / 5.4 MB | 41.0 ms / 5.5 MB | n/a | n/a | n/a | n/a | n/a | 17.8 ms / 56 MB |
| csv-narrow-100k-count | 46.2 ms / 5.9 MB | 45.6 ms / 5.9 MB | n/a | n/a | n/a | 532 ms / 294 MB | n/a | 20.8 ms / 70 MB |
| csv-narrow-100k-first-id | 5.88 ms / 5.7 MB | 5.92 ms / 5.8 MB | n/a | n/a | n/a | 539 ms / 288 MB | n/a | 12.9 ms / 33 MB |
| csv-narrow-100k-high-count | 83.1 ms / 6.0 MB | 83.7 ms / 6.0 MB | n/a | n/a | n/a | 750 ms / 411 MB | n/a | 24.2 ms / 70 MB |
| csv-narrow-100k-sum-score | 78.7 ms / 5.9 MB | 78.7 ms / 6.0 MB | n/a | n/a | n/a | n/a | n/a | 22.3 ms / 67 MB |
| csv-narrow-200k-count | 88.4 ms / 7.0 MB | 87.1 ms / 7.1 MB | n/a | n/a | n/a | 1105 ms / 575 MB | n/a | 26.5 ms / 75 MB |
| csv-narrow-200k-first-id | 5.77 ms / 6.9 MB | 6.00 ms / 6.9 MB | n/a | n/a | n/a | 1063 ms / 555 MB | n/a | 12.7 ms / 33 MB |
| csv-narrow-200k-high-count | 163 ms / 7.1 MB | 171 ms / 7.2 MB | n/a | n/a | n/a | 1489 ms / 834 MB | n/a | 32.6 ms / 109 MB |
| csv-narrow-200k-sum-score | 151 ms / 7.1 MB | 150 ms / 7.1 MB | n/a | n/a | n/a | n/a | n/a | 31.7 ms / 94 MB |
| ndjson-broad-100-first-id | 12.3 ms / 4.7 MB | 9.74 ms / 4.7 MB | 40.5 ms / 2.7 MB | 42.7 ms / 3.9 MB | 44.1 ms / 6.9 MB | n/a | n/a | n/a |
| ndjson-broad-100-identity | 11.4 ms / 4.7 MB | 10.6 ms / 4.8 MB | 40.3 ms / 2.8 MB | 37.7 ms / 3.9 MB | 39.7 ms / 7.3 MB | n/a | n/a | n/a |
| ndjson-broad-100-score | 12.4 ms / 4.7 MB | 10.0 ms / 4.7 MB | 36.6 ms / 2.7 MB | 44.3 ms / 3.9 MB | 43.3 ms / 6.8 MB | n/a | n/a | n/a |
| ndjson-broad-100-select-id | 11.1 ms / 4.9 MB | 10.6 ms / 4.9 MB | 38.7 ms / 2.7 MB | 37.7 ms / 4.0 MB | 40.2 ms / 6.9 MB | n/a | n/a | n/a |
| ndjson-broad-100-select-score | 11.1 ms / 5.0 MB | 12.8 ms / 5.0 MB | 42.2 ms / 2.7 MB | 38.8 ms / 4.0 MB | 42.7 ms / 7.2 MB | n/a | n/a | n/a |
| ndjson-broad-1k-first-id | 11.3 ms / 9.2 MB | 13.3 ms / 5.7 MB | 51.5 ms / 2.7 MB | 44.4 ms / 4.9 MB | 53.7 ms / 11 MB | n/a | n/a | n/a |
| ndjson-broad-1k-identity | 12.9 ms / 12 MB | 13.4 ms / 5.7 MB | 69.1 ms / 2.7 MB | 45.2 ms / 4.9 MB | 50.5 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-1k-score | 10.8 ms / 9.2 MB | 12.8 ms / 5.7 MB | 46.1 ms / 2.7 MB | 41.6 ms / 4.9 MB | 51.7 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-1k-select-id | 10.8 ms / 10 MB | 14.0 ms / 5.9 MB | 51.8 ms / 2.7 MB | 44.4 ms / 5.0 MB | 45.9 ms / 11 MB | n/a | n/a | n/a |
| ndjson-broad-1k-select-score | 13.9 ms / 12 MB | 19.6 ms / 5.9 MB | 62.5 ms / 2.8 MB | 45.2 ms / 5.0 MB | 51.0 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-5k-first-id | 17.9 ms / 19 MB | 22.3 ms / 10 MB | 84.2 ms / 2.7 MB | 60.0 ms / 9.2 MB | 79.4 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-identity | 18.2 ms / 26 MB | 36.4 ms / 10 MB | 216 ms / 2.8 MB | 74.1 ms / 9.2 MB | 101 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-score | 15.5 ms / 17 MB | 21.6 ms / 10 MB | 81.5 ms / 2.7 MB | 55.8 ms / 9.2 MB | 80.7 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-5k-select-id | 13.6 ms / 20 MB | 27.3 ms / 10 MB | 81.7 ms / 2.7 MB | 55.9 ms / 9.3 MB | 76.3 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-5k-select-score | 19.7 ms / 24 MB | 50.2 ms / 10 MB | 188 ms / 2.8 MB | 73.6 ms / 9.3 MB | 99.5 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-25k-first-id | 22.3 ms / 39 MB | 67.8 ms / 32 MB | 247 ms / 2.8 MB | 130 ms / 31 MB | 237 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-25k-identity | 30.2 ms / 65 MB | 113 ms / 32 MB | 703 ms / 2.8 MB | 219 ms / 31 MB | 356 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-25k-score | 22.5 ms / 40 MB | 68.2 ms / 32 MB | 247 ms / 2.7 MB | 130 ms / 31 MB | 237 ms / 12 MB | n/a | n/a | n/a |
| ndjson-broad-25k-select-id | 24.9 ms / 40 MB | 80.2 ms / 32 MB | 247 ms / 2.7 MB | 130 ms / 31 MB | 225 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-25k-select-score | 42.5 ms / 59 MB | 200 ms / 32 MB | 647 ms / 2.8 MB | 223 ms / 31 MB | 344 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-50k-first-id | 32.7 ms / 66 MB | 120 ms / 59 MB | 459 ms / 2.8 MB | 230 ms / 58 MB | 449 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-identity | 46.7 ms / 115 MB | 216 ms / 59 MB | 1327 ms / 2.8 MB | 391 ms / 58 MB | 697 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-score | 32.7 ms / 66 MB | 120 ms / 59 MB | 456 ms / 2.8 MB | 230 ms / 58 MB | 477 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-select-id | 37.4 ms / 67 MB | 151 ms / 59 MB | 456 ms / 2.7 MB | 217 ms / 58 MB | 407 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-50k-select-score | 72.2 ms / 110 MB | 384 ms / 59 MB | 1219 ms / 2.8 MB | 385 ms / 58 MB | 636 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-100k-first-id | 50.6 ms / 121 MB | 231 ms / 113 MB | 875 ms / 2.8 MB | 403 ms / 112 MB | 825 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-100k-identity | 76.1 ms / 191 MB | 416 ms / 113 MB | 2661 ms / 2.8 MB | 754 ms / 112 MB | 1280 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-100k-score | 50.6 ms / 121 MB | 231 ms / 113 MB | 845 ms / 2.8 MB | 403 ms / 112 MB | 832 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-100k-select-id | 60.5 ms / 123 MB | 289 ms / 113 MB | 864 ms / 2.8 MB | 415 ms / 112 MB | 771 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-100k-select-score | 138 ms / 193 MB | 752 ms / 113 MB | 2390 ms / 2.8 MB | 748 ms / 112 MB | 1251 ms / 13 MB | n/a | n/a | n/a |
| ndjson-broad-200k-first-id | 92.7 ms / 229 MB | 462 ms / 221 MB | 1669 ms / 2.8 MB | 776 ms / 220 MB | 1638 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-identity | 148 ms / 338 MB | 821 ms / 221 MB | 5050 ms / 2.8 MB | 1443 ms / 220 MB | 2506 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-score | 91.1 ms / 228 MB | 448 ms / 221 MB | 1667 ms / 2.7 MB | 776 ms / 220 MB | 1635 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-select-id | 110 ms / 229 MB | 565 ms / 221 MB | 1677 ms / 2.7 MB | 742 ms / 220 MB | 1522 ms / 14 MB | n/a | n/a | n/a |
| ndjson-broad-200k-select-score | 251 ms / 330 MB | 1497 ms / 221 MB | 4819 ms / 2.8 MB | 1446 ms / 220 MB | 2533 ms / 14 MB | n/a | n/a | n/a |
| ndjson-narrow-100-first-id | 9.82 ms / 4.6 MB | 10.3 ms / 4.6 MB | 40.8 ms / 2.6 MB | 42.0 ms / 3.8 MB | 43.0 ms / 6.2 MB | n/a | n/a | n/a |
| ndjson-narrow-100-identity | 11.0 ms / 4.5 MB | 10.9 ms / 4.5 MB | 40.9 ms / 2.6 MB | 41.2 ms / 3.8 MB | 42.2 ms / 6.2 MB | n/a | n/a | n/a |
| ndjson-narrow-100-score | 12.6 ms / 4.6 MB | 9.87 ms / 4.6 MB | 43.7 ms / 2.6 MB | 41.0 ms / 3.8 MB | 40.4 ms / 6.2 MB | n/a | n/a | n/a |
| ndjson-narrow-100-select-id | 11.0 ms / 4.7 MB | 10.5 ms / 4.7 MB | 36.9 ms / 2.6 MB | 40.6 ms / 3.9 MB | 36.9 ms / 6.3 MB | n/a | n/a | n/a |
| ndjson-narrow-100-select-score | 11.1 ms / 4.7 MB | 11.5 ms / 4.7 MB | 36.6 ms / 2.6 MB | 36.8 ms / 3.9 MB | 36.9 ms / 6.3 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-first-id | 10.3 ms / 4.6 MB | 10.6 ms / 4.7 MB | 38.2 ms / 2.6 MB | 37.4 ms / 3.9 MB | 43.4 ms / 7.8 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-identity | 15.5 ms / 4.5 MB | 11.6 ms / 4.6 MB | 36.9 ms / 2.6 MB | 42.0 ms / 3.9 MB | 44.0 ms / 7.9 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-score | 11.3 ms / 4.6 MB | 12.2 ms / 4.7 MB | 40.8 ms / 2.6 MB | 36.7 ms / 3.9 MB | 41.0 ms / 7.7 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-select-id | 10.2 ms / 4.8 MB | 10.6 ms / 4.8 MB | 43.2 ms / 2.6 MB | 42.3 ms / 3.9 MB | 43.7 ms / 8.4 MB | n/a | n/a | n/a |
| ndjson-narrow-1k-select-score | 10.7 ms / 4.8 MB | 10.6 ms / 4.8 MB | 36.7 ms / 2.7 MB | 44.8 ms / 3.9 MB | 44.8 ms / 8.6 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-first-id | 12.4 ms / 4.7 MB | 13.2 ms / 4.8 MB | 42.9 ms / 2.6 MB | 45.2 ms / 4.0 MB | 46.6 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-identity | 12.8 ms / 4.7 MB | 13.1 ms / 4.7 MB | 46.4 ms / 2.7 MB | 40.0 ms / 4.0 MB | 44.9 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-score | 13.5 ms / 4.8 MB | 13.0 ms / 4.8 MB | 47.3 ms / 2.6 MB | 41.7 ms / 4.0 MB | 45.9 ms / 11 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-select-id | 13.0 ms / 4.8 MB | 13.2 ms / 4.9 MB | 40.6 ms / 2.6 MB | 43.7 ms / 4.0 MB | 44.2 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-5k-select-score | 12.9 ms / 4.9 MB | 12.6 ms / 4.9 MB | 43.5 ms / 2.7 MB | 44.8 ms / 4.0 MB | 50.8 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-first-id | 13.5 ms / 7.4 MB | 29.4 ms / 5.2 MB | 54.3 ms / 2.7 MB | 55.5 ms / 4.4 MB | 73.4 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-identity | 14.3 ms / 8.1 MB | 20.7 ms / 5.1 MB | 63.3 ms / 2.7 MB | 55.4 ms / 4.4 MB | 73.8 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-score | 14.9 ms / 7.4 MB | 26.0 ms / 5.2 MB | 53.8 ms / 2.7 MB | 55.3 ms / 4.4 MB | 75.7 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-select-id | 16.5 ms / 7.4 MB | 26.4 ms / 5.3 MB | 51.9 ms / 2.7 MB | 49.9 ms / 4.5 MB | 56.1 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-25k-select-score | 17.9 ms / 8.3 MB | 28.5 ms / 5.4 MB | 63.9 ms / 2.7 MB | 58.7 ms / 4.5 MB | 78.2 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-first-id | 16.4 ms / 9.9 MB | 40.1 ms / 5.9 MB | 68.5 ms / 2.7 MB | 70.5 ms / 5.0 MB | 104 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-identity | 16.1 ms / 11 MB | 32.3 ms / 5.8 MB | 87.5 ms / 2.7 MB | 70.5 ms / 5.0 MB | 115 ms / 12 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-score | 16.9 ms / 9.8 MB | 40.3 ms / 5.9 MB | 71.0 ms / 2.7 MB | 79.2 ms / 5.0 MB | 115 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-select-id | 19.1 ms / 9.5 MB | 42.0 ms / 5.9 MB | 70.6 ms / 2.7 MB | 62.9 ms / 5.1 MB | 72.2 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-50k-select-score | 19.2 ms / 12 MB | 47.6 ms / 6.0 MB | 91.7 ms / 2.7 MB | 80.7 ms / 5.1 MB | 116 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-first-id | 23.7 ms / 13 MB | 67.5 ms / 7.0 MB | 103 ms / 2.7 MB | 111 ms / 6.2 MB | 177 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-identity | 22.5 ms / 16 MB | 52.4 ms / 6.9 MB | 134 ms / 2.7 MB | 106 ms / 6.2 MB | 181 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-score | 22.2 ms / 13 MB | 70.5 ms / 7.0 MB | 98.3 ms / 2.7 MB | 106 ms / 6.2 MB | 176 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-select-id | 25.6 ms / 13 MB | 72.4 ms / 7.1 MB | 104 ms / 2.7 MB | 88.5 ms / 6.2 MB | 108 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-100k-select-score | 27.7 ms / 15 MB | 82.9 ms / 7.1 MB | 144 ms / 2.7 MB | 125 ms / 6.2 MB | 191 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-first-id | 32.9 ms / 17 MB | 125 ms / 9.5 MB | 165 ms / 2.7 MB | 183 ms / 8.7 MB | 315 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-identity | 30.2 ms / 21 MB | 92.5 ms / 9.4 MB | 219 ms / 2.7 MB | 178 ms / 8.7 MB | 344 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-score | 32.7 ms / 18 MB | 126 ms / 9.5 MB | 156 ms / 2.7 MB | 183 ms / 8.7 MB | 302 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-select-id | 38.9 ms / 16 MB | 130 ms / 9.6 MB | 172 ms / 2.7 MB | 145 ms / 8.7 MB | 175 ms / 13 MB | n/a | n/a | n/a |
| ndjson-narrow-200k-select-score | 37.6 ms / 21 MB | 153 ms / 9.6 MB | 242 ms / 2.7 MB | 214 ms / 8.7 MB | 353 ms / 13 MB | n/a | n/a | n/a |
| toml-broad-100-count | 11.8 ms / 6.0 MB | 17.1 ms / 6.0 MB | n/a | 5.96 ms / 5.4 MB | n/a | 156 ms / 42 MB | 8.08 ms / 14 MB | n/a |
| toml-broad-100-descent | 10.9 ms / 6.3 MB | 11.6 ms / 6.4 MB | n/a | 11.6 ms / 5.5 MB | n/a | 159 ms / 37 MB | n/a | n/a |
| toml-broad-100-first-id | 11.6 ms / 4.9 MB | 11.8 ms / 4.9 MB | n/a | 6.15 ms / 5.3 MB | n/a | 150 ms / 43 MB | 8.30 ms / 14 MB | n/a |
| toml-broad-100-identity | 11.5 ms / 6.2 MB | 11.7 ms / 6.2 MB | n/a | 8.69 ms / 5.3 MB | n/a | 155 ms / 43 MB | 13.3 ms / 15 MB | n/a |
| toml-broad-100-ids | 12.6 ms / 6.1 MB | 11.8 ms / 6.1 MB | n/a | 6.02 ms / 5.3 MB | n/a | 156 ms / 44 MB | n/a | n/a |
| toml-broad-100-nested-dept | 11.4 ms / 4.9 MB | 11.5 ms / 4.9 MB | n/a | 11.6 ms / 5.3 MB | n/a | 147 ms / 38 MB | 13.7 ms / 13 MB | n/a |
| toml-broad-1k-count | 21.4 ms / 18 MB | 19.9 ms / 18 MB | n/a | 21.3 ms / 15 MB | n/a | 12657 ms / 193 MB | 28.2 ms / 33 MB | n/a |
| toml-broad-1k-descent | 27.4 ms / 21 MB | 25.0 ms / 21 MB | n/a | 31.1 ms / 16 MB | n/a | 12560 ms / 197 MB | n/a | n/a |
| toml-broad-1k-first-id | 17.5 ms / 6.5 MB | 20.4 ms / 6.6 MB | n/a | 21.3 ms / 15 MB | n/a | 13384 ms / 192 MB | 28.4 ms / 33 MB | n/a |
| toml-broad-1k-identity | 21.6 ms / 21 MB | 22.3 ms / 21 MB | n/a | 23.6 ms / 15 MB | n/a | 12497 ms / 191 MB | 48.7 ms / 44 MB | n/a |
| toml-broad-1k-ids | 21.0 ms / 18 MB | 19.9 ms / 18 MB | n/a | 18.6 ms / 15 MB | n/a | 12508 ms / 191 MB | n/a | n/a |
| toml-broad-1k-nested-dept | 17.2 ms / 6.6 MB | 17.2 ms / 6.6 MB | n/a | 28.2 ms / 15 MB | n/a | 12368 ms / 189 MB | 38.4 ms / 33 MB | n/a |
| toml-broad-5k-count | 52.7 ms / 65 MB | 51.9 ms / 65 MB | n/a | 78.9 ms / 70 MB | n/a | timeout | 100 ms / 107 MB | n/a |
| toml-broad-5k-descent | 64.7 ms / 78 MB | 64.4 ms / 78 MB | n/a | 107 ms / 74 MB | n/a | timeout | n/a | n/a |
| toml-broad-5k-first-id | 36.2 ms / 14 MB | 38.9 ms / 14 MB | n/a | 78.9 ms / 70 MB | n/a | timeout | 101 ms / 109 MB | n/a |
| toml-broad-5k-identity | 60.3 ms / 78 MB | 60.0 ms / 78 MB | n/a | 99.1 ms / 70 MB | n/a | timeout | 200 ms / 157 MB | n/a |
| toml-broad-5k-ids | 58.0 ms / 65 MB | 56.8 ms / 65 MB | n/a | 81.4 ms / 70 MB | n/a | timeout | n/a | n/a |
| toml-broad-5k-nested-dept | 33.8 ms / 14 MB | 32.7 ms / 14 MB | n/a | 87.0 ms / 70 MB | n/a | timeout | 109 ms / 109 MB | n/a |
| toml-broad-25k-count | 204 ms / 285 MB | 198 ms / 274 MB | n/a | 371 ms / 285 MB | n/a | timeout | 458 ms / 508 MB | n/a |
| toml-broad-25k-descent | 258 ms / 304 MB | 257 ms / 305 MB | n/a | 453 ms / 332 MB | n/a | timeout | n/a | n/a |
| toml-broad-25k-first-id | 123 ms / 50 MB | 128 ms / 49 MB | n/a | 372 ms / 284 MB | n/a | timeout | 458 ms / 510 MB | n/a |
| toml-broad-25k-identity | 239 ms / 304 MB | 237 ms / 296 MB | n/a | 472 ms / 284 MB | n/a | timeout | 958 ms / 807 MB | n/a |
| toml-broad-25k-ids | 318 ms / 287 MB | 325 ms / 274 MB | n/a | 371 ms / 284 MB | n/a | timeout | n/a | n/a |
| toml-broad-25k-nested-dept | 122 ms / 50 MB | 122 ms / 49 MB | n/a | 396 ms / 284 MB | n/a | timeout | 473 ms / 513 MB | n/a |
| toml-broad-50k-count | 404 ms / 617 MB | 395 ms / 617 MB | n/a | 736 ms / 545 MB | n/a | timeout | 915 ms / 964 MB | n/a |
| toml-broad-50k-descent | 512 ms / 669 MB | 510 ms / 668 MB | n/a | 901 ms / 630 MB | n/a | timeout | n/a | n/a |
| toml-broad-50k-first-id | 226 ms / 93 MB | 230 ms / 93 MB | n/a | 741 ms / 545 MB | n/a | timeout | 914 ms / 965 MB | n/a |
| toml-broad-50k-identity | 463 ms / 673 MB | 499 ms / 674 MB | n/a | 921 ms / 545 MB | n/a | timeout | 1897 ms / 1574 MB | n/a |
| toml-broad-50k-ids | 864 ms / 618 MB | 835 ms / 618 MB | n/a | 738 ms / 545 MB | n/a | timeout | n/a | n/a |
| toml-broad-50k-nested-dept | 225 ms / 93 MB | 225 ms / 93 MB | n/a | 744 ms / 545 MB | n/a | timeout | 930 ms / 961 MB | n/a |
| toml-broad-100k-count | 779 ms / 1219 MB | 824 ms / 1219 MB | n/a | 1462 ms / 1087 MB | n/a | timeout | 1807 ms / 1912 MB | n/a |
| toml-broad-100k-descent | 1066 ms / 1319 MB | 1075 ms / 1319 MB | n/a | 1878 ms / 1272 MB | n/a | timeout | n/a | n/a |
| toml-broad-100k-first-id | 434 ms / 180 MB | 470 ms / 180 MB | n/a | 1462 ms / 1086 MB | n/a | timeout | 1799 ms / 1912 MB | n/a |
| toml-broad-100k-identity | 927 ms / 1321 MB | 984 ms / 1321 MB | n/a | 1845 ms / 1086 MB | n/a | timeout | 3783 ms / 3009 MB | n/a |
| toml-broad-100k-ids | 2591 ms / 1218 MB | 2843 ms / 1218 MB | n/a | 1477 ms / 1086 MB | n/a | timeout | n/a | n/a |
| toml-broad-100k-nested-dept | 468 ms / 180 MB | 467 ms / 180 MB | n/a | 1569 ms / 1086 MB | n/a | timeout | 1941 ms / 1910 MB | n/a |
| toml-narrow-100-count | 13.8 ms / 4.9 MB | 11.8 ms / 4.9 MB | n/a | 48.0 ms / 4.2 MB | n/a | 60.8 ms / 28 MB | 50.6 ms / 9.9 MB | n/a |
| toml-narrow-100-descent | 11.3 ms / 5.0 MB | 10.6 ms / 5.0 MB | n/a | 10.7 ms / 4.2 MB | n/a | 23.8 ms / 29 MB | n/a | n/a |
| toml-narrow-100-first-id | 15.6 ms / 4.8 MB | 12.0 ms / 4.8 MB | n/a | 48.7 ms / 4.1 MB | n/a | 61.3 ms / 29 MB | 50.8 ms / 9.6 MB | n/a |
| toml-narrow-100-identity | 21.4 ms / 4.8 MB | 10.9 ms / 4.8 MB | n/a | 48.8 ms / 4.1 MB | n/a | 63.5 ms / 29 MB | 50.9 ms / 10 MB | n/a |
| toml-narrow-100-ids | 15.3 ms / 5.0 MB | 11.8 ms / 5.0 MB | n/a | 48.2 ms / 4.2 MB | n/a | 61.0 ms / 29 MB | n/a | n/a |
| toml-narrow-100-nested-dept | 11.8 ms / 4.8 MB | 11.9 ms / 4.8 MB | n/a | 11.2 ms / 4.1 MB | n/a | 24.0 ms / 29 MB | error | n/a |
| toml-narrow-1k-count | 14.6 ms / 5.7 MB | 17.1 ms / 5.7 MB | n/a | 49.7 ms / 5.6 MB | n/a | 740 ms / 31 MB | 53.7 ms / 13 MB | n/a |
| toml-narrow-1k-descent | 13.3 ms / 5.8 MB | 10.9 ms / 5.8 MB | n/a | 10.6 ms / 5.6 MB | n/a | 712 ms / 32 MB | n/a | n/a |
| toml-narrow-1k-first-id | 14.2 ms / 5.1 MB | 11.2 ms / 5.1 MB | n/a | 51.0 ms / 5.5 MB | n/a | 733 ms / 38 MB | 53.2 ms / 13 MB | n/a |
| toml-narrow-1k-identity | 14.9 ms / 5.6 MB | 17.1 ms / 5.6 MB | n/a | 51.8 ms / 5.5 MB | n/a | 732 ms / 37 MB | 54.1 ms / 14 MB | n/a |
| toml-narrow-1k-ids | 13.5 ms / 5.9 MB | 11.5 ms / 5.9 MB | n/a | 52.0 ms / 5.6 MB | n/a | 730 ms / 38 MB | n/a | n/a |
| toml-narrow-1k-nested-dept | 11.4 ms / 5.1 MB | 11.6 ms / 5.1 MB | n/a | 10.9 ms / 5.5 MB | n/a | 691 ms / 37 MB | error | n/a |
| toml-narrow-5k-count | 16.8 ms / 9.9 MB | 17.4 ms / 10.0 MB | n/a | 8.30 ms / 11 MB | n/a | 17919 ms / 73 MB | 12.8 ms / 19 MB | n/a |
| toml-narrow-5k-descent | 17.0 ms / 10 MB | 15.9 ms / 10 MB | n/a | 19.4 ms / 11 MB | n/a | 18579 ms / 73 MB | n/a | n/a |
| toml-narrow-5k-first-id | 14.3 ms / 6.7 MB | 17.1 ms / 6.7 MB | n/a | 54.4 ms / 11 MB | n/a | 17887 ms / 73 MB | 59.9 ms / 19 MB | n/a |
| toml-narrow-5k-identity | 18.7 ms / 10.0 MB | 16.6 ms / 9.9 MB | n/a | 8.17 ms / 11 MB | n/a | 17646 ms / 73 MB | 18.2 ms / 23 MB | n/a |
| toml-narrow-5k-ids | 21.5 ms / 10 MB | 19.8 ms / 10 MB | n/a | 8.15 ms / 11 MB | n/a | 17942 ms / 71 MB | n/a | n/a |
| toml-narrow-5k-nested-dept | 17.0 ms / 6.7 MB | 16.5 ms / 6.7 MB | n/a | 19.1 ms / 11 MB | n/a | 18684 ms / 74 MB | error | n/a |
| toml-narrow-25k-count | 29.8 ms / 26 MB | 24.8 ms / 27 MB | n/a | 26.0 ms / 50 MB | n/a | timeout | 43.3 ms / 47 MB | n/a |
| toml-narrow-25k-descent | 26.7 ms / 27 MB | 26.5 ms / 27 MB | n/a | 37.0 ms / 51 MB | n/a | timeout | n/a | n/a |
| toml-narrow-25k-first-id | 21.4 ms / 14 MB | 19.2 ms / 14 MB | n/a | 25.9 ms / 50 MB | n/a | timeout | 40.0 ms / 46 MB | n/a |
| toml-narrow-25k-identity | 25.6 ms / 26 MB | 24.9 ms / 27 MB | n/a | 31.2 ms / 50 MB | n/a | timeout | 74.6 ms / 68 MB | n/a |
| toml-narrow-25k-ids | 38.3 ms / 27 MB | 37.2 ms / 27 MB | n/a | 28.5 ms / 50 MB | n/a | timeout | n/a | n/a |
| toml-narrow-25k-nested-dept | 19.2 ms / 14 MB | 19.0 ms / 14 MB | n/a | 34.7 ms / 50 MB | n/a | timeout | error | n/a |
| toml-narrow-50k-count | 32.8 ms / 53 MB | 34.3 ms / 54 MB | n/a | 48.6 ms / 80 MB | n/a | timeout | 74.9 ms / 81 MB | n/a |
| toml-narrow-50k-descent | 42.3 ms / 54 MB | 39.2 ms / 54 MB | n/a | 61.8 ms / 80 MB | n/a | timeout | n/a | n/a |
| toml-narrow-50k-first-id | 23.7 ms / 24 MB | 28.6 ms / 24 MB | n/a | 48.7 ms / 80 MB | n/a | timeout | 73.8 ms / 82 MB | n/a |
| toml-narrow-50k-identity | 37.4 ms / 53 MB | 37.2 ms / 53 MB | n/a | 56.3 ms / 80 MB | n/a | timeout | 133 ms / 122 MB | n/a |
| toml-narrow-50k-ids | 67.9 ms / 55 MB | 72.2 ms / 55 MB | n/a | 51.1 ms / 80 MB | n/a | timeout | n/a | n/a |
| toml-narrow-50k-nested-dept | 24.1 ms / 24 MB | 24.6 ms / 24 MB | n/a | 56.9 ms / 80 MB | n/a | timeout | error | n/a |
| toml-narrow-100k-count | 52.5 ms / 94 MB | 52.0 ms / 94 MB | n/a | 88.9 ms / 139 MB | n/a | timeout | 144 ms / 159 MB | n/a |
| toml-narrow-100k-descent | 66.4 ms / 94 MB | 64.4 ms / 94 MB | n/a | 112 ms / 139 MB | n/a | timeout | n/a | n/a |
| toml-narrow-100k-first-id | 34.2 ms / 42 MB | 34.6 ms / 42 MB | n/a | 91.2 ms / 139 MB | n/a | timeout | 139 ms / 150 MB | n/a |
| toml-narrow-100k-identity | 62.0 ms / 94 MB | 60.1 ms / 94 MB | n/a | 109 ms / 139 MB | n/a | timeout | 265 ms / 229 MB | n/a |
| toml-narrow-100k-ids | 176 ms / 95 MB | 177 ms / 95 MB | n/a | 101 ms / 139 MB | n/a | timeout | n/a | n/a |
| toml-narrow-100k-nested-dept | 36.6 ms / 42 MB | 34.0 ms / 42 MB | n/a | 96.7 ms / 139 MB | n/a | timeout | error | n/a |
| users-broad-100-all-nonneg | 5.95 ms / 6.1 MB | 5.83 ms / 6.1 MB | 7.62 ms / 3.6 MB | 7.58 ms / 5.0 MB | 7.84 ms / 7.1 MB | n/a | n/a | n/a |
| users-broad-100-any-high | 6.04 ms / 6.0 MB | 5.66 ms / 6.0 MB | 7.32 ms / 3.6 MB | 7.99 ms / 5.0 MB | 7.81 ms / 7.2 MB | n/a | n/a | n/a |
| users-broad-100-count | 5.77 ms / 4.6 MB | 5.73 ms / 4.5 MB | 7.81 ms / 3.6 MB | 7.36 ms / 4.8 MB | 7.72 ms / 7.0 MB | 14.7 ms / 37 MB | n/a | n/a |
| users-broad-100-descent | 5.71 ms / 6.0 MB | 5.73 ms / 5.9 MB | 5.84 ms / 4.1 MB | 5.77 ms / 5.0 MB | 7.91 ms / 10 MB | 13.6 ms / 53 MB | n/a | n/a |
| users-broad-100-filter-active | 5.81 ms / 6.3 MB | 5.72 ms / 6.2 MB | 7.56 ms / 3.6 MB | 7.45 ms / 4.8 MB | 8.20 ms / 7.1 MB | 14.1 ms / 40 MB | n/a | n/a |
| users-broad-100-first-id | 5.85 ms / 4.6 MB | 5.79 ms / 4.6 MB | 7.51 ms / 3.6 MB | 7.37 ms / 4.7 MB | 7.60 ms / 7.1 MB | 14.2 ms / 37 MB | n/a | n/a |
| users-broad-100-group-mod | 5.82 ms / 6.4 MB | 5.81 ms / 6.4 MB | 7.79 ms / 3.7 MB | 7.93 ms / 5.0 MB | 8.35 ms / 7.4 MB | 16.3 ms / 51 MB | n/a | n/a |
| users-broad-100-high-score | 5.87 ms / 6.3 MB | 5.84 ms / 6.3 MB | 7.59 ms / 3.6 MB | 7.51 ms / 4.8 MB | 7.94 ms / 7.1 MB | 14.1 ms / 41 MB | n/a | n/a |
| users-broad-100-identity | 5.79 ms / 6.5 MB | 5.90 ms / 6.4 MB | 10.4 ms / 3.7 MB | 7.60 ms / 4.7 MB | 7.41 ms / 7.2 MB | 16.4 ms / 42 MB | n/a | n/a |
| users-broad-100-ids | 5.84 ms / 6.3 MB | 5.74 ms / 6.3 MB | 7.93 ms / 3.6 MB | 7.56 ms / 4.7 MB | 7.52 ms / 7.0 MB | 14.1 ms / 37 MB | n/a | n/a |
| users-broad-100-keys-len | 5.93 ms / 4.9 MB | 5.81 ms / 4.9 MB | 7.84 ms / 3.6 MB | 7.53 ms / 4.9 MB | 8.09 ms / 7.0 MB | 14.0 ms / 37 MB | n/a | n/a |
| users-broad-100-max-score | 6.05 ms / 6.0 MB | 5.79 ms / 6.0 MB | 8.16 ms / 3.6 MB | 7.51 ms / 4.9 MB | 8.96 ms / 7.1 MB | 15.5 ms / 38 MB | n/a | n/a |
| users-broad-100-nested-dept | 5.92 ms / 4.6 MB | 5.72 ms / 4.6 MB | 5.66 ms / 3.6 MB | 5.80 ms / 4.7 MB | 5.57 ms / 7.0 MB | 10.6 ms / 37 MB | n/a | n/a |
| users-broad-100-project-names | 6.19 ms / 5.9 MB | 5.67 ms / 5.9 MB | 8.02 ms / 3.6 MB | 8.19 ms / 4.7 MB | 9.19 ms / 7.0 MB | 14.7 ms / 38 MB | n/a | n/a |
| users-broad-100-project-pair | 5.87 ms / 6.1 MB | 5.79 ms / 6.1 MB | 7.95 ms / 3.6 MB | 7.68 ms / 4.7 MB | 8.03 ms / 7.2 MB | 14.9 ms / 24 MB | n/a | n/a |
| users-broad-100-reduce-score | 5.86 ms / 5.0 MB | 5.70 ms / 5.0 MB | 7.55 ms / 3.6 MB | 7.57 ms / 4.8 MB | 7.65 ms / 7.2 MB | n/a | n/a | n/a |
| users-broad-100-reverse-id | 5.81 ms / 6.3 MB | 5.87 ms / 6.2 MB | 7.73 ms / 3.6 MB | 7.42 ms / 4.8 MB | 8.07 ms / 7.0 MB | 15.5 ms / 41 MB | n/a | n/a |
| users-broad-100-slice-length | 5.88 ms / 4.6 MB | 5.86 ms / 4.6 MB | 7.40 ms / 3.6 MB | 7.28 ms / 4.8 MB | 7.41 ms / 7.0 MB | 13.4 ms / 37 MB | n/a | n/a |
| users-broad-100-sort-last | 5.80 ms / 6.4 MB | 5.79 ms / 6.3 MB | 7.63 ms / 3.7 MB | 7.59 ms / 5.0 MB | 9.10 ms / 7.2 MB | 13.7 ms / 41 MB | n/a | n/a |
| users-broad-100-sum-score | 6.00 ms / 6.0 MB | 5.83 ms / 6.0 MB | 7.65 ms / 3.6 MB | 7.47 ms / 4.8 MB | 7.98 ms / 7.1 MB | n/a | n/a | n/a |
| users-broad-100-unique-scores | 5.78 ms / 6.0 MB | 5.83 ms / 6.0 MB | 7.42 ms / 3.6 MB | 7.81 ms / 4.9 MB | 10.9 ms / 7.2 MB | 13.3 ms / 37 MB | n/a | n/a |
| users-broad-1k-all-nonneg | 11.1 ms / 14 MB | 8.44 ms / 14 MB | 19.2 ms / 12 MB | 13.3 ms / 13 MB | 16.3 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-any-high | 8.49 ms / 14 MB | 8.47 ms / 14 MB | 15.9 ms / 12 MB | 11.4 ms / 13 MB | 15.7 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-count | 5.86 ms / 5.5 MB | 5.84 ms / 5.5 MB | 16.1 ms / 12 MB | 14.1 ms / 13 MB | 15.9 ms / 15 MB | 31.5 ms / 70 MB | n/a | n/a |
| users-broad-1k-descent | 11.0 ms / 13 MB | 8.26 ms / 13 MB | 21.4 ms / 16 MB | 11.0 ms / 15 MB | 29.1 ms / 23 MB | 69.4 ms / 222 MB | n/a | n/a |
| users-broad-1k-filter-active | 11.1 ms / 15 MB | 11.1 ms / 15 MB | 18.3 ms / 12 MB | 13.6 ms / 13 MB | 15.9 ms / 15 MB | 39.4 ms / 100 MB | n/a | n/a |
| users-broad-1k-first-id | 5.74 ms / 5.5 MB | 5.72 ms / 5.5 MB | 15.7 ms / 12 MB | 13.8 ms / 13 MB | 16.2 ms / 15 MB | 30.8 ms / 70 MB | n/a | n/a |
| users-broad-1k-group-mod | 11.3 ms / 16 MB | 11.1 ms / 16 MB | 19.0 ms / 12 MB | 13.7 ms / 14 MB | 16.0 ms / 15 MB | 48.1 ms / 130 MB | n/a | n/a |
| users-broad-1k-high-score | 11.2 ms / 16 MB | 11.1 ms / 16 MB | 19.3 ms / 12 MB | 10.6 ms / 13 MB | 16.6 ms / 15 MB | 42.0 ms / 108 MB | n/a | n/a |
| users-broad-1k-identity | 13.5 ms / 17 MB | 13.6 ms / 16 MB | 40.0 ms / 13 MB | 16.1 ms / 13 MB | 21.8 ms / 17 MB | 62.5 ms / 107 MB | n/a | n/a |
| users-broad-1k-ids | 13.9 ms / 7.9 MB | 11.0 ms / 7.8 MB | 18.5 ms / 12 MB | 10.8 ms / 13 MB | 16.2 ms / 15 MB | 31.7 ms / 72 MB | n/a | n/a |
| users-broad-1k-keys-len | 8.99 ms / 5.9 MB | 5.85 ms / 5.9 MB | 16.5 ms / 12 MB | 14.2 ms / 13 MB | 16.1 ms / 15 MB | 32.6 ms / 72 MB | n/a | n/a |
| users-broad-1k-max-score | 11.3 ms / 14 MB | 8.46 ms / 14 MB | 16.5 ms / 12 MB | 12.9 ms / 13 MB | 15.8 ms / 15 MB | 32.3 ms / 72 MB | n/a | n/a |
| users-broad-1k-nested-dept | 5.85 ms / 5.6 MB | 5.79 ms / 5.6 MB | 13.6 ms / 12 MB | 8.47 ms / 13 MB | 13.0 ms / 14 MB | 26.3 ms / 71 MB | n/a | n/a |
| users-broad-1k-project-names | 8.48 ms / 14 MB | 8.53 ms / 14 MB | 19.1 ms / 12 MB | 11.0 ms / 13 MB | 15.7 ms / 15 MB | 31.8 ms / 72 MB | n/a | n/a |
| users-broad-1k-project-pair | 11.1 ms / 14 MB | 8.39 ms / 14 MB | 19.8 ms / 13 MB | 13.5 ms / 13 MB | 16.4 ms / 15 MB | 44.7 ms / 109 MB | n/a | n/a |
| users-broad-1k-reduce-score | 5.89 ms / 6.3 MB | 5.91 ms / 6.3 MB | 16.2 ms / 12 MB | 13.9 ms / 13 MB | 15.9 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-reverse-id | 11.2 ms / 16 MB | 11.0 ms / 16 MB | 16.3 ms / 12 MB | 16.0 ms / 13 MB | 15.8 ms / 15 MB | 39.9 ms / 111 MB | n/a | n/a |
| users-broad-1k-slice-length | 5.86 ms / 5.6 MB | 5.75 ms / 5.6 MB | 16.2 ms / 12 MB | 11.1 ms / 13 MB | 15.7 ms / 15 MB | 32.3 ms / 74 MB | n/a | n/a |
| users-broad-1k-sort-last | 11.1 ms / 15 MB | 11.3 ms / 15 MB | 18.5 ms / 12 MB | 13.2 ms / 13 MB | 16.1 ms / 15 MB | 42.5 ms / 111 MB | n/a | n/a |
| users-broad-1k-sum-score | 19.1 ms / 14 MB | 8.36 ms / 14 MB | 19.5 ms / 12 MB | 10.6 ms / 13 MB | 15.7 ms / 15 MB | n/a | n/a | n/a |
| users-broad-1k-unique-scores | 8.64 ms / 14 MB | 8.43 ms / 14 MB | 18.5 ms / 12 MB | 13.2 ms / 13 MB | 15.8 ms / 15 MB | 32.5 ms / 73 MB | n/a | n/a |
| users-broad-5k-all-nonneg | 29.0 ms / 45 MB | 26.7 ms / 46 MB | 57.8 ms / 50 MB | 32.3 ms / 51 MB | 50.0 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-any-high | 26.6 ms / 45 MB | 26.7 ms / 45 MB | 58.0 ms / 50 MB | 32.4 ms / 51 MB | 50.4 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-count | 13.6 ms / 9.8 MB | 10.9 ms / 9.8 MB | 55.4 ms / 50 MB | 27.1 ms / 51 MB | 44.9 ms / 41 MB | 100 ms / 218 MB | n/a | n/a |
| users-broad-5k-descent | 29.1 ms / 42 MB | 29.1 ms / 42 MB | 89.2 ms / 74 MB | 41.8 ms / 63 MB | 122 ms / 77 MB | 328 ms / 862 MB | n/a | n/a |
| users-broad-5k-filter-active | 33.3 ms / 56 MB | 31.6 ms / 56 MB | 58.0 ms / 50 MB | 32.7 ms / 51 MB | 47.8 ms / 41 MB | 134 ms / 330 MB | n/a | n/a |
| users-broad-5k-first-id | 13.9 ms / 9.8 MB | 10.9 ms / 9.9 MB | 55.5 ms / 50 MB | 29.8 ms / 51 MB | 47.1 ms / 41 MB | 101 ms / 217 MB | n/a | n/a |
| users-broad-5k-group-mod | 41.5 ms / 64 MB | 41.9 ms / 64 MB | 65.7 ms / 50 MB | 38.1 ms / 52 MB | 52.7 ms / 42 MB | 187 ms / 472 MB | n/a | n/a |
| users-broad-5k-high-score | 34.1 ms / 61 MB | 34.1 ms / 61 MB | 60.5 ms / 50 MB | 33.2 ms / 51 MB | 47.5 ms / 42 MB | 148 ms / 380 MB | n/a | n/a |
| users-broad-5k-identity | 46.6 ms / 66 MB | 44.2 ms / 66 MB | 145 ms / 55 MB | 50.8 ms / 51 MB | 70.3 ms / 51 MB | 267 ms / 374 MB | n/a | n/a |
| users-broad-5k-ids | 43.8 ms / 13 MB | 41.5 ms / 14 MB | 58.0 ms / 50 MB | 30.1 ms / 51 MB | 48.0 ms / 41 MB | 111 ms / 228 MB | n/a | n/a |
| users-broad-5k-keys-len | 11.1 ms / 10 MB | 11.1 ms / 10 MB | 57.4 ms / 50 MB | 30.4 ms / 51 MB | 48.2 ms / 41 MB | 105 ms / 223 MB | n/a | n/a |
| users-broad-5k-max-score | 28.9 ms / 46 MB | 26.5 ms / 46 MB | 60.3 ms / 50 MB | 30.4 ms / 51 MB | 47.3 ms / 42 MB | 116 ms / 224 MB | n/a | n/a |
| users-broad-5k-nested-dept | 11.0 ms / 9.9 MB | 11.0 ms / 9.9 MB | 49.1 ms / 50 MB | 23.8 ms / 51 MB | 41.1 ms / 41 MB | 94.3 ms / 218 MB | n/a | n/a |
| users-broad-5k-project-names | 26.5 ms / 46 MB | 26.5 ms / 46 MB | 58.1 ms / 50 MB | 29.6 ms / 51 MB | 47.4 ms / 42 MB | 110 ms / 225 MB | n/a | n/a |
| users-broad-5k-project-pair | 29.0 ms / 46 MB | 29.0 ms / 46 MB | 62.4 ms / 52 MB | 35.7 ms / 51 MB | 48.2 ms / 43 MB | 167 ms / 323 MB | n/a | n/a |
| users-broad-5k-reduce-score | 11.1 ms / 12 MB | 11.0 ms / 12 MB | 58.1 ms / 50 MB | 29.9 ms / 51 MB | 45.5 ms / 41 MB | n/a | n/a | n/a |
| users-broad-5k-reverse-id | 34.1 ms / 63 MB | 34.2 ms / 63 MB | 60.8 ms / 50 MB | 30.7 ms / 51 MB | 47.2 ms / 41 MB | 140 ms / 380 MB | n/a | n/a |
| users-broad-5k-slice-length | 11.3 ms / 9.9 MB | 11.1 ms / 9.9 MB | 55.2 ms / 50 MB | 29.5 ms / 51 MB | 45.0 ms / 41 MB | 103 ms / 238 MB | n/a | n/a |
| users-broad-5k-sort-last | 36.5 ms / 63 MB | 34.1 ms / 63 MB | 65.1 ms / 50 MB | 35.3 ms / 51 MB | 52.9 ms / 42 MB | 161 ms / 388 MB | n/a | n/a |
| users-broad-5k-sum-score | 28.9 ms / 45 MB | 26.6 ms / 46 MB | 60.5 ms / 50 MB | 29.9 ms / 51 MB | 46.9 ms / 42 MB | n/a | n/a | n/a |
| users-broad-5k-unique-scores | 28.9 ms / 46 MB | 26.5 ms / 46 MB | 60.5 ms / 50 MB | 32.6 ms / 51 MB | 52.5 ms / 42 MB | 116 ms / 221 MB | n/a | n/a |
| users-broad-25k-all-nonneg | 119 ms / 192 MB | 119 ms / 200 MB | 246 ms / 237 MB | 131 ms / 239 MB | 196 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-any-high | 107 ms / 192 MB | 109 ms / 200 MB | 231 ms / 237 MB | 106 ms / 239 MB | 178 ms / 180 MB | n/a | n/a | n/a |
| users-broad-25k-count | 44.1 ms / 31 MB | 41.6 ms / 31 MB | 241 ms / 237 MB | 109 ms / 239 MB | 185 ms / 179 MB | 467 ms / 944 MB | n/a | n/a |
| users-broad-25k-descent | 133 ms / 189 MB | 131 ms / 189 MB | 437 ms / 371 MB | 184 ms / 299 MB | 579 ms / 389 MB | 1658 ms / 4609 MB | n/a | n/a |
| users-broad-25k-filter-active | 142 ms / 227 MB | 135 ms / 215 MB | 260 ms / 237 MB | 130 ms / 239 MB | 191 ms / 182 MB | 629 ms / 1474 MB | n/a | n/a |
| users-broad-25k-first-id | 41.6 ms / 31 MB | 41.8 ms / 31 MB | 241 ms / 237 MB | 113 ms / 238 MB | 188 ms / 178 MB | 461 ms / 941 MB | n/a | n/a |
| users-broad-25k-group-mod | 174 ms / 267 MB | 174 ms / 251 MB | 267 ms / 239 MB | 139 ms / 243 MB | 195 ms / 187 MB | 855 ms / 2192 MB | n/a | n/a |
| users-broad-25k-high-score | 149 ms / 253 MB | 147 ms / 238 MB | 244 ms / 238 MB | 123 ms / 239 MB | 191 ms / 183 MB | 659 ms / 1717 MB | n/a | n/a |
| users-broad-25k-identity | 202 ms / 285 MB | 202 ms / 261 MB | 673 ms / 266 MB | 204 ms / 238 MB | 289 ms / 219 MB | excluded | n/a | n/a |
| users-broad-25k-ids | 187 ms / 39 MB | 185 ms / 36 MB | 249 ms / 237 MB | 109 ms / 239 MB | 185 ms / 183 MB | 485 ms / 988 MB | n/a | n/a |
| users-broad-25k-keys-len | 41.5 ms / 32 MB | 41.7 ms / 32 MB | 234 ms / 237 MB | 109 ms / 239 MB | 179 ms / 178 MB | 444 ms / 952 MB | n/a | n/a |
| users-broad-25k-max-score | 122 ms / 192 MB | 119 ms / 202 MB | 248 ms / 238 MB | 111 ms / 239 MB | 199 ms / 183 MB | 508 ms / 1006 MB | n/a | n/a |
| users-broad-25k-nested-dept | 41.7 ms / 31 MB | 41.6 ms / 31 MB | 229 ms / 237 MB | 99.4 ms / 238 MB | 178 ms / 178 MB | 449 ms / 940 MB | n/a | n/a |
| users-broad-25k-project-names | 117 ms / 192 MB | 114 ms / 203 MB | 254 ms / 237 MB | 118 ms / 239 MB | 187 ms / 183 MB | 488 ms / 994 MB | n/a | n/a |
| users-broad-25k-project-pair | 119 ms / 192 MB | 122 ms / 204 MB | 271 ms / 248 MB | 129 ms / 239 MB | 203 ms / 196 MB | 778 ms / 1421 MB | n/a | n/a |
| users-broad-25k-reduce-score | 36.6 ms / 38 MB | 34.3 ms / 38 MB | 246 ms / 237 MB | 113 ms / 239 MB | 190 ms / 181 MB | n/a | n/a | n/a |
| users-broad-25k-reverse-id | 144 ms / 264 MB | 147 ms / 250 MB | 241 ms / 238 MB | 111 ms / 239 MB | 183 ms / 179 MB | 635 ms / 1769 MB | n/a | n/a |
| users-broad-25k-slice-length | 31.4 ms / 31 MB | 31.7 ms / 31 MB | 234 ms / 237 MB | 108 ms / 239 MB | 181 ms / 178 MB | 484 ms / 1021 MB | n/a | n/a |
| users-broad-25k-sort-last | 169 ms / 265 MB | 162 ms / 253 MB | 276 ms / 239 MB | 139 ms / 241 MB | 206 ms / 189 MB | 693 ms / 1791 MB | n/a | n/a |
| users-broad-25k-sum-score | 124 ms / 192 MB | 117 ms / 201 MB | 250 ms / 237 MB | 115 ms / 239 MB | 186 ms / 183 MB | n/a | n/a | n/a |
| users-broad-25k-unique-scores | 122 ms / 192 MB | 119 ms / 202 MB | 251 ms / 238 MB | 109 ms / 239 MB | 198 ms / 188 MB | 500 ms / 987 MB | n/a | n/a |
| users-broad-50k-all-nonneg | 235 ms / 379 MB | 235 ms / 379 MB | 479 ms / 472 MB | 244 ms / 475 MB | 368 ms / 354 MB | n/a | n/a | n/a |
| users-broad-50k-any-high | 213 ms / 379 MB | 213 ms / 379 MB | 457 ms / 472 MB | 204 ms / 475 MB | 357 ms / 351 MB | n/a | n/a | n/a |
| users-broad-50k-count | 77.3 ms / 58 MB | 77.3 ms / 58 MB | 480 ms / 472 MB | 214 ms / 474 MB | 348 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-descent | 263 ms / 365 MB | 264 ms / 365 MB | 877 ms / 764 MB | 366 ms / 591 MB | 1179 ms / 759 MB | 3231 ms / 9180 MB | n/a | n/a |
| users-broad-50k-filter-active | 271 ms / 410 MB | 268 ms / 410 MB | 486 ms / 473 MB | 239 ms / 475 MB | 367 ms / 357 MB | 1214 ms / 2980 MB | n/a | n/a |
| users-broad-50k-first-id | 77.2 ms / 58 MB | 79.7 ms / 58 MB | 468 ms / 472 MB | 207 ms / 473 MB | 349 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-group-mod | 348 ms / 491 MB | 355 ms / 491 MB | 525 ms / 476 MB | 262 ms / 481 MB | 372 ms / 365 MB | 1711 ms / 4141 MB | n/a | n/a |
| users-broad-50k-high-score | 311 ms / 463 MB | 299 ms / 463 MB | 477 ms / 473 MB | 234 ms / 475 MB | 369 ms / 358 MB | 1283 ms / 3349 MB | n/a | n/a |
| users-broad-50k-identity | 401 ms / 499 MB | 402 ms / 499 MB | 1341 ms / 529 MB | 407 ms / 473 MB | 600 ms / 426 MB | excluded | n/a | n/a |
| users-broad-50k-ids | 368 ms / 64 MB | 368 ms / 64 MB | 469 ms / 473 MB | 209 ms / 474 MB | 364 ms / 358 MB | 960 ms / 1932 MB | n/a | n/a |
| users-broad-50k-keys-len | 77.3 ms / 59 MB | 77.3 ms / 59 MB | 455 ms / 472 MB | 202 ms / 474 MB | 359 ms / 349 MB | 872 ms / 1869 MB | n/a | n/a |
| users-broad-50k-max-score | 237 ms / 379 MB | 233 ms / 379 MB | 472 ms / 473 MB | 216 ms / 474 MB | 363 ms / 359 MB | 941 ms / 1968 MB | n/a | n/a |
| users-broad-50k-nested-dept | 77.4 ms / 58 MB | 79.9 ms / 58 MB | 457 ms / 472 MB | 203 ms / 473 MB | 350 ms / 349 MB | 864 ms / 1830 MB | n/a | n/a |
| users-broad-50k-project-names | 228 ms / 379 MB | 228 ms / 379 MB | 472 ms / 473 MB | 214 ms / 474 MB | 366 ms / 358 MB | 958 ms / 1936 MB | n/a | n/a |
| users-broad-50k-project-pair | 240 ms / 379 MB | 238 ms / 379 MB | 526 ms / 494 MB | 251 ms / 474 MB | 383 ms / 384 MB | 1556 ms / 2720 MB | n/a | n/a |
| users-broad-50k-reduce-score | 64.8 ms / 70 MB | 64.8 ms / 70 MB | 478 ms / 472 MB | 219 ms / 474 MB | 365 ms / 354 MB | n/a | n/a | n/a |
| users-broad-50k-reverse-id | 294 ms / 486 MB | 297 ms / 486 MB | 464 ms / 473 MB | 212 ms / 474 MB | 350 ms / 350 MB | 1252 ms / 3541 MB | n/a | n/a |
| users-broad-50k-slice-length | 59.5 ms / 58 MB | 61.1 ms / 58 MB | 455 ms / 472 MB | 206 ms / 474 MB | 368 ms / 349 MB | excluded | n/a | n/a |
| users-broad-50k-sort-last | 323 ms / 487 MB | 328 ms / 487 MB | 537 ms / 477 MB | 264 ms / 478 MB | 452 ms / 372 MB | 1433 ms / 3511 MB | n/a | n/a |
| users-broad-50k-sum-score | 239 ms / 378 MB | 238 ms / 378 MB | 475 ms / 473 MB | 209 ms / 474 MB | 367 ms / 359 MB | n/a | n/a | n/a |
| users-broad-50k-unique-scores | 247 ms / 379 MB | 238 ms / 379 MB | 485 ms / 473 MB | 215 ms / 474 MB | 396 ms / 369 MB | 947 ms / 1922 MB | n/a | n/a |
| users-broad-100k-all-nonneg | 455 ms / 721 MB | 451 ms / 721 MB | 954 ms / 943 MB | 475 ms / 945 MB | 748 ms / 697 MB | n/a | n/a | n/a |
| users-broad-100k-any-high | 419 ms / 721 MB | 419 ms / 721 MB | 883 ms / 943 MB | 396 ms / 945 MB | 674 ms / 692 MB | n/a | n/a | n/a |
| users-broad-100k-count | 148 ms / 112 MB | 153 ms / 112 MB | 921 ms / 943 MB | 406 ms / 943 MB | 697 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-descent | 515 ms / 703 MB | 520 ms / 703 MB | 1769 ms / 1583 MB | 743 ms / 1180 MB | 2334 ms / 1559 MB | 6542 ms / 17127 MB | n/a | n/a |
| users-broad-100k-filter-active | 562 ms / 767 MB | 532 ms / 767 MB | 968 ms / 943 MB | 465 ms / 945 MB | 718 ms / 704 MB | 2377 ms / 5756 MB | n/a | n/a |
| users-broad-100k-first-id | 151 ms / 112 MB | 155 ms / 112 MB | 909 ms / 943 MB | 434 ms / 943 MB | 684 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-group-mod | 686 ms / 913 MB | 676 ms / 913 MB | 1041 ms / 951 MB | 512 ms / 959 MB | 755 ms / 724 MB | 3489 ms / 9074 MB | n/a | n/a |
| users-broad-100k-high-score | 565 ms / 858 MB | 563 ms / 858 MB | 972 ms / 943 MB | 466 ms / 946 MB | 720 ms / 707 MB | 2633 ms / 6729 MB | n/a | n/a |
| users-broad-100k-identity | 809 ms / 977 MB | 796 ms / 977 MB | 2599 ms / 1057 MB | 781 ms / 943 MB | 1111 ms / 848 MB | 4737 ms / 6722 MB | n/a | n/a |
| users-broad-100k-ids | 735 ms / 121 MB | 727 ms / 121 MB | 935 ms / 944 MB | 402 ms / 944 MB | 718 ms / 706 MB | 1910 ms / 3837 MB | n/a | n/a |
| users-broad-100k-keys-len | 151 ms / 113 MB | 148 ms / 113 MB | 909 ms / 943 MB | 396 ms / 943 MB | 687 ms / 689 MB | 1780 ms / 3661 MB | n/a | n/a |
| users-broad-100k-max-score | 452 ms / 724 MB | 451 ms / 724 MB | 962 ms / 944 MB | 417 ms / 944 MB | 736 ms / 707 MB | 1896 ms / 3923 MB | n/a | n/a |
| users-broad-100k-nested-dept | 148 ms / 112 MB | 148 ms / 112 MB | 883 ms / 943 MB | 393 ms / 943 MB | 664 ms / 689 MB | 1723 ms / 3649 MB | n/a | n/a |
| users-broad-100k-project-names | 449 ms / 723 MB | 447 ms / 723 MB | 980 ms / 944 MB | 431 ms / 944 MB | 721 ms / 706 MB | 1869 ms / 3920 MB | n/a | n/a |
| users-broad-100k-project-pair | 466 ms / 724 MB | 466 ms / 724 MB | 1034 ms / 988 MB | 473 ms / 944 MB | 730 ms / 758 MB | 2961 ms / 5354 MB | n/a | n/a |
| users-broad-100k-reduce-score | 121 ms / 136 MB | 121 ms / 136 MB | 934 ms / 943 MB | 420 ms / 945 MB | 701 ms / 698 MB | n/a | n/a | n/a |
| users-broad-100k-reverse-id | 579 ms / 906 MB | 569 ms / 906 MB | 925 ms / 945 MB | 416 ms / 943 MB | 719 ms / 690 MB | 2481 ms / 7159 MB | n/a | n/a |
| users-broad-100k-slice-length | 113 ms / 112 MB | 118 ms / 112 MB | 902 ms / 943 MB | 413 ms / 944 MB | 733 ms / 689 MB | excluded | n/a | n/a |
| users-broad-100k-sort-last | 637 ms / 915 MB | 640 ms / 915 MB | 1104 ms / 951 MB | 519 ms / 953 MB | 836 ms / 734 MB | 2776 ms / 7047 MB | n/a | n/a |
| users-broad-100k-sum-score | 470 ms / 724 MB | 451 ms / 724 MB | 951 ms / 944 MB | 407 ms / 944 MB | 698 ms / 707 MB | n/a | n/a | n/a |
| users-broad-100k-unique-scores | 463 ms / 724 MB | 456 ms / 724 MB | 980 ms / 946 MB | 408 ms / 944 MB | 797 ms / 729 MB | 1865 ms / 3833 MB | n/a | n/a |
| users-broad-200k-all-nonneg | 969 ms / 1446 MB | 912 ms / 1446 MB | 1932 ms / 1882 MB | 960 ms / 1887 MB | 1448 ms / 1385 MB | n/a | n/a | n/a |
| users-broad-200k-any-high | 831 ms / 1446 MB | 833 ms / 1446 MB | 1926 ms / 1882 MB | 849 ms / 1887 MB | 1390 ms / 1376 MB | n/a | n/a | n/a |
| users-broad-200k-count | 290 ms / 220 MB | 290 ms / 220 MB | 1802 ms / 1882 MB | 786 ms / 1884 MB | 1334 ms / 1368 MB | excluded | n/a | n/a |
| users-broad-200k-descent | 979 ms / 1406 MB | 984 ms / 1406 MB | 3362 ms / 2923 MB | 1441 ms / 2357 MB | 4553 ms / 2866 MB | 13153 ms / 34815 MB | n/a | n/a |
| users-broad-200k-filter-active | 1048 ms / 1499 MB | 1048 ms / 1499 MB | 1986 ms / 1882 MB | 938 ms / 1889 MB | 1436 ms / 1397 MB | 4817 ms / 11907 MB | n/a | n/a |
| users-broad-200k-first-id | 300 ms / 220 MB | 290 ms / 220 MB | 1807 ms / 1882 MB | 788 ms / 1884 MB | 1336 ms / 1368 MB | 3370 ms / 7286 MB | n/a | n/a |
| users-broad-200k-group-mod | 1355 ms / 1803 MB | 1347 ms / 1803 MB | 2239 ms / 1900 MB | 1054 ms / 1913 MB | 1546 ms / 1440 MB | 6747 ms / 17439 MB | n/a | n/a |
| users-broad-200k-high-score | 1127 ms / 1710 MB | 1119 ms / 1710 MB | 1915 ms / 1884 MB | 929 ms / 1889 MB | 1445 ms / 1403 MB | 5170 ms / 13817 MB | n/a | n/a |
| users-broad-200k-identity | 1672 ms / 1934 MB | 1567 ms / 1934 MB | 5204 ms / 2110 MB | 1600 ms / 1884 MB | 2280 ms / 1688 MB | excluded | n/a | n/a |
| users-broad-200k-ids | 1457 ms / 238 MB | 1446 ms / 238 MB | 1869 ms / 1885 MB | 848 ms / 1884 MB | 1473 ms / 1403 MB | 3817 ms / 7679 MB | n/a | n/a |
| users-broad-200k-keys-len | 301 ms / 221 MB | 292 ms / 221 MB | 1776 ms / 1882 MB | 815 ms / 1884 MB | 1434 ms / 1368 MB | 3470 ms / 7266 MB | n/a | n/a |
| users-broad-200k-max-score | 951 ms / 1446 MB | 944 ms / 1446 MB | 1896 ms / 1885 MB | 867 ms / 1884 MB | 1394 ms / 1405 MB | 3701 ms / 7820 MB | n/a | n/a |
| users-broad-200k-nested-dept | 298 ms / 220 MB | 295 ms / 220 MB | 1854 ms / 1882 MB | 814 ms / 1884 MB | 1403 ms / 1368 MB | 3474 ms / 7274 MB | n/a | n/a |
| users-broad-200k-project-names | 888 ms / 1446 MB | 892 ms / 1446 MB | 1963 ms / 1885 MB | 833 ms / 1884 MB | 1403 ms / 1402 MB | 3726 ms / 7701 MB | n/a | n/a |
| users-broad-200k-project-pair | 924 ms / 1448 MB | 929 ms / 1448 MB | 2184 ms / 1972 MB | 1017 ms / 1887 MB | 1579 ms / 1506 MB | 6450 ms / 10732 MB | n/a | n/a |
| users-broad-200k-reduce-score | 242 ms / 264 MB | 240 ms / 264 MB | 1894 ms / 1882 MB | 839 ms / 1887 MB | 1413 ms / 1386 MB | n/a | n/a | n/a |
| users-broad-200k-reverse-id | 1128 ms / 1803 MB | 1130 ms / 1803 MB | 1848 ms / 1885 MB | 852 ms / 1884 MB | 1386 ms / 1372 MB | 4956 ms / 13851 MB | n/a | n/a |
| users-broad-200k-slice-length | 217 ms / 221 MB | 222 ms / 220 MB | 1780 ms / 1882 MB | 785 ms / 1884 MB | 1331 ms / 1369 MB | excluded | n/a | n/a |
| users-broad-200k-sort-last | 1278 ms / 1828 MB | 1289 ms / 1828 MB | 2396 ms / 1899 MB | 1113 ms / 1902 MB | 1815 ms / 1459 MB | 5724 ms / 14107 MB | n/a | n/a |
| users-broad-200k-sum-score | 899 ms / 1446 MB | 894 ms / 1446 MB | 1943 ms / 1885 MB | 862 ms / 1884 MB | 1459 ms / 1404 MB | n/a | n/a | n/a |
| users-broad-200k-unique-scores | 932 ms / 1446 MB | 909 ms / 1446 MB | 1994 ms / 1885 MB | 816 ms / 1884 MB | 1753 ms / 1451 MB | 3862 ms / 7680 MB | n/a | n/a |
| users-narrow-100-all-nonneg | 5.71 ms / 5.0 MB | 5.73 ms / 5.0 MB | 6.73 ms / 2.7 MB | 7.07 ms / 4.2 MB | 6.96 ms / 6.4 MB | n/a | n/a | n/a |
| users-narrow-100-any-high | 5.56 ms / 5.0 MB | 3.06 ms / 5.0 MB | 7.16 ms / 2.6 MB | 7.03 ms / 4.2 MB | 7.17 ms / 6.3 MB | n/a | n/a | n/a |
| users-narrow-100-count | 3.10 ms / 4.4 MB | 3.21 ms / 4.5 MB | 7.34 ms / 2.7 MB | 6.76 ms / 4.0 MB | 7.19 ms / 6.1 MB | 10.2 ms / 23 MB | n/a | n/a |
| users-narrow-100-descent | 5.74 ms / 4.7 MB | 5.74 ms / 4.7 MB | 5.74 ms / 2.7 MB | 5.82 ms / 4.0 MB | 5.52 ms / 6.4 MB | 8.10 ms / 34 MB | n/a | n/a |
| users-narrow-100-filter-active | 3.10 ms / 4.9 MB | 5.63 ms / 4.9 MB | 7.39 ms / 2.7 MB | 7.31 ms / 4.0 MB | 6.97 ms / 6.1 MB | 14.2 ms / 30 MB | n/a | n/a |
| users-narrow-100-first-id | 3.07 ms / 4.5 MB | 5.64 ms / 4.5 MB | 6.84 ms / 2.7 MB | 6.67 ms / 3.9 MB | 6.62 ms / 6.1 MB | 10.9 ms / 25 MB | n/a | n/a |
| users-narrow-100-group-mod | 5.66 ms / 5.0 MB | 5.72 ms / 5.0 MB | 6.81 ms / 2.7 MB | 6.73 ms / 4.2 MB | 6.94 ms / 6.5 MB | 9.74 ms / 25 MB | n/a | n/a |
| users-narrow-100-high-score | 5.73 ms / 4.9 MB | 5.66 ms / 4.9 MB | 6.78 ms / 2.7 MB | 6.98 ms / 4.0 MB | 6.80 ms / 6.0 MB | 10.7 ms / 17 MB | n/a | n/a |
| users-narrow-100-identity | 5.66 ms / 4.3 MB | 5.59 ms / 4.3 MB | 6.99 ms / 2.6 MB | 6.92 ms / 3.9 MB | 6.93 ms / 6.2 MB | 9.96 ms / 33 MB | n/a | n/a |
| users-narrow-100-ids | 5.93 ms / 4.8 MB | 3.02 ms / 4.8 MB | 7.05 ms / 2.6 MB | 4.49 ms / 3.9 MB | 6.90 ms / 6.1 MB | 10.1 ms / 25 MB | n/a | n/a |
| users-narrow-100-keys-len | 5.58 ms / 4.7 MB | 5.68 ms / 4.7 MB | 6.72 ms / 2.6 MB | 6.70 ms / 4.1 MB | 6.98 ms / 6.0 MB | 10.4 ms / 26 MB | n/a | n/a |
| users-narrow-100-max-score | 6.03 ms / 4.9 MB | 5.73 ms / 4.9 MB | 6.92 ms / 2.7 MB | 7.16 ms / 4.1 MB | 6.84 ms / 6.1 MB | 10.8 ms / 30 MB | n/a | n/a |
| users-narrow-100-nested-dept | 5.68 ms / 4.5 MB | 5.80 ms / 4.6 MB | 5.77 ms / 2.6 MB | 5.75 ms / 3.9 MB | 5.76 ms / 6.0 MB | 10.9 ms / 24 MB | n/a | n/a |
| users-narrow-100-project-names | 3.15 ms / 4.8 MB | 5.58 ms / 4.8 MB | 7.01 ms / 2.6 MB | 4.38 ms / 3.9 MB | 7.09 ms / 6.1 MB | 10.2 ms / 33 MB | n/a | n/a |
| users-narrow-100-project-pair | 5.58 ms / 5.0 MB | 5.39 ms / 5.0 MB | 6.87 ms / 2.7 MB | 6.77 ms / 3.9 MB | 6.80 ms / 6.4 MB | 10.8 ms / 36 MB | n/a | n/a |
| users-narrow-100-reduce-score | 5.59 ms / 4.9 MB | 2.89 ms / 4.9 MB | 6.76 ms / 2.6 MB | 7.12 ms / 4.0 MB | 6.85 ms / 6.0 MB | n/a | n/a | n/a |
| users-narrow-100-reverse-id | 5.61 ms / 4.8 MB | 5.66 ms / 4.8 MB | 6.98 ms / 2.7 MB | 6.77 ms / 4.0 MB | 7.18 ms / 6.1 MB | 10.9 ms / 34 MB | n/a | n/a |
| users-narrow-100-slice-length | 3.11 ms / 4.5 MB | 5.67 ms / 4.5 MB | 7.43 ms / 2.7 MB | 7.26 ms / 4.0 MB | 7.24 ms / 6.2 MB | 11.0 ms / 29 MB | n/a | n/a |
| users-narrow-100-sort-last | 5.82 ms / 4.9 MB | 5.63 ms / 4.9 MB | 6.89 ms / 2.7 MB | 7.08 ms / 4.2 MB | 6.93 ms / 6.3 MB | 9.98 ms / 26 MB | n/a | n/a |
| users-narrow-100-sum-score | 5.80 ms / 4.9 MB | 5.72 ms / 4.9 MB | 7.02 ms / 2.7 MB | 6.96 ms / 4.0 MB | 7.24 ms / 6.2 MB | n/a | n/a | n/a |
| users-narrow-100-unique-scores | 5.67 ms / 4.9 MB | 5.66 ms / 4.9 MB | 6.89 ms / 2.7 MB | 7.08 ms / 4.1 MB | 7.06 ms / 6.2 MB | 9.92 ms / 29 MB | n/a | n/a |
| users-narrow-1k-all-nonneg | 5.62 ms / 5.7 MB | 5.64 ms / 5.7 MB | 6.83 ms / 3.3 MB | 7.06 ms / 4.6 MB | 6.80 ms / 6.9 MB | n/a | n/a | n/a |
| users-narrow-1k-any-high | 5.66 ms / 5.7 MB | 5.60 ms / 5.7 MB | 7.02 ms / 3.3 MB | 7.12 ms / 4.6 MB | 6.79 ms / 6.7 MB | n/a | n/a | n/a |
| users-narrow-1k-count | 5.55 ms / 4.5 MB | 3.11 ms / 4.5 MB | 6.83 ms / 3.3 MB | 6.82 ms / 4.3 MB | 7.06 ms / 6.5 MB | 12.5 ms / 35 MB | n/a | n/a |
| users-narrow-1k-descent | 5.76 ms / 5.4 MB | 5.37 ms / 5.4 MB | 5.76 ms / 3.5 MB | 5.74 ms / 4.4 MB | 5.71 ms / 7.7 MB | 13.3 ms / 47 MB | n/a | n/a |
| users-narrow-1k-filter-active | 5.68 ms / 5.6 MB | 5.57 ms / 5.6 MB | 7.02 ms / 3.3 MB | 6.97 ms / 4.3 MB | 6.92 ms / 6.8 MB | 12.3 ms / 37 MB | n/a | n/a |
| users-narrow-1k-first-id | 5.52 ms / 4.5 MB | 5.64 ms / 4.5 MB | 7.26 ms / 3.3 MB | 6.86 ms / 4.2 MB | 6.96 ms / 6.7 MB | 13.4 ms / 35 MB | n/a | n/a |
| users-narrow-1k-group-mod | 5.78 ms / 5.9 MB | 5.67 ms / 5.9 MB | 7.12 ms / 3.6 MB | 6.84 ms / 4.7 MB | 6.75 ms / 7.1 MB | 12.8 ms / 42 MB | n/a | n/a |
| users-narrow-1k-high-score | 5.75 ms / 5.8 MB | 5.69 ms / 5.8 MB | 7.13 ms / 3.4 MB | 6.86 ms / 4.3 MB | 6.96 ms / 6.8 MB | 12.9 ms / 40 MB | n/a | n/a |
| users-narrow-1k-identity | 5.60 ms / 4.3 MB | 5.59 ms / 4.3 MB | 7.09 ms / 3.4 MB | 6.84 ms / 4.2 MB | 6.81 ms / 6.7 MB | 12.7 ms / 37 MB | n/a | n/a |
| users-narrow-1k-ids | 5.61 ms / 5.0 MB | 5.52 ms / 5.0 MB | 7.07 ms / 3.4 MB | 7.25 ms / 4.2 MB | 6.97 ms / 6.8 MB | 12.8 ms / 37 MB | n/a | n/a |
| users-narrow-1k-keys-len | 5.66 ms / 4.8 MB | 3.06 ms / 4.8 MB | 6.95 ms / 3.3 MB | 6.79 ms / 4.4 MB | 6.89 ms / 6.7 MB | 12.9 ms / 35 MB | n/a | n/a |
| users-narrow-1k-max-score | 5.67 ms / 5.6 MB | 5.60 ms / 5.6 MB | 7.12 ms / 3.4 MB | 6.94 ms / 4.4 MB | 6.89 ms / 6.7 MB | 13.2 ms / 37 MB | n/a | n/a |
| users-narrow-1k-nested-dept | 5.76 ms / 4.6 MB | 5.65 ms / 4.6 MB | 5.62 ms / 3.3 MB | 5.70 ms / 4.2 MB | 5.67 ms / 6.6 MB | 10.2 ms / 35 MB | n/a | n/a |
| users-narrow-1k-project-names | 5.66 ms / 5.5 MB | 5.74 ms / 5.5 MB | 6.85 ms / 3.3 MB | 6.80 ms / 4.2 MB | 7.07 ms / 6.9 MB | 13.0 ms / 38 MB | n/a | n/a |
| users-narrow-1k-project-pair | 5.70 ms / 5.9 MB | 5.70 ms / 5.9 MB | 7.11 ms / 3.8 MB | 6.96 ms / 4.3 MB | 6.88 ms / 7.2 MB | 22.8 ms / 64 MB | n/a | n/a |
| users-narrow-1k-reduce-score | 5.65 ms / 5.2 MB | 5.49 ms / 5.2 MB | 6.84 ms / 3.3 MB | 6.96 ms / 4.3 MB | 6.83 ms / 6.6 MB | n/a | n/a | n/a |
| users-narrow-1k-reverse-id | 5.71 ms / 5.7 MB | 5.67 ms / 5.7 MB | 6.91 ms / 3.4 MB | 7.07 ms / 4.3 MB | 6.73 ms / 6.7 MB | 12.5 ms / 37 MB | n/a | n/a |
| users-narrow-1k-slice-length | 5.77 ms / 4.6 MB | 3.23 ms / 4.5 MB | 7.23 ms / 3.3 MB | 7.00 ms / 4.3 MB | 6.99 ms / 6.6 MB | 11.0 ms / 36 MB | n/a | n/a |
| users-narrow-1k-sort-last | 5.73 ms / 5.9 MB | 5.57 ms / 5.9 MB | 6.84 ms / 3.5 MB | 7.06 ms / 4.6 MB | 6.91 ms / 7.0 MB | 12.5 ms / 38 MB | n/a | n/a |
| users-narrow-1k-sum-score | 5.67 ms / 5.6 MB | 5.69 ms / 5.6 MB | 6.83 ms / 3.4 MB | 6.77 ms / 4.3 MB | 6.86 ms / 6.8 MB | n/a | n/a | n/a |
| users-narrow-1k-unique-scores | 5.69 ms / 5.7 MB | 5.77 ms / 5.7 MB | 7.10 ms / 3.4 MB | 7.14 ms / 4.6 MB | 6.99 ms / 7.0 MB | 17.2 ms / 38 MB | n/a | n/a |
| users-narrow-5k-all-nonneg | 5.71 ms / 8.1 MB | 5.75 ms / 8.1 MB | 9.55 ms / 6.0 MB | 9.91 ms / 6.3 MB | 10.2 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-any-high | 5.62 ms / 8.1 MB | 5.39 ms / 8.1 MB | 6.97 ms / 6.0 MB | 6.97 ms / 6.3 MB | 7.35 ms / 9.6 MB | n/a | n/a | n/a |
| users-narrow-5k-count | 5.70 ms / 4.6 MB | 5.67 ms / 4.6 MB | 7.04 ms / 6.0 MB | 7.06 ms / 5.9 MB | 7.00 ms / 8.6 MB | 16.0 ms / 54 MB | n/a | n/a |
| users-narrow-5k-descent | 5.82 ms / 7.3 MB | 5.91 ms / 7.3 MB | 10.8 ms / 6.8 MB | 8.29 ms / 6.5 MB | 10.5 ms / 14 MB | 26.0 ms / 80 MB | n/a | n/a |
| users-narrow-5k-filter-active | 5.76 ms / 8.2 MB | 5.68 ms / 8.2 MB | 9.86 ms / 6.0 MB | 9.52 ms / 5.9 MB | 10.8 ms / 10 MB | 17.7 ms / 52 MB | n/a | n/a |
| users-narrow-5k-first-id | 5.70 ms / 4.6 MB | 5.84 ms / 4.6 MB | 6.92 ms / 5.9 MB | 6.87 ms / 5.8 MB | 7.05 ms / 8.6 MB | 15.6 ms / 46 MB | n/a | n/a |
| users-narrow-5k-group-mod | 8.26 ms / 10 MB | 10.7 ms / 10 MB | 12.5 ms / 6.7 MB | 10.3 ms / 7.3 MB | 9.70 ms / 11 MB | 26.1 ms / 69 MB | n/a | n/a |
| users-narrow-5k-high-score | 5.70 ms / 9.1 MB | 8.43 ms / 9.1 MB | 9.62 ms / 6.1 MB | 7.20 ms / 6.1 MB | 10.3 ms / 11 MB | 22.8 ms / 65 MB | n/a | n/a |
| users-narrow-5k-identity | 5.54 ms / 4.5 MB | 6.01 ms / 4.5 MB | 9.95 ms / 6.2 MB | 7.30 ms / 5.8 MB | 10.6 ms / 10 MB | 23.5 ms / 57 MB | n/a | n/a |
| users-narrow-5k-ids | 5.65 ms / 5.6 MB | 5.72 ms / 5.6 MB | 9.73 ms / 6.1 MB | 7.02 ms / 6.0 MB | 9.93 ms / 10 MB | 17.8 ms / 52 MB | n/a | n/a |
| users-narrow-5k-keys-len | 5.80 ms / 4.9 MB | 5.85 ms / 4.9 MB | 7.29 ms / 6.0 MB | 7.08 ms / 6.0 MB | 7.15 ms / 8.7 MB | 15.7 ms / 54 MB | n/a | n/a |
| users-narrow-5k-max-score | 5.67 ms / 8.3 MB | 5.60 ms / 8.3 MB | 10.1 ms / 6.0 MB | 7.16 ms / 6.2 MB | 10.7 ms / 10 MB | 20.8 ms / 52 MB | n/a | n/a |
| users-narrow-5k-nested-dept | 5.89 ms / 4.7 MB | 5.89 ms / 4.7 MB | 5.84 ms / 5.9 MB | 5.96 ms / 5.8 MB | 5.82 ms / 8.6 MB | 15.7 ms / 54 MB | n/a | n/a |
| users-narrow-5k-project-names | 5.65 ms / 8.2 MB | 5.82 ms / 8.2 MB | 9.65 ms / 6.0 MB | 7.88 ms / 6.0 MB | 9.38 ms / 10 MB | 19.9 ms / 54 MB | n/a | n/a |
| users-narrow-5k-project-pair | 8.20 ms / 9.4 MB | 8.29 ms / 9.3 MB | 12.2 ms / 8.3 MB | 10.8 ms / 6.0 MB | 9.96 ms / 13 MB | 73.2 ms / 117 MB | n/a | n/a |
| users-narrow-5k-reduce-score | 5.77 ms / 7.0 MB | 5.75 ms / 7.0 MB | 9.57 ms / 6.0 MB | 6.98 ms / 6.0 MB | 9.58 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-reverse-id | 5.73 ms / 9.3 MB | 5.77 ms / 9.3 MB | 9.50 ms / 6.1 MB | 7.85 ms / 5.9 MB | 6.93 ms / 8.6 MB | 18.4 ms / 53 MB | n/a | n/a |
| users-narrow-5k-slice-length | 5.63 ms / 4.7 MB | 5.74 ms / 4.6 MB | 6.85 ms / 6.0 MB | 7.00 ms / 6.0 MB | 7.25 ms / 8.7 MB | 15.8 ms / 55 MB | n/a | n/a |
| users-narrow-5k-sort-last | 5.76 ms / 9.9 MB | 7.90 ms / 9.9 MB | 12.3 ms / 6.5 MB | 7.16 ms / 6.6 MB | 12.6 ms / 11 MB | 23.1 ms / 56 MB | n/a | n/a |
| users-narrow-5k-sum-score | 5.62 ms / 8.3 MB | 5.88 ms / 8.3 MB | 9.59 ms / 6.1 MB | 6.95 ms / 6.1 MB | 7.03 ms / 10 MB | n/a | n/a | n/a |
| users-narrow-5k-unique-scores | 5.70 ms / 8.6 MB | 8.79 ms / 8.6 MB | 9.69 ms / 6.1 MB | 7.77 ms / 6.9 MB | 15.1 ms / 11 MB | 19.7 ms / 53 MB | n/a | n/a |
| users-narrow-25k-all-nonneg | 13.6 ms / 21 MB | 13.6 ms / 21 MB | 21.4 ms / 19 MB | 20.7 ms / 16 MB | 20.2 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-any-high | 10.9 ms / 21 MB | 8.56 ms / 21 MB | 15.6 ms / 19 MB | 9.97 ms / 16 MB | 12.7 ms / 20 MB | n/a | n/a | n/a |
| users-narrow-25k-count | 5.66 ms / 5.0 MB | 5.96 ms / 5.0 MB | 15.5 ms / 19 MB | 10.1 ms / 15 MB | 12.7 ms / 19 MB | 33.7 ms / 83 MB | n/a | n/a |
| users-narrow-25k-descent | 10.9 ms / 20 MB | 10.9 ms / 20 MB | 23.5 ms / 24 MB | 13.4 ms / 18 MB | 28.3 ms / 34 MB | 93.4 ms / 270 MB | n/a | n/a |
| users-narrow-25k-filter-active | 10.8 ms / 21 MB | 13.5 ms / 21 MB | 21.1 ms / 19 MB | 20.5 ms / 15 MB | 17.9 ms / 21 MB | 48.8 ms / 116 MB | n/a | n/a |
| users-narrow-25k-first-id | 5.65 ms / 5.0 MB | 8.57 ms / 5.1 MB | 15.7 ms / 19 MB | 10.5 ms / 15 MB | 13.3 ms / 19 MB | 32.9 ms / 83 MB | n/a | n/a |
| users-narrow-25k-group-mod | 28.2 ms / 27 MB | 26.1 ms / 27 MB | 41.5 ms / 21 MB | 18.5 ms / 20 MB | 23.6 ms / 25 MB | 86.5 ms / 185 MB | n/a | n/a |
| users-narrow-25k-high-score | 15.9 ms / 25 MB | 15.8 ms / 25 MB | 20.7 ms / 19 MB | 18.1 ms / 16 MB | 20.5 ms / 23 MB | 70.6 ms / 146 MB | n/a | n/a |
| users-narrow-25k-identity | 5.75 ms / 4.9 MB | 5.73 ms / 4.9 MB | 26.3 ms / 20 MB | 16.0 ms / 15 MB | 18.3 ms / 23 MB | 65.4 ms / 115 MB | n/a | n/a |
| users-narrow-25k-ids | 13.5 ms / 7.4 MB | 13.3 ms / 7.4 MB | 21.0 ms / 19 MB | 15.8 ms / 16 MB | 18.8 ms / 23 MB | 56.3 ms / 112 MB | n/a | n/a |
| users-narrow-25k-keys-len | 5.95 ms / 5.3 MB | 5.67 ms / 5.3 MB | 15.7 ms / 19 MB | 10.1 ms / 15 MB | 12.8 ms / 19 MB | 33.6 ms / 82 MB | n/a | n/a |
| users-narrow-25k-max-score | 11.2 ms / 21 MB | 10.8 ms / 21 MB | 18.8 ms / 19 MB | 15.6 ms / 16 MB | 18.1 ms / 24 MB | 54.0 ms / 125 MB | n/a | n/a |
| users-narrow-25k-nested-dept | 5.72 ms / 5.1 MB | 5.74 ms / 5.1 MB | 10.8 ms / 19 MB | 8.36 ms / 15 MB | 10.6 ms / 19 MB | 28.9 ms / 83 MB | n/a | n/a |
| users-narrow-25k-project-names | 10.8 ms / 21 MB | 11.0 ms / 21 MB | 23.0 ms / 19 MB | 15.2 ms / 16 MB | 15.1 ms / 24 MB | 63.2 ms / 134 MB | n/a | n/a |
| users-narrow-25k-project-pair | 18.6 ms / 26 MB | 18.4 ms / 26 MB | 36.2 ms / 30 MB | 28.5 ms / 16 MB | 23.5 ms / 35 MB | 322 ms / 342 MB | n/a | n/a |
| users-narrow-25k-reduce-score | 10.8 ms / 15 MB | 10.9 ms / 15 MB | 21.0 ms / 19 MB | 15.8 ms / 15 MB | 18.0 ms / 21 MB | n/a | n/a | n/a |
| users-narrow-25k-reverse-id | 10.9 ms / 25 MB | 11.0 ms / 25 MB | 20.8 ms / 19 MB | 9.95 ms / 15 MB | 13.1 ms / 19 MB | 46.4 ms / 125 MB | n/a | n/a |
| users-narrow-25k-slice-length | 5.64 ms / 5.1 MB | 5.92 ms / 5.1 MB | 15.6 ms / 19 MB | 10.6 ms / 15 MB | 13.2 ms / 19 MB | 35.3 ms / 89 MB | n/a | n/a |
| users-narrow-25k-sort-last | 16.3 ms / 26 MB | 13.3 ms / 26 MB | 41.4 ms / 21 MB | 19.2 ms / 17 MB | 36.0 ms / 29 MB | 91.5 ms / 132 MB | n/a | n/a |
| users-narrow-25k-sum-score | 13.2 ms / 21 MB | 10.8 ms / 21 MB | 20.7 ms / 19 MB | 14.9 ms / 16 MB | 15.4 ms / 23 MB | n/a | n/a | n/a |
| users-narrow-25k-unique-scores | 13.5 ms / 21 MB | 13.3 ms / 21 MB | 32.1 ms / 20 MB | 12.6 ms / 16 MB | 30.8 ms / 25 MB | 52.2 ms / 119 MB | n/a | n/a |
| users-narrow-50k-all-nonneg | 21.2 ms / 23 MB | 21.1 ms / 23 MB | 36.6 ms / 35 MB | 37.0 ms / 27 MB | 33.8 ms / 35 MB | n/a | n/a | n/a |
| users-narrow-50k-any-high | 13.5 ms / 23 MB | 13.5 ms / 23 MB | 23.8 ms / 35 MB | 15.6 ms / 27 MB | 21.4 ms / 32 MB | n/a | n/a | n/a |
| users-narrow-50k-count | 5.70 ms / 5.6 MB | 8.49 ms / 5.6 MB | 24.3 ms / 35 MB | 15.7 ms / 26 MB | 20.4 ms / 30 MB | 56.1 ms / 134 MB | n/a | n/a |
| users-narrow-50k-descent | 15.9 ms / 23 MB | 16.1 ms / 23 MB | 43.8 ms / 47 MB | 21.0 ms / 33 MB | 52.2 ms / 60 MB | 186 ms / 470 MB | n/a | n/a |
| users-narrow-50k-filter-active | 18.3 ms / 23 MB | 18.6 ms / 23 MB | 36.3 ms / 35 MB | 36.5 ms / 26 MB | 29.1 ms / 34 MB | 89.0 ms / 211 MB | n/a | n/a |
| users-narrow-50k-first-id | 8.43 ms / 5.6 MB | 8.25 ms / 5.7 MB | 23.7 ms / 35 MB | 15.4 ms / 26 MB | 20.6 ms / 30 MB | 57.5 ms / 125 MB | n/a | n/a |
| users-narrow-50k-group-mod | 48.8 ms / 32 MB | 48.8 ms / 32 MB | 84.4 ms / 39 MB | 34.1 ms / 34 MB | 44.5 ms / 48 MB | 166 ms / 310 MB | n/a | n/a |
| users-narrow-50k-high-score | 23.9 ms / 27 MB | 23.9 ms / 27 MB | 39.0 ms / 36 MB | 29.2 ms / 28 MB | 31.4 ms / 39 MB | 131 ms / 266 MB | n/a | n/a |
| users-narrow-50k-identity | 6.05 ms / 5.5 MB | 5.84 ms / 5.5 MB | 46.8 ms / 38 MB | 23.7 ms / 26 MB | 28.4 ms / 39 MB | 116 ms / 197 MB | n/a | n/a |
| users-narrow-50k-ids | 21.2 ms / 9.6 MB | 21.3 ms / 9.7 MB | 33.5 ms / 36 MB | 18.2 ms / 27 MB | 23.1 ms / 39 MB | 98.9 ms / 203 MB | n/a | n/a |
| users-narrow-50k-keys-len | 8.43 ms / 5.9 MB | 8.34 ms / 5.9 MB | 23.8 ms / 35 MB | 15.2 ms / 26 MB | 20.5 ms / 31 MB | 56.7 ms / 134 MB | n/a | n/a |
| users-narrow-50k-max-score | 18.6 ms / 24 MB | 15.9 ms / 24 MB | 35.0 ms / 36 MB | 26.5 ms / 27 MB | 26.7 ms / 40 MB | 95.7 ms / 214 MB | n/a | n/a |
| users-narrow-50k-nested-dept | 8.24 ms / 5.7 MB | 8.31 ms / 5.7 MB | 20.9 ms / 35 MB | 13.2 ms / 26 MB | 18.1 ms / 31 MB | 53.0 ms / 122 MB | n/a | n/a |
| users-narrow-50k-project-names | 16.1 ms / 24 MB | 16.1 ms / 23 MB | 34.0 ms / 36 MB | 23.7 ms / 27 MB | 26.0 ms / 39 MB | 105 ms / 231 MB | n/a | n/a |
| users-narrow-50k-project-pair | 31.3 ms / 29 MB | 31.3 ms / 29 MB | 63.6 ms / 58 MB | 54.2 ms / 28 MB | 41.0 ms / 60 MB | 668 ms / 631 MB | n/a | n/a |
| users-narrow-50k-reduce-score | 16.1 ms / 18 MB | 16.2 ms / 18 MB | 33.7 ms / 35 MB | 29.1 ms / 27 MB | 28.9 ms / 35 MB | n/a | n/a | n/a |
| users-narrow-50k-reverse-id | 18.7 ms / 28 MB | 15.8 ms / 28 MB | 34.2 ms / 36 MB | 16.0 ms / 26 MB | 20.9 ms / 31 MB | 82.4 ms / 226 MB | n/a | n/a |
| users-narrow-50k-slice-length | 5.81 ms / 5.7 MB | 8.42 ms / 5.7 MB | 26.7 ms / 35 MB | 16.5 ms / 26 MB | 24.1 ms / 31 MB | 60.4 ms / 142 MB | n/a | n/a |
| users-narrow-50k-sort-last | 23.6 ms / 33 MB | 23.7 ms / 33 MB | 87.0 ms / 40 MB | 37.0 ms / 31 MB | 65.0 ms / 50 MB | 189 ms / 252 MB | n/a | n/a |
| users-narrow-50k-sum-score | 15.7 ms / 24 MB | 16.0 ms / 23 MB | 39.1 ms / 36 MB | 24.3 ms / 27 MB | 26.3 ms / 39 MB | n/a | n/a | n/a |
| users-narrow-50k-unique-scores | 18.6 ms / 24 MB | 18.5 ms / 24 MB | 53.9 ms / 36 MB | 23.9 ms / 28 MB | 56.2 ms / 46 MB | 93.1 ms / 201 MB | n/a | n/a |
| users-narrow-100k-all-nonneg | 38.1 ms / 40 MB | 36.4 ms / 40 MB | 64.6 ms / 70 MB | 64.3 ms / 52 MB | 57.0 ms / 61 MB | n/a | n/a | n/a |
| users-narrow-100k-any-high | 21.3 ms / 40 MB | 21.1 ms / 40 MB | 41.8 ms / 70 MB | 24.2 ms / 52 MB | 34.2 ms / 57 MB | n/a | n/a | n/a |
| users-narrow-100k-count | 11.0 ms / 6.8 MB | 10.7 ms / 6.8 MB | 41.7 ms / 70 MB | 23.7 ms / 50 MB | 31.6 ms / 53 MB | 99.0 ms / 215 MB | n/a | n/a |
| users-narrow-100k-descent | 28.8 ms / 40 MB | 28.8 ms / 40 MB | 78.9 ms / 89 MB | 33.7 ms / 64 MB | 95.9 ms / 112 MB | 349 ms / 970 MB | n/a | n/a |
| users-narrow-100k-filter-active | 31.3 ms / 40 MB | 31.2 ms / 40 MB | 64.3 ms / 70 MB | 66.9 ms / 50 MB | 48.9 ms / 63 MB | 161 ms / 332 MB | n/a | n/a |
| users-narrow-100k-first-id | 11.0 ms / 6.8 MB | 10.8 ms / 6.8 MB | 41.6 ms / 70 MB | 23.9 ms / 50 MB | 33.9 ms / 53 MB | 98.7 ms / 212 MB | n/a | n/a |
| users-narrow-100k-group-mod | 99.0 ms / 72 MB | 91.5 ms / 72 MB | 161 ms / 79 MB | 67.0 ms / 66 MB | 78.4 ms / 79 MB | 337 ms / 592 MB | n/a | n/a |
| users-narrow-100k-high-score | 44.1 ms / 59 MB | 43.8 ms / 59 MB | 70.0 ms / 70 MB | 56.5 ms / 53 MB | 56.3 ms / 70 MB | 267 ms / 444 MB | n/a | n/a |
| users-narrow-100k-identity | 8.35 ms / 6.7 MB | 8.23 ms / 6.7 MB | 84.0 ms / 76 MB | 44.1 ms / 50 MB | 49.0 ms / 71 MB | 231 ms / 341 MB | n/a | n/a |
| users-narrow-100k-ids | 36.1 ms / 14 MB | 36.1 ms / 14 MB | 59.3 ms / 72 MB | 31.7 ms / 53 MB | 41.7 ms / 71 MB | 179 ms / 343 MB | n/a | n/a |
| users-narrow-100k-keys-len | 11.1 ms / 7.1 MB | 11.0 ms / 7.1 MB | 44.2 ms / 70 MB | 24.4 ms / 50 MB | 35.5 ms / 54 MB | 99.0 ms / 220 MB | n/a | n/a |
| users-narrow-100k-max-score | 31.4 ms / 43 MB | 28.9 ms / 43 MB | 59.6 ms / 72 MB | 44.6 ms / 53 MB | 48.9 ms / 73 MB | 187 ms / 377 MB | n/a | n/a |
| users-narrow-100k-nested-dept | 10.9 ms / 6.9 MB | 10.8 ms / 6.9 MB | 36.4 ms / 70 MB | 21.2 ms / 50 MB | 28.5 ms / 54 MB | 94.0 ms / 217 MB | n/a | n/a |
| users-narrow-100k-project-names | 26.2 ms / 40 MB | 26.3 ms / 40 MB | 59.3 ms / 72 MB | 41.8 ms / 53 MB | 41.8 ms / 72 MB | 193 ms / 392 MB | n/a | n/a |
| users-narrow-100k-project-pair | 56.0 ms / 62 MB | 53.9 ms / 62 MB | 120 ms / 116 MB | 94.6 ms / 52 MB | 67.4 ms / 118 MB | 1301 ms / 1229 MB | n/a | n/a |
| users-narrow-100k-reduce-score | 28.6 ms / 30 MB | 28.7 ms / 30 MB | 61.9 ms / 70 MB | 48.9 ms / 51 MB | 46.5 ms / 64 MB | n/a | n/a | n/a |
| users-narrow-100k-reverse-id | 28.8 ms / 62 MB | 28.8 ms / 62 MB | 72.1 ms / 72 MB | 24.2 ms / 50 MB | 36.7 ms / 56 MB | 155 ms / 402 MB | n/a | n/a |
| users-narrow-100k-slice-length | 8.27 ms / 6.9 MB | 8.23 ms / 6.9 MB | 41.6 ms / 70 MB | 23.7 ms / 50 MB | 31.5 ms / 54 MB | 105 ms / 229 MB | n/a | n/a |
| users-narrow-100k-sort-last | 43.8 ms / 74 MB | 41.3 ms / 74 MB | 167 ms / 79 MB | 61.8 ms / 59 MB | 117 ms / 88 MB | 363 ms / 437 MB | n/a | n/a |
| users-narrow-100k-sum-score | 28.8 ms / 43 MB | 28.8 ms / 43 MB | 71.4 ms / 72 MB | 39.6 ms / 53 MB | 44.0 ms / 73 MB | n/a | n/a | n/a |
| users-narrow-100k-unique-scores | 36.4 ms / 45 MB | 33.8 ms / 45 MB | 104 ms / 73 MB | 39.7 ms / 54 MB | 97.8 ms / 74 MB | 174 ms / 319 MB | n/a | n/a |
| users-narrow-200k-all-nonneg | 66.5 ms / 78 MB | 66.7 ms / 78 MB | 115 ms / 137 MB | 117 ms / 98 MB | 99.9 ms / 117 MB | n/a | n/a | n/a |
| users-narrow-200k-any-high | 38.8 ms / 78 MB | 36.4 ms / 78 MB | 74.5 ms / 137 MB | 42.0 ms / 98 MB | 59.0 ms / 108 MB | n/a | n/a | n/a |
| users-narrow-200k-count | 15.7 ms / 9.3 MB | 13.7 ms / 9.3 MB | 72.0 ms / 137 MB | 39.5 ms / 94 MB | 57.5 ms / 100 MB | 184 ms / 428 MB | n/a | n/a |
| users-narrow-200k-descent | 54.1 ms / 78 MB | 53.7 ms / 78 MB | 159 ms / 182 MB | 64.1 ms / 122 MB | 186 ms / 221 MB | 688 ms / 1817 MB | n/a | n/a |
| users-narrow-200k-filter-active | 56.5 ms / 78 MB | 56.5 ms / 78 MB | 120 ms / 137 MB | 125 ms / 94 MB | 87.1 ms / 116 MB | 328 ms / 643 MB | n/a | n/a |
| users-narrow-200k-first-id | 18.3 ms / 9.3 MB | 18.6 ms / 9.3 MB | 80.0 ms / 137 MB | 42.2 ms / 94 MB | 59.3 ms / 100 MB | 192 ms / 429 MB | n/a | n/a |
| users-narrow-200k-group-mod | 182 ms / 117 MB | 179 ms / 117 MB | 320 ms / 154 MB | 115 ms / 130 MB | 143 ms / 150 MB | 619 ms / 1049 MB | n/a | n/a |
| users-narrow-200k-high-score | 81.3 ms / 111 MB | 81.4 ms / 111 MB | 125 ms / 139 MB | 97.0 ms / 99 MB | 97.3 ms / 135 MB | 473 ms / 873 MB | n/a | n/a |
| users-narrow-200k-identity | 11.1 ms / 9.2 MB | 10.9 ms / 9.2 MB | 173 ms / 149 MB | 72.3 ms / 94 MB | 82.2 ms / 134 MB | 430 ms / 596 MB | n/a | n/a |
| users-narrow-200k-ids | 66.2 ms / 23 MB | 66.2 ms / 23 MB | 117 ms / 140 MB | 59.7 ms / 98 MB | 76.7 ms / 136 MB | 360 ms / 667 MB | n/a | n/a |
| users-narrow-200k-keys-len | 18.7 ms / 9.6 MB | 18.4 ms / 9.6 MB | 74.9 ms / 137 MB | 39.7 ms / 94 MB | 56.9 ms / 100 MB | 183 ms / 404 MB | n/a | n/a |
| users-narrow-200k-max-score | 53.8 ms / 78 MB | 51.5 ms / 78 MB | 105 ms / 140 MB | 79.9 ms / 98 MB | 79.9 ms / 136 MB | 347 ms / 760 MB | n/a | n/a |
| users-narrow-200k-nested-dept | 18.5 ms / 9.4 MB | 18.6 ms / 9.4 MB | 69.1 ms / 137 MB | 36.5 ms / 94 MB | 53.7 ms / 100 MB | 177 ms / 417 MB | n/a | n/a |
| users-narrow-200k-project-names | 46.4 ms / 78 MB | 46.4 ms / 78 MB | 107 ms / 140 MB | 81.6 ms / 97 MB | 76.4 ms / 136 MB | 396 ms / 739 MB | n/a | n/a |
| users-narrow-200k-project-pair | 106 ms / 116 MB | 107 ms / 116 MB | 238 ms / 227 MB | 180 ms / 99 MB | 122 ms / 227 MB | 2558 ms / 2512 MB | n/a | n/a |
| users-narrow-200k-reduce-score | 51.5 ms / 53 MB | 51.5 ms / 53 MB | 112 ms / 137 MB | 91.4 ms / 97 MB | 86.9 ms / 119 MB | n/a | n/a | n/a |
| users-narrow-200k-reverse-id | 51.6 ms / 116 MB | 51.4 ms / 116 MB | 127 ms / 140 MB | 39.7 ms / 94 MB | 57.1 ms / 103 MB | 280 ms / 780 MB | n/a | n/a |
| users-narrow-200k-slice-length | 13.5 ms / 9.4 MB | 10.9 ms / 9.4 MB | 79.1 ms / 137 MB | 41.8 ms / 95 MB | 59.4 ms / 100 MB | 199 ms / 448 MB | n/a | n/a |
| users-narrow-200k-sort-last | 83.9 ms / 131 MB | 81.5 ms / 131 MB | 394 ms / 153 MB | 120 ms / 113 MB | 253 ms / 174 MB | 805 ms / 817 MB | n/a | n/a |
| users-narrow-200k-sum-score | 51.2 ms / 78 MB | 51.5 ms / 78 MB | 123 ms / 140 MB | 67.4 ms / 98 MB | 77.1 ms / 136 MB | n/a | n/a | n/a |
| users-narrow-200k-unique-scores | 66.4 ms / 78 MB | 64.1 ms / 78 MB | 190 ms / 140 MB | 64.9 ms / 98 MB | 191 ms / 158 MB | 315 ms / 702 MB | n/a | n/a |
| yaml-broad-100-count | 18.9 ms / 5.9 MB | 14.3 ms / 6.0 MB | n/a | 52.5 ms / 5.6 MB | 54.0 ms / 11 MB | 57.5 ms / 30 MB | 56.9 ms / 16 MB | n/a |
| yaml-broad-100-descent | 18.8 ms / 10 MB | 16.6 ms / 10 MB | n/a | 15.8 ms / 5.8 MB | 21.2 ms / 13 MB | 25.5 ms / 38 MB | n/a | n/a |
| yaml-broad-100-first-id | 17.4 ms / 5.7 MB | 10.8 ms / 5.7 MB | n/a | 52.3 ms / 5.5 MB | 54.6 ms / 11 MB | 55.3 ms / 26 MB | 55.9 ms / 16 MB | n/a |
| yaml-broad-100-identity | 15.9 ms / 10 MB | 13.6 ms / 10 MB | n/a | 52.9 ms / 5.5 MB | 56.4 ms / 10 MB | 60.5 ms / 34 MB | 60.0 ms / 17 MB | n/a |
| yaml-broad-100-ids | 12.8 ms / 6.1 MB | 15.6 ms / 6.1 MB | n/a | 52.6 ms / 5.5 MB | 54.8 ms / 11 MB | 57.8 ms / 26 MB | n/a | n/a |
| yaml-broad-100-nested-dept | 15.4 ms / 5.7 MB | 15.6 ms / 5.7 MB | n/a | 15.2 ms / 5.5 MB | 17.1 ms / 11 MB | 22.3 ms / 24 MB | 25.5 ms / 16 MB | n/a |
| yaml-broad-1k-count | 26.3 ms / 14 MB | 25.1 ms / 13 MB | n/a | 66.5 ms / 22 MB | 98.1 ms / 35 MB | 91.3 ms / 66 MB | 104 ms / 59 MB | n/a |
| yaml-broad-1k-descent | 41.3 ms / 55 MB | 41.3 ms / 55 MB | n/a | 34.1 ms / 24 MB | 74.4 ms / 52 MB | 97.0 ms / 208 MB | n/a | n/a |
| yaml-broad-1k-first-id | 27.5 ms / 12 MB | 26.5 ms / 12 MB | n/a | 67.8 ms / 22 MB | 95.0 ms / 35 MB | 89.7 ms / 66 MB | 100 ms / 57 MB | n/a |
| yaml-broad-1k-identity | 42.6 ms / 55 MB | 41.4 ms / 55 MB | n/a | 73.4 ms / 22 MB | 103 ms / 38 MB | 120 ms / 101 MB | 123 ms / 68 MB | n/a |
| yaml-broad-1k-ids | 28.2 ms / 14 MB | 25.4 ms / 14 MB | n/a | 68.2 ms / 22 MB | 97.4 ms / 36 MB | 93.0 ms / 68 MB | n/a | n/a |
| yaml-broad-1k-nested-dept | 25.7 ms / 12 MB | 26.3 ms / 12 MB | n/a | 30.5 ms / 22 MB | 58.9 ms / 36 MB | 51.3 ms / 66 MB | 63.9 ms / 56 MB | n/a |
| yaml-broad-5k-count | 76.1 ms / 48 MB | 73.4 ms / 48 MB | n/a | 140 ms / 96 MB | 274 ms / 149 MB | 230 ms / 259 MB | 291 ms / 234 MB | n/a |
| yaml-broad-5k-descent | 144 ms / 233 MB | 144 ms / 233 MB | n/a | 117 ms / 109 MB | 314 ms / 207 MB | 423 ms / 956 MB | n/a | n/a |
| yaml-broad-5k-first-id | 78.3 ms / 41 MB | 76.7 ms / 41 MB | n/a | 142 ms / 96 MB | 273 ms / 147 MB | 226 ms / 259 MB | 292 ms / 237 MB | n/a |
| yaml-broad-5k-identity | 141 ms / 233 MB | 140 ms / 233 MB | n/a | 158 ms / 96 MB | 296 ms / 156 MB | 377 ms / 428 MB | 387 ms / 283 MB | n/a |
| yaml-broad-5k-ids | 81.3 ms / 48 MB | 76.3 ms / 48 MB | n/a | 138 ms / 96 MB | 284 ms / 145 MB | 232 ms / 268 MB | n/a | n/a |
| yaml-broad-5k-nested-dept | 76.6 ms / 41 MB | 76.6 ms / 41 MB | n/a | 102 ms / 96 MB | 236 ms / 148 MB | 185 ms / 259 MB | 249 ms / 235 MB | n/a |
| yaml-broad-25k-count | 326 ms / 229 MB | 313 ms / 229 MB | n/a | 486 ms / 464 MB | 1149 ms / 730 MB | 898 ms / 1224 MB | 1212 ms / 1112 MB | n/a |
| yaml-broad-25k-descent | 671 ms / 1078 MB | 670 ms / 1078 MB | n/a | 531 ms / 521 MB | 1497 ms / 1081 MB | 2043 ms / 4667 MB | n/a | n/a |
| yaml-broad-25k-first-id | 333 ms / 225 MB | 323 ms / 225 MB | n/a | 498 ms / 464 MB | 1152 ms / 694 MB | 897 ms / 1222 MB | 1216 ms / 1109 MB | n/a |
| yaml-broad-25k-identity | 691 ms / 1078 MB | 648 ms / 1078 MB | n/a | 581 ms / 464 MB | 1242 ms / 764 MB | 1640 ms / 2095 MB | 1729 ms / 1329 MB | n/a |
| yaml-broad-25k-ids | 359 ms / 242 MB | 335 ms / 242 MB | n/a | 484 ms / 464 MB | 1138 ms / 727 MB | 932 ms / 1270 MB | n/a | n/a |
| yaml-broad-25k-nested-dept | 335 ms / 225 MB | 337 ms / 225 MB | n/a | 466 ms / 464 MB | 1092 ms / 709 MB | 860 ms / 1223 MB | 1172 ms / 1110 MB | n/a |
| yaml-broad-50k-count | 626 ms / 455 MB | 608 ms / 455 MB | n/a | 927 ms / 924 MB | 2222 ms / 1491 MB | 1700 ms / 2412 MB | 2353 ms / 2258 MB | n/a |
| yaml-broad-50k-descent | 1325 ms / 2073 MB | 1317 ms / 2073 MB | n/a | 1049 ms / 1030 MB | 2980 ms / 2301 MB | 3971 ms / 9447 MB | n/a | n/a |
| yaml-broad-50k-first-id | 639 ms / 446 MB | 633 ms / 447 MB | n/a | 922 ms / 924 MB | 2228 ms / 1472 MB | 1734 ms / 2426 MB | 2376 ms / 2306 MB | n/a |
| yaml-broad-50k-identity | 1303 ms / 2069 MB | 1272 ms / 2069 MB | n/a | 1097 ms / 924 MB | 2418 ms / 1580 MB | 3231 ms / 4310 MB | 3344 ms / 2812 MB | n/a |
| yaml-broad-50k-ids | 695 ms / 449 MB | 673 ms / 449 MB | n/a | 923 ms / 924 MB | 2243 ms / 1484 MB | 1785 ms / 2510 MB | n/a | n/a |
| yaml-broad-50k-nested-dept | 633 ms / 447 MB | 631 ms / 447 MB | n/a | 884 ms / 924 MB | 2164 ms / 1469 MB | 1658 ms / 2425 MB | 2322 ms / 2262 MB | n/a |
| yaml-broad-100k-count | 1210 ms / 890 MB | 1212 ms / 890 MB | n/a | 1789 ms / 1845 MB | 4411 ms / 2701 MB | 3342 ms / 4807 MB | 4671 ms / 4474 MB | n/a |
| yaml-broad-100k-descent | 2608 ms / 4090 MB | 2729 ms / 4090 MB | n/a | 2107 ms / 2067 MB | 6138 ms / 4656 MB | 7680 ms / 19123 MB | n/a | n/a |
| yaml-broad-100k-first-id | 1272 ms / 874 MB | 1247 ms / 874 MB | n/a | 1783 ms / 1845 MB | 4385 ms / 2696 MB | 3365 ms / 4805 MB | 4701 ms / 4411 MB | n/a |
| yaml-broad-100k-identity | 2587 ms / 4103 MB | 2597 ms / 4103 MB | n/a | 2167 ms / 1845 MB | 4767 ms / 3137 MB | 6352 ms / 9066 MB | 6574 ms / 5508 MB | n/a |
| yaml-broad-100k-ids | 1418 ms / 896 MB | 1393 ms / 896 MB | n/a | 1797 ms / 1845 MB | 4412 ms / 2810 MB | 3469 ms / 5027 MB | n/a | n/a |
| yaml-broad-100k-nested-dept | 1253 ms / 874 MB | 1250 ms / 874 MB | n/a | 1749 ms / 1845 MB | 4322 ms / 2895 MB | 3313 ms / 4814 MB | 4666 ms / 4545 MB | n/a |
| yaml-narrow-100-count | 16.0 ms / 4.9 MB | 12.0 ms / 4.9 MB | n/a | 45.9 ms / 4.1 MB | 45.6 ms / 6.2 MB | 49.0 ms / 24 MB | 47.2 ms / 10.0 MB | n/a |
| yaml-narrow-100-descent | 11.1 ms / 5.3 MB | 10.6 ms / 5.3 MB | n/a | 11.6 ms / 4.1 MB | 10.9 ms / 6.6 MB | 14.5 ms / 18 MB | n/a | n/a |
| yaml-narrow-100-first-id | 15.7 ms / 4.8 MB | 11.0 ms / 4.8 MB | n/a | 47.5 ms / 4.0 MB | 44.3 ms / 6.3 MB | 48.7 ms / 17 MB | 51.1 ms / 9.9 MB | n/a |
| yaml-narrow-100-identity | 13.5 ms / 5.1 MB | 10.8 ms / 5.1 MB | n/a | 48.4 ms / 4.0 MB | 46.1 ms / 6.4 MB | 49.7 ms / 22 MB | 52.0 ms / 9.9 MB | n/a |
| yaml-narrow-100-ids | 14.0 ms / 5.2 MB | 19.4 ms / 5.2 MB | n/a | 50.0 ms / 4.0 MB | 46.8 ms / 6.4 MB | 52.0 ms / 18 MB | n/a | n/a |
| yaml-narrow-100-nested-dept | 11.2 ms / 4.8 MB | 12.0 ms / 4.8 MB | n/a | 12.3 ms / 4.0 MB | 11.5 ms / 6.1 MB | 13.6 ms / 16 MB | error | n/a |
| yaml-narrow-1k-count | 12.4 ms / 5.4 MB | 11.4 ms / 5.4 MB | n/a | 55.7 ms / 4.6 MB | 50.3 ms / 7.7 MB | 52.9 ms / 20 MB | 52.6 ms / 13 MB | n/a |
| yaml-narrow-1k-descent | 11.1 ms / 7.2 MB | 11.0 ms / 7.2 MB | n/a | 10.5 ms / 4.7 MB | 13.4 ms / 8.9 MB | 19.9 ms / 30 MB | n/a | n/a |
| yaml-narrow-1k-first-id | 13.1 ms / 5.2 MB | 12.1 ms / 5.2 MB | n/a | 51.3 ms / 4.5 MB | 52.0 ms / 7.8 MB | 52.8 ms / 20 MB | 50.9 ms / 13 MB | n/a |
| yaml-narrow-1k-identity | 12.7 ms / 7.0 MB | 11.7 ms / 7.0 MB | n/a | 52.1 ms / 4.5 MB | 52.2 ms / 8.1 MB | 57.0 ms / 29 MB | 57.0 ms / 15 MB | n/a |
| yaml-narrow-1k-ids | 12.6 ms / 6.5 MB | 12.4 ms / 6.5 MB | n/a | 48.7 ms / 4.5 MB | 50.3 ms / 8.0 MB | 55.9 ms / 29 MB | n/a | n/a |
| yaml-narrow-1k-nested-dept | 11.5 ms / 5.2 MB | 12.3 ms / 5.2 MB | n/a | 11.3 ms / 4.5 MB | 11.5 ms / 7.8 MB | 15.3 ms / 24 MB | error | n/a |
| yaml-narrow-5k-count | 15.3 ms / 8.1 MB | 15.5 ms / 8.0 MB | n/a | 54.3 ms / 7.5 MB | 56.8 ms / 15 MB | 58.2 ms / 33 MB | 61.9 ms / 23 MB | n/a |
| yaml-narrow-5k-descent | 17.5 ms / 16 MB | 17.3 ms / 16 MB | n/a | 16.8 ms / 8.0 MB | 25.0 ms / 18 MB | 35.9 ms / 66 MB | n/a | n/a |
| yaml-narrow-5k-first-id | 15.1 ms / 6.7 MB | 15.5 ms / 6.7 MB | n/a | 53.7 ms / 7.4 MB | 57.8 ms / 15 MB | 60.1 ms / 32 MB | 62.4 ms / 23 MB | n/a |
| yaml-narrow-5k-identity | 19.0 ms / 15 MB | 17.2 ms / 15 MB | n/a | 60.5 ms / 7.3 MB | 60.0 ms / 15 MB | 66.8 ms / 45 MB | 66.7 ms / 24 MB | n/a |
| yaml-narrow-5k-ids | 19.2 ms / 12 MB | 17.6 ms / 12 MB | n/a | 54.3 ms / 7.6 MB | 56.9 ms / 15 MB | 67.2 ms / 45 MB | n/a | n/a |
| yaml-narrow-5k-nested-dept | 14.5 ms / 6.7 MB | 14.2 ms / 6.7 MB | n/a | 15.0 ms / 7.3 MB | 19.3 ms / 15 MB | 22.8 ms / 36 MB | error | n/a |
| yaml-narrow-25k-count | 28.6 ms / 19 MB | 28.1 ms / 19 MB | n/a | 68.8 ms / 22 MB | 94.2 ms / 42 MB | 90.6 ms / 79 MB | 107 ms / 69 MB | n/a |
| yaml-narrow-25k-descent | 43.7 ms / 58 MB | 43.3 ms / 58 MB | n/a | 36.4 ms / 26 MB | 76.2 ms / 61 MB | 114 ms / 254 MB | n/a | n/a |
| yaml-narrow-25k-first-id | 26.6 ms / 13 MB | 29.1 ms / 13 MB | n/a | 68.3 ms / 22 MB | 95.8 ms / 42 MB | 94.0 ms / 79 MB | 103 ms / 70 MB | n/a |
| yaml-narrow-25k-identity | 42.6 ms / 58 MB | 41.6 ms / 58 MB | n/a | 74.9 ms / 22 MB | 99.1 ms / 47 MB | 121 ms / 113 MB | 129 ms / 86 MB | n/a |
| yaml-narrow-25k-ids | 47.4 ms / 38 MB | 46.6 ms / 38 MB | n/a | 70.7 ms / 23 MB | 97.6 ms / 46 MB | 113 ms / 113 MB | n/a | n/a |
| yaml-narrow-25k-nested-dept | 25.8 ms / 13 MB | 25.4 ms / 13 MB | n/a | 30.9 ms / 22 MB | 58.7 ms / 42 MB | 53.3 ms / 79 MB | error | n/a |
| yaml-narrow-50k-count | 45.0 ms / 33 MB | 44.5 ms / 33 MB | n/a | 86.5 ms / 41 MB | 142 ms / 78 MB | 128 ms / 141 MB | 152 ms / 117 MB | n/a |
| yaml-narrow-50k-descent | 74.4 ms / 110 MB | 76.6 ms / 110 MB | n/a | 59.1 ms / 48 MB | 143 ms / 116 MB | 211 ms / 502 MB | n/a | n/a |
| yaml-narrow-50k-first-id | 43.1 ms / 22 MB | 42.0 ms / 22 MB | n/a | 88.3 ms / 41 MB | 140 ms / 79 MB | 125 ms / 140 MB | 155 ms / 123 MB | n/a |
| yaml-narrow-50k-identity | 73.8 ms / 110 MB | 68.9 ms / 110 MB | n/a | 97.8 ms / 41 MB | 146 ms / 87 MB | 187 ms / 202 MB | 207 ms / 141 MB | n/a |
| yaml-narrow-50k-ids | 90.2 ms / 71 MB | 89.3 ms / 71 MB | n/a | 92.7 ms / 43 MB | 143 ms / 87 MB | 168 ms / 212 MB | n/a | n/a |
| yaml-narrow-50k-nested-dept | 42.0 ms / 22 MB | 41.5 ms / 22 MB | n/a | 51.9 ms / 41 MB | 109 ms / 79 MB | 91.0 ms / 140 MB | error | n/a |
| yaml-narrow-100k-count | 72.9 ms / 56 MB | 74.2 ms / 56 MB | n/a | 126 ms / 78 MB | 226 ms / 153 MB | 199 ms / 265 MB | 250 ms / 220 MB | n/a |
| yaml-narrow-100k-descent | 132 ms / 184 MB | 129 ms / 183 MB | n/a | 104 ms / 91 MB | 269 ms / 251 MB | 394 ms / 1037 MB | n/a | n/a |
| yaml-narrow-100k-first-id | 69.6 ms / 37 MB | 69.0 ms / 37 MB | n/a | 127 ms / 78 MB | 226 ms / 152 MB | 196 ms / 265 MB | 251 ms / 219 MB | n/a |
| yaml-narrow-100k-identity | 124 ms / 184 MB | 122 ms / 183 MB | n/a | 148 ms / 78 MB | 245 ms / 163 MB | 320 ms / 426 MB | 363 ms / 280 MB | n/a |
| yaml-narrow-100k-ids | 226 ms / 128 MB | 222 ms / 128 MB | n/a | 140 ms / 81 MB | 244 ms / 166 MB | 282 ms / 447 MB | n/a | n/a |
| yaml-narrow-100k-nested-dept | 69.3 ms / 37 MB | 66.7 ms / 37 MB | n/a | 91.9 ms / 78 MB | 197 ms / 152 MB | 161 ms / 264 MB | error | n/a |

## known disagreements

gojq: gojq writes object keys in sorted order; compact bytes differ, JSON values match

users-broad-100-identity, users-broad-1k-identity, users-broad-5k-identity, users-broad-25k-identity, users-broad-50k-identity, users-broad-100k-identity, users-broad-200k-identity

## disagreements

none.
