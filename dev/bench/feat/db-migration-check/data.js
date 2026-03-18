window.BENCHMARK_DATA = {
  "lastUpdate": 1773830177126,
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
          "id": "41e2da0f6bfd4f022cffa925c3b4bb9bf07c0a33",
          "message": "feat: check database schema version on startup",
          "timestamp": "2026-03-17T16:33:12Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1223/commits/41e2da0f6bfd4f022cffa925c3b4bb9bf07c0a33"
        },
        "date": 1773830176269,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.053067,
            "range": "± 0.018142",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 917.16997,
            "range": "± 36.576731",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1293.581841,
            "range": "± 30.242962",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 10.394252,
            "range": "± 0.166683",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1299.470148,
            "range": "± 75.686552",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1652.000987,
            "range": "± 31.930281",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 3.103636,
            "range": "± 0.045326",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 258.650524,
            "range": "± 2.648687",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 333.941836,
            "range": "± 5.09685",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0.0",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}