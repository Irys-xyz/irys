window.BENCHMARK_DATA = {
  "lastUpdate": 1773831949401,
  "repoUrl": "https://github.com/Irys-xyz/irys",
  "entries": {
    "Benchmark": [
      {
        "commit": {
          "author": {
            "name": "jason",
            "username": "glottologist",
            "email": "jason@ridgway-taylor.co.uk"
          },
          "committer": {
            "name": "jason",
            "username": "glottologist",
            "email": "jason@ridgway-taylor.co.uk"
          },
          "id": "464e7e19e7a0d3019f2810931a0d99a9dac8a6f7",
          "message": "refactor(chain-tests): remove unused vdf imports",
          "timestamp": "2026-03-18T10:49:00Z",
          "url": "https://github.com/Irys-xyz/irys/commit/464e7e19e7a0d3019f2810931a0d99a9dac8a6f7"
        },
        "date": 1773831948592,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.216976,
            "range": "± 0.014345",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 744.8221,
            "range": "± 0.705502",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 967.946836,
            "range": "± 2.964381",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.69224,
            "range": "± 0.31818",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1270.037904,
            "range": "± 87.730392",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1949.6298,
            "range": "± 15.903349",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 3.59233,
            "range": "± 1.066738",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 231.303423,
            "range": "± 12.959101",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 299.3839,
            "range": "± 36.573041",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00012,
            "range": "± 1.7e-05",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}