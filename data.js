window.BENCHMARK_DATA = {
  "lastUpdate": 1774018212211,
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
        "date": 1773766099197,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.766564,
            "range": "± 0.281229",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 903.514025,
            "range": "± 38.193127",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1187.09686,
            "range": "± 60.903382",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 10.908284,
            "range": "± 0.716149",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1444.150136,
            "range": "± 130.078283",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1623.91766,
            "range": "± 16.01811",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.964489,
            "range": "± 0.329429",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 262.44671,
            "range": "± 4.180433",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 337.312387,
            "range": "± 11.642313",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000122,
            "range": "± 1.2e-05",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "57174310+glottologist@users.noreply.github.com",
            "name": "Jason Ridgway-Taylor (~misfur-mondut)",
            "username": "glottologist"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f4a9481197abc876013593d1a136c67c3c5b2421",
          "message": "feat(bench): add criterion benchmarks and CI workflow (#1221)\n\n* feat(vdf): add criterion benchmarks and CI workflow\n\n* fix(bench): narrow CI permissions, use real network configs, and refine triggers\n\n* fix(bench): use on sync\n\n* fix(bench): removed branch-ahead check\n\n* fix(bench): use build cache\n\n* fix(bench): use --bench '*' to skip lib harness targets\n\n* fix(bench): move sccache setup before repo setup\n\n* fix(bench): scope concurrency group by event name\n\n* fix(bench): use config-driven checkpoint count and add black_box\n\n* feat(bench): add branch cleanup on merge and PR results comment\n\n* fix(bench): convert benchmark output from ns to ms and remove sccache",
          "timestamp": "2026-03-17T16:32:55Z",
          "tree_id": "aaa430e071c6206311b092785a975be9b053dfaf",
          "url": "https://github.com/Irys-xyz/irys/commit/f4a9481197abc876013593d1a136c67c3c5b2421"
        },
        "date": 1773766360214,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.899993,
            "range": "± 0.441056",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 922.574564,
            "range": "± 91.90081",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1182.268264,
            "range": "± 17.8226",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.625207,
            "range": "± 0.401466",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1233.669644,
            "range": "± 28.418838",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1783.359336,
            "range": "± 103.536187",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.573015,
            "range": "± 0.187648",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 268.843555,
            "range": "± 14.216676",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 338.035649,
            "range": "± 4.017774",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "57174310+glottologist@users.noreply.github.com",
            "name": "Jason Ridgway-Taylor (~misfur-mondut)",
            "username": "glottologist"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f4a9481197abc876013593d1a136c67c3c5b2421",
          "message": "feat(bench): add criterion benchmarks and CI workflow (#1221)\n\n* feat(vdf): add criterion benchmarks and CI workflow\n\n* fix(bench): narrow CI permissions, use real network configs, and refine triggers\n\n* fix(bench): use on sync\n\n* fix(bench): removed branch-ahead check\n\n* fix(bench): use build cache\n\n* fix(bench): use --bench '*' to skip lib harness targets\n\n* fix(bench): move sccache setup before repo setup\n\n* fix(bench): scope concurrency group by event name\n\n* fix(bench): use config-driven checkpoint count and add black_box\n\n* feat(bench): add branch cleanup on merge and PR results comment\n\n* fix(bench): convert benchmark output from ns to ms and remove sccache",
          "timestamp": "2026-03-17T16:32:55Z",
          "tree_id": "aaa430e071c6206311b092785a975be9b053dfaf",
          "url": "https://github.com/Irys-xyz/irys/commit/f4a9481197abc876013593d1a136c67c3c5b2421"
        },
        "date": 1773864467418,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.981767,
            "range": "± 0.077752",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 861.228185,
            "range": "± 8.892024",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1108.365669,
            "range": "± 4.851987",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.268759,
            "range": "± 0.010698",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1184.765407,
            "range": "± 1.895635",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1537.861266,
            "range": "± 1.246916",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.925694,
            "range": "± 0.196608",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 261.335552,
            "range": "± 1.803647",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 337.143378,
            "range": "± 1.234687",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000112,
            "range": "± 0",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "jesse.cruz.wright@gmail.com",
            "name": "JesseTheRobot",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "jesse.cruz.wright@gmail.com",
            "name": "JesseTheRobot",
            "username": "JesseTheRobot"
          },
          "distinct": true,
          "id": "bda098549bf281e9f1f073e9533aaa2442fab5eb",
          "message": "feat(ci): add sccache to Flaky Test Detection workflow\n\nAdd RUSTC_WRAPPER, SCCACHE_DIR, and SCCACHE_CACHE_SIZE env vars.\nAdd sccache stats steps for observability.",
          "timestamp": "2026-03-18T19:00:24Z",
          "tree_id": "088dd778948a52e2e7492424289e377396873868",
          "url": "https://github.com/Irys-xyz/irys/commit/bda098549bf281e9f1f073e9533aaa2442fab5eb"
        },
        "date": 1773866239485,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.075392,
            "range": "± 0.012774",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 870.425291,
            "range": "± 1.943549",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1128.957503,
            "range": "± 2.178386",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.462529,
            "range": "± 0.062059",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1211.053784,
            "range": "± 7.817699",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 2012.236043,
            "range": "± 108.643818",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 4.058105,
            "range": "± 2.117001",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 315.016687,
            "range": "± 47.643159",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 432.809238,
            "range": "± 12.619939",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0.000001",
            "unit": "ms/iter"
          }
        ]
      },
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
        "date": 1773915710154,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.138232,
            "range": "± 0.039527",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 876.836027,
            "range": "± 0.9829",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1141.476226,
            "range": "± 2.603574",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.331144,
            "range": "± 0.385097",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1189.039477,
            "range": "± 1.383516",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1544.71444,
            "range": "± 8.676065",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 3.020837,
            "range": "± 0.188745",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 256.757206,
            "range": "± 3.422023",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 328.421511,
            "range": "± 1.192362",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "33699735+roberts-pumpurs@users.noreply.github.com",
            "name": "Roberts Pumpurs",
            "username": "roberts-pumpurs"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e8f7fa95fb8457cf54368f73c7e762035a13d3d2",
          "message": "fix: exclude confirmed txs from submit selection (#1224)\n\n* fix: exclude confirmed txs from submit selection\n* test: cover confirmed tx submit selector filter\n* test: cover stale-parent submit reselection",
          "timestamp": "2026-03-19T12:27:57+02:00",
          "tree_id": "a995af23ae097c5395458805813215dc5b7c8fc6",
          "url": "https://github.com/Irys-xyz/irys/commit/e8f7fa95fb8457cf54368f73c7e762035a13d3d2"
        },
        "date": 1773916928311,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.625551,
            "range": "± 0.300586",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 941.812956,
            "range": "± 56.988982",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1233.057475,
            "range": "± 58.757317",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.43895,
            "range": "± 0.032136",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1214.43487,
            "range": "± 9.51716",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1569.600245,
            "range": "± 3.242468",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.760438,
            "range": "± 0.262506",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 258.144187,
            "range": "± 2.049901",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 338.884672,
            "range": "± 1.004824",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0",
            "unit": "ms/iter"
          }
        ]
      },
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
          "id": "0c17972c6012e33f8ad2d96948901e89eae83e20",
          "message": "ci(design): extract design documents",
          "timestamp": "2026-03-19T10:28:02Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1212/commits/0c17972c6012e33f8ad2d96948901e89eae83e20"
        },
        "date": 1773922247169,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.059128,
            "range": "± 0.019523",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 867.384874,
            "range": "± 1.70469",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1128.32164,
            "range": "± 1.357848",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.430377,
            "range": "± 0.044302",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1203.750797,
            "range": "± 3.279177",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1562.617527,
            "range": "± 6.947972",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 3.100673,
            "range": "± 0.069763",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 257.331688,
            "range": "± 3.666844",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 337.32895,
            "range": "± 1.059484",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "57174310+glottologist@users.noreply.github.com",
            "name": "Jason Ridgway-Taylor (~misfur-mondut)",
            "username": "glottologist"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "579ab4f3a1b4fdd42c91b051b4805e5f5f4d626e",
          "message": "perf(vdf): optimize hot loop with direct compress256 and copy elimination (#1214)\n\n* perf(vdf): replace Sha256 API with direct compress256 in hot loop\n\n* perf(vdf): eliminate per-iteration 32-byte copy in hot loop\n\n* refactor(vdf): review comments\n\n* refactor(vdf): review comments\n\n* refactor(vdf): review comments\n\n* chore(ci): restrict benchmark workflow to PR creation, approval, and merge\n\n* fix(bench): update vdf_bench to match rebased vdf_sha signature\n\n* fix(vdf): add debug_assert for checkpoint length in vdf_sha\n\n* fix(ci): handle workflow_dispatch branch detection in bench workflow\n\n* refactor(vdf): address review findings for vdf optimisation PR\n\n* refactor(chain-tests): remove unused vdf imports\n\n* refactor(vdf): extract compress_n_rounds and remove redundant comments\n\n* fix(vdf): qualify size_of/align_of for Rust 2024 edition\n\n* feat(ci): seed branch benchmarks with master baseline",
          "timestamp": "2026-03-19T12:40:23Z",
          "tree_id": "35c939437bd43b0029d59fdc7341f5326a0955cf",
          "url": "https://github.com/Irys-xyz/irys/commit/579ab4f3a1b4fdd42c91b051b4805e5f5f4d626e"
        },
        "date": 1773925103164,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.220263,
            "range": "± 0.062429",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 747.80027,
            "range": "± 9.404192",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 968.635104,
            "range": "± 0.594919",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.209644,
            "range": "± 0.024465",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1173.655031,
            "range": "± 2.437966",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1622.183005,
            "range": "± 57.780566",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.252172,
            "range": "± 0.236265",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 220.399245,
            "range": "± 4.749725",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 280.291259,
            "range": "± 6.137287",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000146,
            "range": "± 0.000014",
            "unit": "ms/iter"
          }
        ]
      },
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
          "id": "783934c5bd180bde5a7afcd3cbf357e287cf2e5e",
          "message": "feat: run mode",
          "timestamp": "2026-03-19T12:40:29Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1228/commits/783934c5bd180bde5a7afcd3cbf357e287cf2e5e"
        },
        "date": 1773925797556,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 6.17325,
            "range": "± 0.135563",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 926.222535,
            "range": "± 34.878227",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1171.37626,
            "range": "± 19.589766",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.304686,
            "range": "± 0.075143",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1191.613605,
            "range": "± 5.274282",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1546.789488,
            "range": "± 44.048674",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 4.762137,
            "range": "± 2.350555",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 422.449799,
            "range": "± 23.391674",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 373.416479,
            "range": "± 45.379866",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000152,
            "range": "± 0.000008",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "jesse.cruz.wright@gmail.com",
            "name": "Jesse Cruz Wright",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "418c728d2cab08670dc2b613bfa867c7bc5e1db8",
          "message": "feat: run mode (#1228)\n\n* docs: add design spec and implementation plan for RunMode flag\n\nDesign spec covers replacing all 6 cfg!(debug_assertions) runtime checks\nwith an explicit RunMode enum on NodeConfig plus granular per-behavior\nconfig parameters (DbSyncMode, CorePinning, Reth cache/validation\nsettings). Implementation plan details the bottom-up execution strategy.\n\n* feat: add RunMode, DbSyncMode, CorePinning, DatabaseConfig types\n\nAdd explicit configuration types to replace cfg!(debug_assertions)\nruntime checks. RunMode enum (Production/Test) on NodeConfig,\nDbSyncMode enum for MDBX sync settings, CorePinning enum for VDF\nthread pinning, and DatabaseConfig sub-struct grouping DB sync modes.\nNew fields on NodeConfig, RethConfig, VdfNodeConfig with serde defaults\nfor backward compatibility.\n\n* feat: thread DbSyncMode through database functions\n\nReplace cfg!(debug_assertions) with explicit DbSyncMode parameter in\nopen_or_create_db and all DB wrapper functions. Sync mode is only\napplied when args is None; custom DatabaseArguments take precedence.\nAdds DatabaseArgs extension trait and db_sync_mode_to_mdbx helper.\n\n* feat: update all callers with explicit DbSyncMode\n\nPass DbSyncMode through all database call sites across domain, actors,\np2p, storage, and debug-utils crates. Test callers use UtterlyNoSync,\nproduction paths use Durable or pull from config.\n\n* feat: replace cfg!(debug_assertions) with config-driven settings\n\nUse config fields instead of cfg!(debug_assertions) for DB sync mode,\nVDF core pinning, Reth DB sync mode, cache size, and validation task\ncount. Separate debug-build warning from run_mode startup warning.\n\n* fix: keep temp dir alive\n\n* feat: unify SyncMode\n\n* fix: address feedback\n\n* chore: fmt\n\n* feat: update plan\n\n* feat: add design docs\n\n* feat: address feedback\n\n* docs: update docs",
          "timestamp": "2026-03-19T17:01:54Z",
          "tree_id": "e197955d404a5c605dcf252598996199b566dd07",
          "url": "https://github.com/Irys-xyz/irys/commit/418c728d2cab08670dc2b613bfa867c7bc5e1db8"
        },
        "date": 1773940734102,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.230659,
            "range": "± 0.031898",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 747.35484,
            "range": "± 6.129658",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 968.739387,
            "range": "± 8.515346",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.213504,
            "range": "± 0.021492",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1174.743845,
            "range": "± 10.460994",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1530.643651,
            "range": "± 26.52108",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.021956,
            "range": "± 0.23007",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 224.855491,
            "range": "± 26.746164",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 286.879271,
            "range": "± 5.646848",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.000001",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "jesse.cruz.wright@gmail.com",
            "name": "Jesse Cruz Wright",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fc8485b8a3438c30cfc1fb63218f98b5366b9d0c",
          "message": "feat: check database schema version on startup (#1223)\n\n* feat: add TempDirBuilder and migrate all crates to unified temp dirs\n\nIntroduces TempDirBuilder in irys-testing-utils as the single entry\npoint for creating test temporary directories. All test temp dirs now\nroute through .tmp/ (or IRYS_CUSTOM_TMP_DIR) for consistent cleanup\nand discoverability.\n\nMigrates all crates (database, actors, domain, p2p, chain-tests) from\nthe deprecated temporary_directory() / setup_tracing_and_temp_dir()\nhelpers and raw tempfile::tempdir() calls to TempDirBuilder. Removes\nthe deprecated functions and the direct tempfile dev-dependency from\ncrates that no longer need it.\n\n* feat: add database schema versioning and migration checks\n\nAdds startup database version validation (ensure_db_version_compatible)\nthat runs before any services initialize. Handles four cases: fresh DB\n(stamps current version), legacy DB without version (panics with\nmigration guidance), newer DB than binary (rejects to prevent rollback\ncorruption), and older DB (runs forward migrations then stamps).\n\nIntroduces DatabaseVersion enum and centralized version definitions in\ncrates/types/src/versions.rs, consolidating protocol, P2P, and database\nversion constants. Refactors ProtocolVersion to use fallible conversion\nfor safer version negotiation in P2P handshakes.\n\n* docs: add design decision records\n\nAdds ADRs for the three changes in this branch:\n- Database schema versioning and migration strategy\n- Centralized version enums in irys-types\n- Test temporary directory builder pattern\n\n* feat: switch migration code to a loop\n\n* docs: update design docs\n\n* feat: add legacy tx migration test\n\n* docs: update docs",
          "timestamp": "2026-03-20T14:33:59Z",
          "tree_id": "2839e1a65201df0db993e02a3cbb6b23eea3a64d",
          "url": "https://github.com/Irys-xyz/irys/commit/fc8485b8a3438c30cfc1fb63218f98b5366b9d0c"
        },
        "date": 1774018211187,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.224487,
            "range": "± 0.0541",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 746.90309,
            "range": "± 7.853519",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 973.818783,
            "range": "± 47.32704",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 10.761276,
            "range": "± 0.330886",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1244.779161,
            "range": "± 90.746641",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1609.689046,
            "range": "± 96.597553",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.220934,
            "range": "± 0.224203",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 213.432625,
            "range": "± 2.255061",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 278.620485,
            "range": "± 1.96714",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000112,
            "range": "± 0",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}