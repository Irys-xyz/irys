window.BENCHMARK_DATA = {
  "lastUpdate": 1773918039930,
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
          "id": "0823bc9a3f0d8cbef5a28ae28ca514a71e3adcab",
          "message": "test: add additional tests",
          "timestamp": "2026-03-19T10:28:02Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1206/commits/0823bc9a3f0d8cbef5a28ae28ca514a71e3adcab"
        },
        "date": 1773918039063,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.960921,
            "range": "± 0.051793",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 912.388414,
            "range": "± 36.331006",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1306.531813,
            "range": "± 70.515254",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.774204,
            "range": "± 0.274421",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1280.437675,
            "range": "± 64.260286",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1620.738204,
            "range": "± 147.175579",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.632575,
            "range": "± 0.1784",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 257.658902,
            "range": "± 2.947085",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 343.295139,
            "range": "± 17.076574",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00011,
            "range": "± 0.0",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}