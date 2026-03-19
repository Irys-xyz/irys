window.BENCHMARK_DATA = {
  "lastUpdate": 1773916976471,
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
          "id": "c91fe26c6c6d772c636df1fec297b92bdb98dd4f",
          "message": "fix: exclude confirmed txs from submit selection",
          "timestamp": "2026-03-18T19:54:11Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1224/commits/c91fe26c6c6d772c636df1fec297b92bdb98dd4f"
        },
        "date": 1773916975470,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.385851,
            "range": "± 0.174062",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 939.265809,
            "range": "± 45.10945",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1146.697965,
            "range": "± 59.837367",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.470459,
            "range": "± 0.100499",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1208.011296,
            "range": "± 10.302241",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1571.383911,
            "range": "± 13.574918",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 3.034275,
            "range": "± 0.161202",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 264.320118,
            "range": "± 2.841402",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 337.033003,
            "range": "± 3.918453",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 1e-06",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}