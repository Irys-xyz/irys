window.BENCHMARK_DATA = {
  "lastUpdate": 1773782200857,
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
          "id": "8e7cd6fdd24e568f310aedbb53495eb6bf3e3eb8",
          "message": "feat: Cascade hardfork — term ledgers (OneYear/ThirtyDay) and height-aware pricing",
          "timestamp": "2026-03-17T16:33:12Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1166/commits/8e7cd6fdd24e568f310aedbb53495eb6bf3e3eb8"
        },
        "date": 1773782199942,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.941008,
            "range": "± 0.067922",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 854.18241,
            "range": "± 7.439916",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1105.040214,
            "range": "± 1.117784",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.278082,
            "range": "± 0.013057",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1183.234747,
            "range": "± 2.416101",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1537.438627,
            "range": "± 1.949546",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.993642,
            "range": "± 0.180884",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 256.536788,
            "range": "± 1.941894",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 331.039499,
            "range": "± 2.436576",
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