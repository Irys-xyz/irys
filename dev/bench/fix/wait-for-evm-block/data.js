window.BENCHMARK_DATA = {
  "lastUpdate": 1775658798001,
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
      },
      {
        "commit": {
          "author": {
            "email": "samuraidan@gmail.com",
            "name": "DMac",
            "username": "DanMacDonald"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "9725fe915cee133f296b7d51feefd9d706d3e31e",
          "message": "docs: enrich version enum doc comments (#1237)\n\ndocs: enrich version enum doc comments with operator-facing descriptions\n\nAdd higher-level \"why it matters\" context to each version variant's doc\ncomments, complementing the existing implementation-focused bullet points.\nDescriptions sourced from the release dashboard glossary.\n\nCo-authored-by: Claude Opus 4.6 <noreply@anthropic.com>",
          "timestamp": "2026-03-20T11:52:42-07:00",
          "tree_id": "c08aecc6a426047916df76ad7433e384239848f4",
          "url": "https://github.com/Irys-xyz/irys/commit/9725fe915cee133f296b7d51feefd9d706d3e31e"
        },
        "date": 1774033778480,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.212718,
            "range": "± 0.025195",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 745.960184,
            "range": "± 3.284064",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 967.885322,
            "range": "± 1.08108",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.358182,
            "range": "± 0.02301",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1197.735492,
            "range": "± 2.838824",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1556.796505,
            "range": "± 3.836688",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.54714,
            "range": "± 0.211408",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.659296,
            "range": "± 1.724996",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.894918,
            "range": "± 1.201053",
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
          "id": "36469cb638f3ad10ffa8ce84b6f830f370487c43",
          "message": "fix: avoid block_discovery panic after block validation rejection (#1235)\n\n* fix: avoid block_discovery panic after pd base fee shadow tx rejection\n\nBug: heavy_test_block_with_incorrect_pd_base_fee_gets_rejected exposed a\nrace where a bad parent block was correctly rejected for a\nPdBaseFeeUpdate shadow transaction mismatch, but a descendant was still\nprocessed afterward.\n\nError: block_discovery panicked with \"Parent block ... should be in the\nblock tree!\" after the rejected parent had already been removed, which\ncascaded into service shutdown and SendError failures in the test\nharness.`\n\nFix: return PreviousBlockNotFound instead of panicking when the parent\ndisappears during block discovery, and make block_tree invalid-result\ncleanup idempotent when a descendant was already removed as part of\nancestor cleanup.\n\n* flake fix\n\n* comments",
          "timestamp": "2026-03-23T11:29:27+01:00",
          "tree_id": "417df053c0eb008d4d212ca6e7aeb387c3817f5e",
          "url": "https://github.com/Irys-xyz/irys/commit/36469cb638f3ad10ffa8ce84b6f830f370487c43"
        },
        "date": 1774262814177,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.217196,
            "range": "± 0.059421",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 745.313489,
            "range": "± 2.113712",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 968.211271,
            "range": "± 0.842672",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.79342,
            "range": "± 0.355164",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1175.858936,
            "range": "± 8.011496",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1534.270665,
            "range": "± 5.655554",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.587442,
            "range": "± 0.065812",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.836515,
            "range": "± 2.957458",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 294.251308,
            "range": "± 76.665624",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0.000003",
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
          "id": "6044eccc5efe41a7cdc0cfb3507733bcfe2fcdaa",
          "message": "fix(ci): replace envsubst with node for prompt templating (#1238)\n\n* fix(ci): replace envsubst with node for prompt templating\n\n* refactor(ci): review comments",
          "timestamp": "2026-03-23T10:53:25Z",
          "tree_id": "f9f5407b44664bb0fc7f5d63c763c9ac9656659a",
          "url": "https://github.com/Irys-xyz/irys/commit/6044eccc5efe41a7cdc0cfb3507733bcfe2fcdaa"
        },
        "date": 1774264206311,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.224011,
            "range": "± 0.074086",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 746.624781,
            "range": "± 5.322816",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 968.510226,
            "range": "± 3.048523",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.403524,
            "range": "± 0.078877",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1200.86557,
            "range": "± 8.960017",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1558.234064,
            "range": "± 14.364306",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.13232,
            "range": "± 0.085495",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.736862,
            "range": "± 3.571809",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.547763,
            "range": "± 1.846593",
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
          "id": "0ca706c6b69ac4193a981f1b26b75652fa554a7d",
          "message": "feat: perm ledger expiry for testnet (#1201)\n\n* feat: add publish_ledger_epoch_length to EpochConfig\n* feat: add validation for publish_ledger_epoch_length\n* feat: store publish_ledger_epoch_length on Ledgers\n* feat: implement perm slot expiry in Ledgers::expire_partitions()\n\nRename expire_term_partitions() to expire_partitions() and add publish\nledger expiry logic. When publish_ledger_epoch_length is configured,\nperm slots whose last_height is older than (epoch_height - epoch_length\n* num_blocks_in_epoch) are expired, with the last slot always protected.\nIncludes 4 unit tests covering disabled, enabled, last-slot-protection,\nand not-enough-blocks scenarios.\n* feat: include perm slots in get_expiring_partitions() read-only path\nRename get_expiring_term_partitions() to get_expiring_partitions() and\nadd perm ledger slot expiry logic that mirrors expire_partitions() but\nwithout mutation. This ensures the read-only path used by EpochSnapshot\nreports the same expiring partitions as the mutating path.\n* refactor: rename expire_term_* methods to expire_* (now handles perm too)\n\nUpdate callers in epoch_snapshot.rs to use the renamed methods:\n- expire_term_ledger_slots() -> expire_ledger_slots()\n- expire_term_partitions() -> expire_partitions()\n- get_expiring_term_partitions() -> get_expiring_partitions()\n\n* fix: filter expired slots from PermanentLedger::get_slot_needs()\n\nAdd !slot.is_expired check to PermanentLedger::get_slot_needs(),\nmatching the existing behavior in TermLedger::get_slot_needs().\nThis prevents expired permanent ledger slots from being offered\nfor new partition assignments.\n\nIncludes a test that verifies expired slots are excluded.\n\n* docs: update bail comment in collect_expired_partitions for perm expiry\n\nClarify that the DataLedger::Publish bail prevents accidental fee\ncalculation, not that publish ledger cannot expire at all.\n\n* fix: add publish_ledger_epoch_length to EpochConfig constructors in tests\n\n* test: add integration test for publish ledger expiry\n\n* style: fix formatting in collect_expired_partitions bail message\n* fix: filter by ledger type before partition lookup in collect_expired_partitions\n\nPrevents a Publish partition state inconsistency from blocking Submit fee\ndistribution. Previously, get_assignment() was called for ALL expired\npartitions before the ledger type check — a missing Publish partition would\nbail the entire function.\n\nFixes security review Finding 1 (Medium-High).\n\n* fix: add debug_assert preventing Publish fee distribution\n\nMoves the unreachable bail guard from collect_expired_partitions to a\ndebug_assert at the calculate_expired_ledger_fees entry point. This\ncatches misuse during development without runtime overhead in release.\n\nFixes security review Finding 4 (Low).\n\n* fix: use checked_mul for expiry height arithmetic\n\nAll 3 locations computing epoch_length * num_blocks_in_epoch now use\nchecked_mul with a descriptive panic. This prevents silent overflow\nin release builds with extreme config values.\n\nApplied to TermLedger::get_expired_slot_indexes, Ledgers::expire_partitions,\nand Ledgers::get_expiring_partitions for consistency.\n\nFixes security review Finding 3 (Low).\n\n* test: add expiry state assertions to perm_ledger_expiry integration test\n\nVerifies:\n- Perm slots are marked is_expired after expiry height\n- No TermFeeReward shadow txs in the expiry epoch block\n- Expired partitions are returned to capacity pool",
          "timestamp": "2026-03-24T13:53:48+01:00",
          "tree_id": "0ad5182f1377b3f1d91b1055c223856b259fa6a7",
          "url": "https://github.com/Irys-xyz/irys/commit/0ca706c6b69ac4193a981f1b26b75652fa554a7d"
        },
        "date": 1774357890949,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.237144,
            "range": "± 0.082601",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 773.935617,
            "range": "± 35.291638",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1097.067108,
            "range": "± 7.00349",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 10.46178,
            "range": "± 0.212274",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1337.175249,
            "range": "± 101.804692",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1735.270906,
            "range": "± 177.707016",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 2.101878,
            "range": "± 0.118998",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 221.7978,
            "range": "± 5.208315",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.583896,
            "range": "± 2.237352",
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
          "id": "46f66118cf598324172c37524c24523c76263930",
          "message": "feat(multiversion-tests): add cross-version integration test harness (#1207)\n\n* fix: node compatibility for multi-version clusters\n\n- Allow block production without configured price oracles\n- Add synced peer discovery timeout and retry logic in chain sync\n- Add block index wire type backwards compatibility\n- Handle oracle config gracefully in chain startup\n\n* fix: VDF reset seed test and EMA pricing test improvements\n\n- Fix VDF reset seed test for updated block production flow\n- Add oracle-less block production test coverage for EMA pricing\n\n* feat: add multiversion integration test harness\n\nAdd a standalone test harness for cross-version integration testing.\nSupports building and running multiple versions of the node binary,\nmanaging multi-node clusters, and injecting network/crash faults.\n\nKey components:\n- Binary builder with git ref resolution and caching\n- Cluster orchestration with configurable node topologies\n- Health/block-height probes for convergence checks\n- Fault injection (network partitions, process crashes)\n- Port allocation to avoid conflicts in parallel runs\n\nCo-Authored-By: JesseTheRobot <jesse.cruz.wright@gmail.com>\n\n* test: add multiversion E2E and upgrade/rollback tests\n\n- E2E smoke tests: homogeneous cluster block production, mixed-version\n  cluster convergence\n- Upgrade tests: rolling upgrade with block continuity verification,\n  rollback scenario testing\n- Common test utilities for cluster setup and assertions\n\nCo-Authored-By: JesseTheRobot <jesse.cruz.wright@gmail.com>\n\n* feat: add multiversion xtask commands and CI workflow\n\n- Add `cargo xtask multiversion-test` for running cross-version tests\n- Add `cargo xtask clean-data` for cleaning test data directories\n- Add CI workflow triggered on master push, PR approval, and manual dispatch\n\nCo-Authored-By: jason <jason@ridgway-taylor.co.uk>\n\n* fix: improve node cleanup on error\n\n* feat: explicit PriceOracleError\n\n* fix: propagate status file write errors\n\n* fix: remove accessors, add checked_api_urls helper\n\n* feat: split tests\n\n* docs: add explainer comments to NetworkPartitioner\n\n* chore: move multiversion utility to tooling workspace member crate\n\n* chore: move tests\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: update multiversion testing action\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* chore: unify thiserror\n\n* deat: deduplicate deps, remove libc, address feedback\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-03-25T14:17:34Z",
          "tree_id": "93384e882b7df20219fbac31aaaa5e122a7fd659",
          "url": "https://github.com/Irys-xyz/irys/commit/46f66118cf598324172c37524c24523c76263930"
        },
        "date": 1774449357905,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.770966,
            "range": "± 0.076406",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 829.18182,
            "range": "± 38.865371",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 971.317084,
            "range": "± 8.295616",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 8.976595,
            "range": "± 0.23818",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1286.960408,
            "range": "± 64.296505",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1602.778626,
            "range": "± 168.066319",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 3.320966,
            "range": "± 0.85488",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 285.408838,
            "range": "± 27.687894",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 396.954631,
            "range": "± 48.117973",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000119,
            "range": "± 0.000007",
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
          "id": "20b317104218c028d3addae196aa3028622a5b87",
          "message": "fix: ring recompilation (#1252)\n\n* fix: ring recompilation\n\n* fix: update doc comment",
          "timestamp": "2026-03-26T10:53:53Z",
          "tree_id": "3a93e023978a923b1539f254acf835ad68c09e89",
          "url": "https://github.com/Irys-xyz/irys/commit/20b317104218c028d3addae196aa3028622a5b87"
        },
        "date": 1774523593474,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 5.508152,
            "range": "± 0.022591",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 814.475654,
            "range": "± 24.383527",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1096.518602,
            "range": "± 37.002878",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 11.231523,
            "range": "± 0.664582",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1481.985283,
            "range": "± 106.871812",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1654.159732,
            "range": "± 75.248112",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 1.966241,
            "range": "± 0.177517",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 248.404628,
            "range": "± 45.985785",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 403.660684,
            "range": "± 45.474291",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000163,
            "range": "± 0.000026",
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
          "id": "18339ca16b15f59d9579e53049854bf9c896b708",
          "message": "feat: VDF throttling (#1239)\n\n* feat: v1\n\n* feat: remove worktrees\n\n* feat: tune test VDF\n\n* fix: heavy4_ema_intervals_roll_over_in_forks via block state event\n\n* fix: magic 3s sleep mempool tests\n\n* feat: remove magic sleeps\n\n* feat: address feedback\n\n* fix: restore priority for slow tests, improve programmable data API poll\n\n* feat: improve nextest monitor analysis\n\n* feat: capacity resizing pass 1\n\n* wip: spiky test class\n\n* feat: address feedback\n\n* fix: slow capturing spiky tests, add design doc\n\n* feat: address feedback\n\n* feat: address feedback\n\n* chore: fmt\n\n* feat: address feedback\n\n* feat: address feedback\n\n* chore: remove unused _test_name param\n\n* feat: switch from debug_assertions to a regular config\n\n* fix: missing throttle field\n\n* fix: VDF throttle in config ser/des test\n\n* fix: SIGSTOP error in unprivileged environments\n\n* chore: remove commented out priorities\n\n* chore: update gitignore",
          "timestamp": "2026-03-26T21:55:59Z",
          "tree_id": "55d2b41179109bb3d139965d0b0bbcd84d03075b",
          "url": "https://github.com/Irys-xyz/irys/commit/18339ca16b15f59d9579e53049854bf9c896b708"
        },
        "date": 1774563203400,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074793,
            "range": "± 0.001548",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 796.298168,
            "range": "± 28.898556",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 974.721098,
            "range": "± 3.696575",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.122875,
            "range": "± 0.002743",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1210.918671,
            "range": "± 15.098478",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1550.584253,
            "range": "± 24.968353",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.462532,
            "range": "± 0.023795",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.599417,
            "range": "± 1.567998",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 276.111497,
            "range": "± 2.272589",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00011,
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
          "id": "1e2e89894e35e9a90c3ea89cb894c3e8278ca3b6",
          "message": "test: add additional tests (#1206)\n\n* refactor(tests): delete redundant tests, add missing coverage\n\n* refactor: review comments\n\n* refactor(tests): address review comments\n\n* fix(tests): wait for last expected chunk offset in migration guards\n\n* fix(xtask): resolve ownership errors in coverage path\n\n* fix(xtask): drop invalid --workspace flag from coverage report\n\n* fix(coverage): guard missing artifacts and warn on unsupported scope flags\n\n* fix: address review findings across crates\n\n* fix: address review comments\n\n* fix(tests): use shorter activation delay in epoch boundary test\n\n* fix: address review comments\n\n* fix(database): slice buffer to len in GlobalChunkOffset::from_compact\n\n* fix(coverage): guard HTML copy on directory existence\n\n* docs(efficient-sampling): restore comments\n\n* docs(tests): restore helpful comments removed during test consolidation\n\n* refactor: address review findings\n\n* refactor: address review findings\n\n* refactor: address review findings\n\n* refactor: address review findings\n\n* refactor: address review findings\n\n* refactor: address review findings",
          "timestamp": "2026-03-30T18:05:54+01:00",
          "tree_id": "c85997ced0bbd1509f3504ded083680e623ec411",
          "url": "https://github.com/Irys-xyz/irys/commit/1e2e89894e35e9a90c3ea89cb894c3e8278ca3b6"
        },
        "date": 1774891793475,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.078858,
            "range": "± 0.001266",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 753.497253,
            "range": "± 19.213899",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 983.98926,
            "range": "± 9.744591",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.124021,
            "range": "± 0.003771",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1175.468338,
            "range": "± 26.664278",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1525.346941,
            "range": "± 2.127444",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.472973,
            "range": "± 0.019063",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.121346,
            "range": "± 1.69199",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 277.684256,
            "range": "± 1.32394",
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
            "name": "Jesse Cruz Wright",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "da9b36071025155ef3cd88c4eb0e4615b8176f1b",
          "message": "fix: unify edition to 2024 via workspace inheritance (#1261)\n\nAll crate Cargo.toml files now use `edition.workspace = true` instead of\nhardcoding their own edition. This also fixes edition 2024 migration\nissues: reserved `gen` keyword, collapsible if-let chains, pattern\nmatching changes, and impl Trait lifetime capture rules.",
          "timestamp": "2026-03-30T22:10:45+01:00",
          "tree_id": "aff3defe1ea98490dbf7bee658886db076364796",
          "url": "https://github.com/Irys-xyz/irys/commit/da9b36071025155ef3cd88c4eb0e4615b8176f1b"
        },
        "date": 1774906249757,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.080611,
            "range": "± 0.003307",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 833.462782,
            "range": "± 23.226416",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1022.517035,
            "range": "± 15.396029",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.147696,
            "range": "± 0.01404",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1242.668567,
            "range": "± 38.031022",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1707.414069,
            "range": "± 50.137991",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.494774,
            "range": "± 0.039499",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 221.56362,
            "range": "± 3.607211",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 284.718086,
            "range": "± 2.686042",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000112,
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
          "id": "1585b76af4599ed8a6958dbd9dfa206c4ab58e02",
          "message": "fix: devnet fixes (#1253)\n\n* fix: retryable stale parent tx selector error\n\n* feat: rework VDF validation polling logic\n\n* fix: debug assert\n\n* feat: add more tests\n\n* fix: tmpfs fixes\n\n* fix: add max rebuild attempts\n\n* feat: address feedback\n\n* chore: fix max rebuild operator\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: add test mode VdfScheduler\n\n* feat: add VdfScheduler VdfSpawnStrategy for testing\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: refine logic & comments\n\n* chore: add TODO comment",
          "timestamp": "2026-04-01T09:15:45+01:00",
          "tree_id": "36cf6422f486f848f1dd5d05c3ee7842adda9a09",
          "url": "https://github.com/Irys-xyz/irys/commit/1585b76af4599ed8a6958dbd9dfa206c4ab58e02"
        },
        "date": 1775032522879,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074973,
            "range": "± 0.001405",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 770.159372,
            "range": "± 15.34426",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 969.992669,
            "range": "± 4.295085",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.117581,
            "range": "± 0.001058",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1199.42339,
            "range": "± 11.777086",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1532.238535,
            "range": "± 21.715148",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.467051,
            "range": "± 0.016335",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.573121,
            "range": "± 2.758251",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 275.247258,
            "range": "± 1.822002",
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
          "id": "db091339380b6801010fa9467461c5c65fbf8a8e",
          "message": "chore(CI): don't cancel in progress jobs on master",
          "timestamp": "2026-04-01T08:39:54Z",
          "tree_id": "6f73e41463929e41cb1fdab689f90d53389fa393",
          "url": "https://github.com/Irys-xyz/irys/commit/db091339380b6801010fa9467461c5c65fbf8a8e"
        },
        "date": 1775033681020,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.081972,
            "range": "± 0.001807",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 805.457408,
            "range": "± 16.686099",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 971.145354,
            "range": "± 4.882525",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.125182,
            "range": "± 0.004324",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1267.231568,
            "range": "± 54.819931",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1571.628136,
            "range": "± 16.531107",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.476986,
            "range": "± 0.024794",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 213.518318,
            "range": "± 3.620797",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.345875,
            "range": "± 1.407429",
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
          "id": "4e5e184b4d98f3995e586422a8540f3cfaca46da",
          "message": "fix(oracle): add the json feature in for bundler (#1385)",
          "timestamp": "2026-04-02T15:27:07+01:00",
          "tree_id": "9cfee4c4cb1457254c9797037f56c7534fcff036",
          "url": "https://github.com/Irys-xyz/irys/commit/4e5e184b4d98f3995e586422a8540f3cfaca46da"
        },
        "date": 1775140927006,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.081915,
            "range": "± 0.003067",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 789.001037,
            "range": "± 12.155207",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 993.026891,
            "range": "± 20.195387",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.134484,
            "range": "± 0.005366",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1181.278173,
            "range": "± 65.467791",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1563.088455,
            "range": "± 17.28024",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.458383,
            "range": "± 0.019749",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 214.607281,
            "range": "± 2.30654",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.378449,
            "range": "± 0.773554",
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
            "email": "samuraidan@gmail.com",
            "name": "DMac",
            "username": "DanMacDonald"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "8d95afad5d3a16c7f43689bc9d8c0ec3c8248a41",
          "message": "feat: Cascade hardfork — term ledgers (OneYear/ThirtyDay) and height-aware pricing (#1166)\n\n* feat: add annual_cost_per_gb to Cascade hardfork config\n\nThread block height through pricing functions so term/perm fees use the\nCascade-overridden annual cost ($0.028/GB/year) when active, instead of\nthe base rate ($0.01/GB/year). Includes unit and integration tests.\n\n* fix: address PR #1166 review feedback for Cascade hardfork\n\n- Switch Cascade activation from block height to timestamp (matching Borealis pattern)\n- Validate OneYear/ThirtyDay fees in block validation (add term_txs to data_txs_are_valid)\n- Remove silent unwrap_or defaults in block validation and pricing, return errors instead\n- Add term fee & EMA balance checks for OneYear/ThirtyDay in mempool selection\n- Add full validation in gossip path for term ledger txs\n- Add validate_term_ledger_expiry() for expires field validation\n- Reject Submit from pricing endpoint, consolidate shared pricing logic\n\n* refactor: clean up data_txs_are_valid mixed responsibilities\n\nExtract validate_price/validate_term_price closures into standalone\nfunctions, take &BlockTransactions instead of 3 slices, add structural\npre-pass for submit/term txs, include term txs in the inclusion-checking\npipeline with cross-ledger collision guards, and remove redundant\nSteps 5 & 6.\n\n* refactor: use epoch-aligned cascade activation checks\n\nReplace all is_cascade_active_at(timestamp) calls with\nis_cascade_active_for_epoch(epoch_snapshot), consistent with how\nborealis activation is checked. Remove the now-unused method to\nprevent accidental timestamp-based activation checks.\n\n* fix: use single block_tree lock for term ledger pricing\n\nMerge two separate block_tree.read() calls into one lock scope so\ncascade gating and fee/pricing inputs observe the same canonical tip.\n\n* Update crates/types/src/config/mod.rs\n\nCo-authored-by: Roberts Pumpurs <33699735+roberts-pumpurs@users.noreply.github.com>\n\n* fix: use epoch-aligned cascade activation in block producer\n\nReplace raw timestamp comparison against cascade.activation_timestamp\nwith is_cascade_active_for_epoch(epoch_snapshot) when building block\ndata ledgers, consistent with all other activation checks.\n\n* fix: reject term-ledger txs with any perm_fee, not just non-zero\n\nThe previous check `perm_fee.is_some_and(|f| f > zero())` allowed\n`Some(0)` through. Term-ledger txs must not carry a perm_fee at all.\n\n* fix: replace unreachable! with error for Submit ledger in mempool ingress\n\nSubmit is not user-targetable — a malicious peer could still attempt\nto gossip submit-level txs. Return TxIngressError::InvalidLedger\ninstead of panicking.\n\n* fix: return errors instead of panicking in validate_term_ledger_expiry\n\nReplace `continue` on invalid ledger ID with LedgerIdInvalid error,\nand replace `.expect()` on cascade config with proper error propagation.\n\n* fix: correct misleading comment about genesis block submit data\n\n* fix: reject Submit txs with promoted_height instead of just logging\n\nConvert the tracing::error! for Submit ledger transactions that have a\npromoted_height tag into a proper PreValidationError, making it a block\nrejection rather than a silent log.\n\n* fix: strip mempool metadata from txs before building block body\n\nClear promoted_height and included_height metadata from data transaction\nheaders before including them in the block body. This metadata is\nmempool-internal state and must not leak into produced blocks, where it\nwould cause false SubmitTxHasPromotedHeight validation failures on\nself-produced blocks (particularly after node restart when the mempool\nreconstructs metadata from DB).\n\n* fix: skip already-promoted txs during mempool block selection\n\nAfter node restart, the mempool reconstructs promoted_height metadata\nfrom the DB. Previously these txs were re-selected as submit txs\n(just not re-promoted), wasting block space and causing validation\nfailures from the SubmitTxHasPromotedHeight check. Now the mempool\nskips them entirely during block tx selection.\n\n* feat: generate shadow txs for OneYear/ThirtyDay term ledger txs\n\nTerm-only ledger transactions had their fees validated but no shadow\ntransactions were generated to actually debit user balances and credit\nthe treasury on the EVM layer. This adds a TermLedger phase to the\nshadow tx generator that processes one_year and thirty_day txs with\nthe same 5% block-producer / 95% treasury split as submit txs.\n\nAlso adds integration tests verifying wallet balance decrements after\nterm ledger tx inclusion and treasury non-negativity through expiry.\n\n* fix: deterministic tx ordering, miner dedup, and ledger_id validation\n\nM1: Replace HashSet with BTreeSet for miner deduplication in fee\ndistribution so the remainder is assigned deterministically across nodes.\n\nM2: Sort one_year_tx and thirty_day_tx before block inclusion so all\nnodes compute identical merkle roots for the same tx set.\n\nM3: Add ledger_id validation for Publish, OneYear, and ThirtyDay tx\nsets in the structural pre-pass, matching the existing Submit check.\n\n* feat: add configurable num_partitions_per_term_ledger_slot\n\nAdd independent partition count for term ledger slots (OneYear/ThirtyDay),\nreplacing hardcoded replica count of 1. Defaults to same value as\nnum_partitions_per_slot in all configs.\n\n* fix: update config tests for Cascade fields\n\nAdd num_partitions_per_term_ledger_slot to TOML deserialization test\nand testnet config template. Update consensus hash regression test.\n\nCo-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>\n\n* fix: update oversized legacy payload test for Cascade ledger variants\n\nDataLedger::ALL now has 4 entries (added OneYear, ThirtyDay), so the\ntest dynamically builds ALL.len()+1 entries to trigger the overflow.\n\n* remove unreachable oversized legacy payload test\n\nThe overflow error path can never be hit: old nodes only send 2 ledgers\n(Publish, Submit) and new nodes include the explicit ledger field.\n\n* fix: apply rustfmt to chainspec\n\n* fix: collapse nested if for clippy\n\n* docs: add commented-out cascade hardfork example to testnet config template\n\n---------\n\nCo-authored-by: Roberts Pumpurs <33699735+roberts-pumpurs@users.noreply.github.com>\nCo-authored-by: Claude Opus 4.6 <noreply@anthropic.com>\nCo-authored-by: Jesse Cruz Wright <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-04-06T20:34:58-07:00",
          "tree_id": "cf0176818dd68ca6dbcdf43554679ef2acaf5a35",
          "url": "https://github.com/Irys-xyz/irys/commit/8d95afad5d3a16c7f43689bc9d8c0ec3c8248a41"
        },
        "date": 1775534013503,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.078368,
            "range": "± 0.001766",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 760.347565,
            "range": "± 16.251313",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 994.194275,
            "range": "± 24.610766",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.117569,
            "range": "± 0.002286",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1178.654867,
            "range": "± 23.438589",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1520.301957,
            "range": "± 3.004874",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.466402,
            "range": "± 0.026634",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 212.735552,
            "range": "± 2.213207",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 279.447068,
            "range": "± 2.306003",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00011,
            "range": "± 0",
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
          "id": "26fc9bfd88b2b5b8484279c83a8d46109109ca9a",
          "message": "fix: promotion candidate pruning (#1387)\n\n* fix: stale promotion candidate\n\n* fix: scope lenient missing-txid handling to tx-selector path only\n\n* fix: clean CachedDataRoot.txid_set after reorg-driven mempool pruning\n\n* test: gate debug-mode regression test and replace sleep with poll loop\n\n* chore: cargo fmt\n\n* fix: clippy collapsible_if in PruneTxidsFromCachedDataRoots send sites\n\n* refactor: return TxLookupResult struct from get_data_tx_in_parallel_inner; callers own error policy\n\n- Remove TxLookupMode enum entirely\n- get_data_tx_in_parallel_inner now returns TxLookupResult { found, missing }\n- Callers handle missing txids according to their own policy:\n  get_data_tx_in_parallel: Err if any missing (strict, used by block-body serving path)\n  tx_selector: warn + debug_assert, returns partial result (lenient, stale CachedDataRoot path)\n\n* chore: fmt & docs\n\n* chore: fmt\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* fix: tests",
          "timestamp": "2026-04-07T19:14:57+01:00",
          "tree_id": "b4ad951b99625db97a679845936eabed9dce6b2b",
          "url": "https://github.com/Irys-xyz/irys/commit/26fc9bfd88b2b5b8484279c83a8d46109109ca9a"
        },
        "date": 1775586858275,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074749,
            "range": "± 0.000669",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 746.025629,
            "range": "± 2.439555",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 973.563936,
            "range": "± 8.229682",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120673,
            "range": "± 0.002626",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1216.636536,
            "range": "± 16.327133",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1560.349316,
            "range": "± 0.979331",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.496334,
            "range": "± 0.047149",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.630852,
            "range": "± 1.731418",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.941033,
            "range": "± 1.718087",
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
            "name": "Jesse Cruz Wright",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "88fea5fe2f274c60d7ef6ea38d5d84e1a4e50d88",
          "message": "fix(ci): cargo-llvm-cov install (#1379)\n\n* fix(ci): force install cargo-llvm-cov to avoid cached version conflict\n\nThe coverage CI job fails with \"binary already exists in destination\"\nwhen a different version of cargo-llvm-cov is cached. Adding --force\nensures the pinned version is always installed.\n\n* fix: split out sccache env setting and stat reset\n\n* feat(xtask): log functions with mismatched coverage data\n\nAfter generating coverage reports, compares function names in the merged\nprofdata against the JSON export to identify functions with hash\nmismatches. Filters to workspace crates to reduce noise. Writes full\nlist to target/llvm-cov/mismatched-functions.txt.\n\nAlso refactors CmdExt to share env-var removal logic between\nremove_and_run and the new remove_and_read method.\n\n* fix: prevent sccache from being used for problematic C compilation\n\n* feat: improvements\n\n* chore: fmt\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* ci: tweak runner config\n\n* chore: unify sccache env vars\n\n* feat: address feedback\n\n* fix: propagate lcov errors\n\n* fix: make LCOV failures non-fatal",
          "timestamp": "2026-04-08T09:47:04+01:00",
          "tree_id": "7a3ea4baa75841fcde90a572ba5b51cc573c4aeb",
          "url": "https://github.com/Irys-xyz/irys/commit/88fea5fe2f274c60d7ef6ea38d5d84e1a4e50d88"
        },
        "date": 1775639139830,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074742,
            "range": "± 0.000634",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 758.433182,
            "range": "± 29.856518",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 983.404359,
            "range": "± 50.58232",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.117723,
            "range": "± 0.000269",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1177.920457,
            "range": "± 8.234822",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1549.567388,
            "range": "± 20.76798",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.47181,
            "range": "± 0.017324",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 213.52786,
            "range": "± 2.42054",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.217479,
            "range": "± 1.719014",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00011,
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
          "id": "48bcf5f055d06909fc9c3ced4dbd251e37fa1d5d",
          "message": "feat: wait_for_evm_block in mine_block and wait_for_block_at_height",
          "timestamp": "2026-04-08T12:08:41Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1391/commits/48bcf5f055d06909fc9c3ced4dbd251e37fa1d5d"
        },
        "date": 1775658796749,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.0804,
            "range": "± 0.002824",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 814.507145,
            "range": "± 19.225476",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1088.768416,
            "range": "± 14.053685",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.148074,
            "range": "± 0.003178",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1374.569109,
            "range": "± 116.379794",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1866.519832,
            "range": "± 123.127328",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.69365,
            "range": "± 0.147345",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 228.471239,
            "range": "± 32.879726",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 371.258161,
            "range": "± 51.390409",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.000002",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}