window.BENCHMARK_DATA = {
  "lastUpdate": 1773758546573,
  "repoUrl": "https://github.com/Irys-xyz/irys",
  "entries": {
    "Benchmark": [
      {
        "commit": {
          "author": {
            "name": "Irys-xyz",
            "username": "Irys-xyz"
          },
          "committer": {
            "name": "Irys-xyz",
            "username": "Irys-xyz"
          },
          "id": "025f3d3528973219203347a073defbe51fb41d4d",
          "message": "feat(bench): add criterion benchmarks and CI workflow",
          "timestamp": "2026-03-17T10:06:48Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1221/commits/025f3d3528973219203347a073defbe51fb41d4d"
        },
        "date": 1773758545741,
        "tool": "cargo",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6066317,
            "range": "± 52432",
            "unit": "ns/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 866317834,
            "range": "± 981602",
            "unit": "ns/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1127892107,
            "range": "± 722987",
            "unit": "ns/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8402113,
            "range": "± 38045",
            "unit": "ns/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1202745022,
            "range": "± 1994886",
            "unit": "ns/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1566508712,
            "range": "± 2219351",
            "unit": "ns/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 3059370,
            "range": "± 97039",
            "unit": "ns/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 254244833,
            "range": "± 2387154",
            "unit": "ns/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 332566694,
            "range": "± 3573867",
            "unit": "ns/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 112,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}