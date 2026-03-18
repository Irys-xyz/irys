window.BENCHMARK_DATA = {
  "lastUpdate": 1773825476875,
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
          "id": "5f67e679a0f845cd09265ed2c141a3a0de14a00e",
          "message": "feat(multiversion-tests): add cross-version integration test harness",
          "timestamp": "2026-03-17T16:33:12Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1207/commits/5f67e679a0f845cd09265ed2c141a3a0de14a00e"
        },
        "date": 1773825476024,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.019852,
            "range": "± 0.181046",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 854.782656,
            "range": "± 14.005952",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1104.960466,
            "range": "± 28.931602",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 10.107234,
            "range": "± 0.638986",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1351.926189,
            "range": "± 149.304042",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1597.727194,
            "range": "± 175.67841",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.572491,
            "range": "± 0.105352",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 259.062403,
            "range": "± 4.899693",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 331.657483,
            "range": "± 2.096136",
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