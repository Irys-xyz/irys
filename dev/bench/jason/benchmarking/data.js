window.BENCHMARK_DATA = {
  "lastUpdate": 1773766285785,
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
          "id": "adac154903d1003ea32c40ef4fc355df1fbb30f0",
          "message": "feat(bench): add criterion benchmarks and CI workflow",
          "timestamp": "2026-03-17T10:06:48Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1221/commits/adac154903d1003ea32c40ef4fc355df1fbb30f0"
        },
        "date": 1773766284670,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.881068,
            "range": "± 0.154658",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 931.524326,
            "range": "± 81.496398",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1165.667946,
            "range": "± 8.781691",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.329001,
            "range": "± 0.171306",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1214.133205,
            "range": "± 29.701316",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1552.597018,
            "range": "± 78.381288",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.547947,
            "range": "± 0.499251",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 303.24163,
            "range": "± 38.853363",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 345.972961,
            "range": "± 20.418818",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.0",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}