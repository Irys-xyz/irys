window.BENCHMARK_DATA = {
  "lastUpdate": 1784033543725,
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
          "id": "cca32388c5a2456c48b34ff4a4c62be30dbf183e",
          "message": "feat: genesis CLI (#1380)\n\n* docs: add implementation plan for standalone genesis block CLI\n\nCo-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>\n\n* feat: add genesis_builder core function with multi-miner support\n\n* feat: add disk I/O for genesis commitment transactions\n\n* feat: add GenesisMinerManifest TOML config for multi-miner genesis\n\nAdd GenesisMinerManifest and GenesisMinerManifestEntry types with serde\nSerialize/Deserialize derives for parsing genesis_miners.toml files.\nIncludes GenesisMinerManifest::load() for reading from disk and\ninto_entries() for converting parsed TOML entries into GenesisMinerEntry\nvalues with hex-decoded signing keys.\n\n* feat: add build-genesis CLI subcommand\n\n* refactor: delegate IrysNode genesis creation to genesis_builder core function\n\n* chore: fmt and clippy fixes for genesis CLI\n\n* feat: validate and canonicalize genesis miner manifest\n\nReject duplicate mining keys and zero-pledge miners in into_entries().\nSort miners by derived IrysAddress so manifest order does not affect\nthe resulting block hash.\n\n* refactor: use sort_by_cached_key to avoid redundant EC derivations in into_entries\n\n* test: verify manifest canonicalization produces stable order\n\n* test: verify build_signed_genesis_block produces deterministic output\n\n* test: verify partition assignments are deterministic from genesis commitments\n\n* fix: compare full PartitionAssignment structs in determinism test\n\nCheck all fields (partition_hash, miner_address, ledger_id, slot_index)\ninstead of only miner_address for stronger determinism verification.\n\n* feat: add generate-miner-info CLI to derive addresses from mining key\n\n* docs: add multi-miner genesis setup guide\n\n* fix: remove redundant clone flagged by clippy\n\n* feat: support building genesis block from pre-signed commitments\n\nAdd build_genesis_block_from_commitments() which packages already-signed\ncommitment transactions into a genesis block, enabling a production\nworkflow where miners independently sign commitments offline and a\ncoordinator assembles them. The CLI build-genesis command now accepts\neither --miners (existing behavior) or --commitments + --signing-key.\n\n* feat: add inspect-genesis CLI to display partition assignments\n\nLoads genesis block and commitments from disk, replays them through\nEpochSnapshot, and prints a partition assignment table grouped by miner.\n\n* docs: update genesis setup guide with existing-commitments workflow and inspect-genesis\n\n* docs: fix genesis balance requirement — pledges are free at genesis\n\n* docs: clarify that genesis miners need both stake and pledges\n\n* feat: add dump-commitments CLI to export commitments from database\n\n* fix: rename _ba variables to _rev to fix typos check\n\n* feat: dump commitment refinement\n\n* docs: update genesis CLI docs\n\n* feat: update functionality\n\n* feat: proper database init\n\n* feat: support additonal key loading methods\n\n* feat: address feedback\n\n* feat: address feedback\n\n* feat: address feedback\n\n* fix: move 3 SM invariant\n\n* feat: address feedback\n\n* chore: remove old docs\n\n* feat: address feedback\n\n* docs: update StorageSubmodulesConfig::load API call to include node_mode parameter\n\n* feat: add genesis block hash mismatch warning to compare-genesis output\n\nAdd prominent visual warning when comparing genesis blocks if their hashes\ndiffer. The comparison now displays:\n- \"Block hash: MATCH\" for matching hashes\n- Bold red warning \"⚠ Block hash: MISMATCH — current and target genesis blocks differ\" when hashes don't match\n\nThis helps operators quickly identify when genesis blocks diverge, which indicates\na critical mismatch requiring investigation.\n\n* feat: improve nextest wrapper timeout behaviour\n\n* feat: address feedback\n\n* fix: refine commitment duplicate guard\n\n* feat: address feedback\n\n* fix: add intraslice duplicate detection\n\n* feat: address feedback\n\n* feat: refine append_commitments guard\n\n---------\n\nCo-authored-by: Claude Opus 4.6 <noreply@anthropic.com>",
          "timestamp": "2026-04-08T15:21:06+01:00",
          "tree_id": "aad165f97b8beebf10d231a6c7076c0533894383",
          "url": "https://github.com/Irys-xyz/irys/commit/cca32388c5a2456c48b34ff4a4c62be30dbf183e"
        },
        "date": 1775659348970,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.084377,
            "range": "± 0.00364",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 867.679206,
            "range": "± 16.121871",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 996.411647,
            "range": "± 36.758818",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120074,
            "range": "± 0.001434",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1246.347125,
            "range": "± 52.672863",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1929.943009,
            "range": "± 155.747644",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.800788,
            "range": "± 0.161612",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 256.750021,
            "range": "± 25.288518",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 282.680067,
            "range": "± 2.546765",
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
          "id": "57fc73c168ed195bc0cd97c0faab33decb1bc27c",
          "message": "fix(build): pass git metadata via env vars for Docker builds (#1386)\n\n* fix(build): pass git metadata via env vars for Docker builds\n\n* fix(build): add rerun-if-env-changed directives and fix Docker git metadata\n\n* fix(docker): default telemetry to local observation stack\n\n* fix(build): validate git env vars and fix sidecar host resolution\n\n* fix: review comments\n\n* fix(build): reject empty GIT_SHA for untagged builds at compile time",
          "timestamp": "2026-04-08T15:33:33+01:00",
          "tree_id": "8bf25017f8306fa3a02b6432b2e9fda3bddd1485",
          "url": "https://github.com/Irys-xyz/irys/commit/57fc73c168ed195bc0cd97c0faab33decb1bc27c"
        },
        "date": 1775660238330,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074694,
            "range": "± 0.000298",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 746.162749,
            "range": "± 3.035919",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 979.159498,
            "range": "± 9.135333",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.121507,
            "range": "± 0.002869",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1180.023644,
            "range": "± 25.339113",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1524.109981,
            "range": "± 1.00325",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.466405,
            "range": "± 0.021749",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.486208,
            "range": "± 2.117413",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 276.477543,
            "range": "± 2.163417",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "276eea580a9bb632444557cf4bdd8e055c91ede9",
          "message": "feat: wait_for_evm_block in mine_block and wait_for_block_at_height (#1391)\n\n* feat: wait_for_evm_block in mine_block and wait_for_block_at_height\n\n* chore: fmt\n\n* fix: remove redundant wait_for_evm_block in mine_block",
          "timestamp": "2026-04-12T19:25:41+01:00",
          "tree_id": "bd0c593b19855834096134734159a931dc76b17d",
          "url": "https://github.com/Irys-xyz/irys/commit/276eea580a9bb632444557cf4bdd8e055c91ede9"
        },
        "date": 1776019247403,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.08243,
            "range": "± 0.00169",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 828.857646,
            "range": "± 31.280374",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 971.305584,
            "range": "± 3.053285",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.118058,
            "range": "± 0.002716",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1270.465361,
            "range": "± 104.846557",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1542.567122,
            "range": "± 17.952736",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.461149,
            "range": "± 0.026833",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 213.816832,
            "range": "± 1.699113",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 279.466147,
            "range": "± 3.066136",
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
          "id": "218660c8da93a416a599cecd476473e7a95f323e",
          "message": "feat: gate ingress proofs on submit ledger confirmation (#1390)\n\n* feat: gate ingress proof generation on submit ledger confirmation\n\nIngress proofs were being generated for unconfirmed/unfunded mempool-only\ntransactions. This change gates proof generation on block_set being non-empty\n(populated when a block confirming the tx in the submit ledger is processed),\nadds a TryGenerateProofsForConfirmedRoots trigger after block confirmation,\nand removes single-block promotion from block production. Block validation\nis unchanged so nodes still accept single-block promotions from peers.\n\n* test: reorder mine-before-proof-wait for submit confirmation\n\nIngress proofs now require submit ledger confirmation, so mine_block()\nmust precede wait_for_ingress_proofs_no_mining. Some tests switch to\nwait_for_ingress_proofs (with mining) when async timing is tight.\n\n- perm_ledger_expiry/mod.rs — heavy_perm_ledger_expiry_basic,\n  heavy_perm_exact_boundary_expiry, heavy_perm_last_slot_never_expires,\n  heavy_perm_and_term_expiry_same_epoch, heavy_perm_partition_recycle_and_reuse,\n  heavy_perm_expiry_disabled_nothing_expires\n- promotion/data_promotion_double.rs — spiky_heavy_double_root_data_promotion_test\n- validation/ingress_proof_validation.rs — block_with_unstaked_ingress_proof_signer_rejected,\n  mempool_filters_unstaked_ingress_proofs\n\n* test: split single-block promotion into two-block flow\n\nTests that asserted Submit+Publish in the same block now mine a submit\nblock, wait for proofs, then mine a promotion block. Balance assertions,\nmempool shape expectations, reorg block delivery, promoted_height checks,\nand wait_for_tx_confirmed_in_raw_mempool calls updated accordingly.\n\n- api/client.rs — api_client_wait_for_promotion_happy_path,\n  api_double_promotion_after_restart\n- multi_node/mempool_tests.rs — heavy3_mempool_publish_fork_recovery_test,\n  promoted_tx_is_not_reselected_for_submit_after_confirmation,\n  pending_chunks_test\n- multi_node/fork_recovery.rs — heavy4_reorg_tip_moves_across_nodes_publish_txs\n\n* test: fix edge-case anchor calculations and evil block strategies\n\nAnchor height formulas adjusted for the extra submit-confirmation block.\nTests with deliberately underfunded txs use manually crafted ingress proofs\nsince those txs can't enter the submit ledger. Evil block strategies updated\nto avoid duplicate-tx validation errors for already-confirmed txs.\n\n- promotion/data_promotion_basic.rs — promotion_validates_submit_inclusion_test,\n  promotion_validates_ingress_proof_anchor_edge_doesnt_promote,\n  promotion_validates_ingress_proof_anchor_edge_does_promote\n- validation/data_tx_pricing.rs —\n  same_block_promoted_tx_with_ema_price_change_gets_rejected\n\n* fix: collapse nested if per clippy\n\n* test: fix stale_txid_in_cached_data_root for submit confirmation gate\n\nThe tx uses a genesis anchor that expires quickly and can never enter the\nsubmit ledger. Use a manually crafted ingress proof instead of relying on\nauto-generation which now requires submit confirmation.\n\n* test: add same-block promotion validation test\n\nVerifies that a block promoting a tx in the same block it enters the\nsubmit ledger passes full validation on a peer node. Our node no longer\nproduces this pattern (ingress proofs require submit confirmation), but\nthe validation rules still permit it.\n\n* fmt: rustfmt same_block_promotion test\n\n* fix: only trigger proof generation for successfully cached data roots",
          "timestamp": "2026-04-16T08:27:32-07:00",
          "tree_id": "26accc759daefbcfcdddeab638a55998f1131acc",
          "url": "https://github.com/Irys-xyz/irys/commit/218660c8da93a416a599cecd476473e7a95f323e"
        },
        "date": 1776354452909,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074878,
            "range": "± 0.000573",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 746.012937,
            "range": "± 1.987994",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 976.881152,
            "range": "± 8.718266",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.124561,
            "range": "± 0.003826",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1189.653957,
            "range": "± 35.494251",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1523.622428,
            "range": "± 1.258542",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.476482,
            "range": "± 0.021507",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.788816,
            "range": "± 1.330029",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.490885,
            "range": "± 1.988238",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00011,
            "range": "± 0.000001",
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
          "id": "3c0ce58ebbd085254b82d7611b1ee195ad36da81",
          "message": "refactor(p2p): reduce data cloning in gossip subsystem (#1249)\n\n* refactor(p2p): reduce data cloning in gossip subsystem\n\n* test(p2p): expand commitment serde parity test to all variants\n\n* refactor(p2p): address review findings for gossip clone reduction",
          "timestamp": "2026-04-17T15:03:29+01:00",
          "tree_id": "3251401e27b770ad75f6d76162380830ef0267e4",
          "url": "https://github.com/Irys-xyz/irys/commit/3c0ce58ebbd085254b82d7611b1ee195ad36da81"
        },
        "date": 1776435491701,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.083581,
            "range": "± 0.001147",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 793.298484,
            "range": "± 29.920566",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 970.028801,
            "range": "± 17.754981",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.146404,
            "range": "± 0.002114",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1282.162768,
            "range": "± 146.035533",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1529.549553,
            "range": "± 13.654501",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.457819,
            "range": "± 0.016283",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 215.623744,
            "range": "± 1.782929",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.586549,
            "range": "± 1.275163",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fd107a5e9280498ae84fcccc784ffceeeb6f8fa8",
          "message": "feat: bundle_format -> metadata_format (#1401)\n\n* feat: bundle_format -> metadata_format\n\n* feat: change gossip fixtures\n\n* fix: migration\n\n* feat: add database migration",
          "timestamp": "2026-04-29T12:35:06+01:00",
          "tree_id": "1fc30e14e89aa9037e8648c0906b4090902db103",
          "url": "https://github.com/Irys-xyz/irys/commit/fd107a5e9280498ae84fcccc784ffceeeb6f8fa8"
        },
        "date": 1777463830822,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074747,
            "range": "± 0.000438",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 746.426336,
            "range": "± 2.784814",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 977.107165,
            "range": "± 5.980124",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.121256,
            "range": "± 0.003108",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1191.644349,
            "range": "± 27.558072",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1523.663602,
            "range": "± 1.888214",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.46573,
            "range": "± 0.02177",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.108317,
            "range": "± 2.268649",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.530517,
            "range": "± 0.889853",
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
          "id": "8ed43e685bd7e0e214e748379b10a8e1ffe5e2a9",
          "message": "fix(storage): map OneYear/ThirtyDay ledgers to storage modules (#1406)\n\nfix(storage): map OneYear/ThirtyDay ledgers to storage modules and migrate their chunks\n\nmap_storage_modules_to_partition_assignments() only processed Publish,\nSubmit, and Capacity partitions — OneYear and ThirtyDay assignments\nexisted in the epoch snapshot but were never forwarded to the\nStorageModuleService.\n\non_block_migrated() only extracted Submit and Publish ledger transactions\nduring chunk migration, so OneYear and ThirtyDay chunks were never\nwritten to storage modules.",
          "timestamp": "2026-05-01T11:44:49-07:00",
          "tree_id": "eb26b23296f0bab4f519070abee615d66a1548bc",
          "url": "https://github.com/Irys-xyz/irys/commit/8ed43e685bd7e0e214e748379b10a8e1ffe5e2a9"
        },
        "date": 1777661971769,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.083769,
            "range": "± 0.004837",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 837.730562,
            "range": "± 25.22574",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1020.788313,
            "range": "± 17.895627",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.145369,
            "range": "± 0.006837",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1491.606439,
            "range": "± 52.676809",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1596.661943,
            "range": "± 119.324765",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.453707,
            "range": "± 0.037271",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 227.744878,
            "range": "± 8.852574",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 292.424308,
            "range": "± 65.864013",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000128,
            "range": "± 0.000012",
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
          "id": "43140be50e1f5e462e859716dc4a4a8a4f8eb3d7",
          "message": "fix(chunk-ingress, p2p): divergence post-mortem fixes for overload cascades (#1394)\n\n* fix(chunk-ingress): reserve control-plane lane and fail fast on overload\n\n* refactor(chunk-ingress): typed overloaded error and configurable lane\n\n* fix(chunk-ingress): preserve pending-chunks drain and split lane budget\n\n* fix(peer-scoring): soften network penalty, lower active threshold\n\n* fix(config): reject control-plane lane exceeding chunk ingress budget\n\n* fix(p2p): classify ingress proof overload as rate_limited\n\n* refactor: drop chunk-ingress lower clamp, tighten bypass tests\n\n* refactor(chunk-ingress): unify overloaded handling for all message types\n\n* fix(peer-scoring): skip anchor penalty while syncing\n\n* fix(chunk-ingress): don't drop no-reply messages under overload\n\n* fix(peer-scoring): sync is_online with peer probe result\n\n* refactor(chunk-ingress): route proof-trigger through control plane\n\n* feat(block-tree): add latest reorg depth metric\n\n* fix(chunk-ingress): decouple shutdown flush from control-lane drain\n\n* feat(block-tree): log cache insert, remove and prune events\n\n* fix(error): log when a reorg is passed the migration boundary\n\n* fix(startup): surface real cause from node lifecycle init failures\n\n* fix(vdf): convert gap and lock-poison panics to graceful exits\n\n* fix: hybrid wait for peers (#1403)\n\n* docs: design spec for hybrid wait_for_active_peers (N peers or timeout)\n\n* docs: place new wait_for_active_peers fields in existing SyncConfig\n\n* docs: implementation plan for hybrid wait_for_active_peers\n\n* feat(config): add sync.min_active_peers and sync.peer_wait_timeout_millis\n\n* refactor(p2p): hybrid wait_for_active_peers (N peers or timeout)\n\n* test: align config TOML test with SyncConfig testing override\n\n* chore: fmt\n\n* refactor(p2p): route min_active_peers and peer_wait_timeout_millis through SyncParams\n\n* test(config): shorten testing peer_wait_timeout_millis to 100ms\n\n* fix(mempool): replace lock-timeout panics with graceful shutdown\n\n* fix(block-tree): convert panic sites to typed errors and graceful logs\n\n* fix(peer-list): sync inner peer_id field when migrating cache entry\n\n* fix: address review comments in chunk-ingress, p2p, chain, vdf\n\n* fix: route subsystem failures through controlled-shutdown path\n\n* fix: route contention, overload, and init-cause signals distinctly\n\n* fix: distinguish retry-races and contention from terminal failures\n\n* chore: tidy logging volume, iteration order, and comment numbering\n\n* fix: rework saturation, pre-validation, and peer-wait handling\n\n* fix: address review comments\n\n* fix(sync): extend test peer-wait timeout to cover handshake\n\n* fix: address review comments\n\n* fix: address review comments\n\n* fix: address review comments\n\n* test(peer-discovery): loop offline decrements past active threshold\n\n* docs(peer-scoring): scrub incident references from comments\n\n* feat: reduce VDF thread pause when actively syncing\n\nThis reduces the time the rest of the system needs to wait for VDF step fast-forwarding, which allows for more than 5 blocks/sec to be processed.\n\n* fix(sync): raise test peer-wait timeout to 10s for restart catch-up\n\n---------\n\nCo-authored-by: dmac <samuraidan@gmail.com>\nCo-authored-by: Jesse <20095347+JesseTheRobot@users.noreply.github.com>\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-05-05T19:38:56+01:00",
          "tree_id": "48176d2b2f8b5765f34bcb3da43fcde03970d7fd",
          "url": "https://github.com/Irys-xyz/irys/commit/43140be50e1f5e462e859716dc4a4a8a4f8eb3d7"
        },
        "date": 1778007478734,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.078816,
            "range": "± 0.001868",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 790.396018,
            "range": "± 27.766595",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1001.206049,
            "range": "± 13.049376",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120403,
            "range": "± 0.001844",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1227.950171,
            "range": "± 18.30886",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1600.740762,
            "range": "± 25.673045",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.473665,
            "range": "± 0.013503",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 212.322833,
            "range": "± 1.5655",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.657418,
            "range": "± 1.795556",
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
          "id": "ab7a4a1b57940c66c5132e8d7b0142bb53530e28",
          "message": "feat(metrics): expose MDBX metrics via Reth's /metrics endpoint (#1409)\n\n* feat(database, telemetry): expose MDBX metrics for consensus DB\n\n* fix(database, telemetry): install metrics recorder unconditionally\n\n* feat(database, chain): add 60s gauge hook for irys consensus DB\n\n* feat(database): add tx_mut acquire histogram for irys consensus DB\n\n* feat(database, telemetry): count libmdbx rw-tx lock stall warnings\n\n* feat(database, telemetry): count libmdbx rw-tx lock stall warnings\n\n* feat(database): add update_scoped to scope-attribute consensus DB stalls\n\n* refactor(telemetry): use dotted field names in tracing spans\n\n* feat(validation, telemetry): add stage, outcome and E2E metrics\n\n* fix(database, telemetry): scope reth-evm writes and harden stall matcher\n\n* fix(validation, telemetry): drop double-count in parent_got_cancelled\n\nThe `parent_got_cancelled` closure in `BlockValidationTask::execute_concurrent`\nrecorded `record_validation_cancellation(\"height_diff\")`, but every path that\nreaches it has already recorded a labelled cancellation inside\n`exit_if_block_is_too_old`:\n\n- `Either::Right` branch: the boxed `exit_if_block_is_too_old(|_| Continue(()))`\n  finished first. Its closure never breaks, so the only ways it returns are via\n  the `height_diff` (line 269) or `channel_closed` (line 302) early returns,\n  both of which record a counter.\n- `Either::Left -> ParentValidationResult::Cancelled` branch: reached only when\n  `wait_for_parent_validation()` returned `Cancelled`, which means the inner\n  `exit_if_block_is_too_old(parent_chain_state_check)` returned via one of\n  `height_diff`, `parent_missing` (line 277), or `channel_closed` — all of\n  which record a counter.\n\nNet effect of the duplicate: every concurrent-stage cancellation was\ndouble-counted, and `parent_missing` / `channel_closed` cancellations were\ninflated with a spurious `height_diff` increment that masked the real reason\nin dashboards.\n\nThe trailing `tracing::warn!`'s \"due to height difference\" wording is now\nslightly misleading for the parent_missing / channel_closed paths, but\nfixing that requires threading the actual reason out of\n`exit_if_block_is_too_old` and is out of scope for this metric-correctness fix.\n\n* refactor(database, telemetry): centralize MDBX span name and scope consts\n\nPromote the rw-tx span name to a public constant in irys-utils alongside\nthe existing `DB_SCOPE_*` constants, and switch all callers to the\ncanonical exports instead of inlining literal strings.\n\nBefore, `crates/database/src/db.rs` declared its own `MDBX_RW_TX_SPAN`,\n`DB_SCOPE_RETH_EVM`, and `DB_SCOPE_IRYS_CONSENSUS` consts with a\n\"keep these literals in sync with mdbx_metrics.rs\" comment, and the\nremaining rw-tx wrap sites in chain, cache_service, and the cli hard-coded\nthe same `\"mdbx_rw_tx\"` + `\"irys-consensus\"` strings inline. Any drift in\nthose strings silently demoted the stall counter to `scope=\"unknown\"` for\nthe affected writer.\n\nChanges:\n- Add `pub const MDBX_RW_TX_SPAN` next to the `DB_SCOPE_*` consts in\n  `crates/utils/utils/src/mdbx_metrics.rs` and re-export it from `lib.rs`.\n- Drop the three local consts and the \"keep in sync\" comment in\n  `crates/database/src/db.rs`; import them from `irys_utils` instead.\n- Replace inlined literals at the four wrap sites in\n  `crates/chain/src/chain.rs`, `crates/actors/src/cache_service.rs`,\n  `crates/cli/src/commands.rs`, and `crates/cli/src/db_utils.rs`.\n- Add `irys-utils` to `crates/database/Cargo.toml` (no cycle —\n  irys-utils does not depend on irys-database).\n\nHistogram label string values (e.g. `\"scope\" => \"irys-consensus\"`) are\nleft untouched; those are metrics-layer label values, independent of the\ntracing span field and out of scope for this refactor.\n\n* perf(telemetry): cache tx_mut acquire histogram handles + add description\n\n`IrysDatabaseExt::update_eyre` previously called `metrics::histogram!(...)`\ninline on every invocation. The macro doesn't allocate a new histogram per\ncall (the recorder amortises lookups), but it still pays a recorder\nindirection + per-call `Key` construction on a path that runs for every\nconsensus and EVM rw-tx. There was also no `describe_histogram!` for the\nmetric, so its prometheus output had no HELP line.\n\nChanges:\n- Add `DB_TX_MUT_ACQUIRE_DURATION_SECONDS` constant + `describe_histogram!`\n  (Unit::Seconds, explanatory description) in `irys-utils::mdbx_metrics`,\n  re-exported from the crate root. Description noted as\n  recorder-default-bucketed; per-metric bucket tuning requires touching\n  the prometheus recorder install in `install_metrics_recorder`.\n- Cache the per-scope `Histogram` handles as `LazyLock<Histogram>` statics\n  in `crates/database/src/db.rs` so the rw-tx path only calls `.record()`.\n  Inline comment documents the binding-timing tradeoff: the handle binds\n  to whatever recorder is global at first use, so any test relying on\n  `metrics::with_local_recorder` will not see writes from this path.\n  Production safety is enforced by `install_metrics_recorder()` running\n  before any DB is opened (chain/src/main.rs).\n- As a side benefit, the histogram name and the `\"scope\"` label values\n  are now sourced from the same constants used elsewhere, removing the\n  last remaining magic-string literals in this file.\n\n* chore: fmt\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-05-12T12:09:07+01:00",
          "tree_id": "eb1554ac0121f72f3cf633e08c5c7286e8cfc85f",
          "url": "https://github.com/Irys-xyz/irys/commit/ab7a4a1b57940c66c5132e8d7b0142bb53530e28"
        },
        "date": 1778585204555,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.074766,
            "range": "± 0.001166",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 757.037097,
            "range": "± 11.26515",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1004.107461,
            "range": "± 30.039968",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.11795,
            "range": "± 0.001045",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1186.123203,
            "range": "± 8.690713",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1551.204673,
            "range": "± 11.692492",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.462338,
            "range": "± 0.031781",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.999331,
            "range": "± 1.988239",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.168789,
            "range": "± 6.201586",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "109c94cffab577e6739497c2dee985cdb903d6bb",
          "message": "feat: vdf progress (#1411)\n\n* docs: add VDF validation progress-check implementation plan\n\n* feat(vdf): add progress_timeout_secs to VdfConfig\n\n* feat(vdf): add cancel + progress check to wait_for_step\n\nExtend VdfStateReadonly::wait_for_step with a cancel signal and a\nprogress-timeout Duration. The progress check tracks (last_observed_step,\nlast_progress_at): if global_step does not advance within the timeout\nwindow, the wait bails with a typed error instead of hanging forever.\nThis catches the case where the local VDF writer thread has died (e.g.,\npoisoned lock -> run_vdf returns) without imposing a wall-clock cap on\nlegitimately long waits.\n\nAlso enable tokio's `test-util` feature for irys-vdf dev-deps so the new\ntests can use `start_paused = true`. The single in-tree caller in\nvalidation_service.rs is updated minimally to keep the workspace\ncompiling; full plumbing of cancel + configured progress_timeout to that\nsite lands in Task 4.\n\n* chore(validation): clarify Task 4 TODO at site C shim\n\n* refactor(validation): route site A through VdfStateReadonly::wait_for_step\n\n* fix(validation): plumb cancel signal into site C's wait_for_step\n\n* feat(validation): promote ensure_vdf_is_valid stage logs to debug/info\n\n* chore(validation): drop redundant pre-entry debug log\n\n* feat(vdf): log explicit error when run_vdf exits via poisoned store_step\n\n* test(chain-tests): document VDF progress-check integration test (ignored)\n\n* style: cargo fmt fixes after task 5/7\n\n* docs(vdf): clarify worst-case progress-timeout latency\n\n* Implement progressive VDF validation watchdog\n\n* Fix bounded VDF fast-forward receiver wiring\n\n* feat: panic on watchdog intervention\n\n* feat: improvements\n\n* docs: produce ADR\n\n* chore: fix typo\n\n* feat: code refinement\n\n* fix: address feedback\n\n* feat(metrics): expose VDF stage durations and stall/preempt labels\n\nIntegrates the branch's VDF stall-detection work with master's new\nvalidation-metrics framework.\n\n- Wire `record_vdf_step_wait_duration_ms` to both `wait_for_step` calls\n  in `ensure_vdf_is_valid`. Replaces the per-call helper that master\n  added and the branch removed in the wait_for_step refactor.\n- Record per-VDF-stage histograms via `record_validation_stage_duration_ms`\n  inside `record_vdf_task_progress`. Stage labels are `vdf_`-prefixed\n  (e.g. `vdf_validate_batch`) so they don't collide with the existing\n  concurrent-validation labels (`seeds`, `recall_range`, `poa`,\n  `shadow_tx`, `concurrent_overall`).\n- Label `VALIDATION_TASK_FORCE_ABORTED` by stage so operators can see\n  which stage is stalling when the watchdog fires.\n- Categorise `wait_for_step` failures on\n  `record_validation_cancellation`: `vdf_preempted` for cooperative\n  cancellation, `vdf_stalled` for the progress-timeout path. The two\n  failure modes are deliberately kept separate end-to-end.\n\n* chore: fmt\n\n* feat: record global VDF step more often\n\n* docs: update comment",
          "timestamp": "2026-05-12T18:45:40+01:00",
          "tree_id": "a9e163dc53f45b2908ac40c08839a0a40093809e",
          "url": "https://github.com/Irys-xyz/irys/commit/109c94cffab577e6739497c2dee985cdb903d6bb"
        },
        "date": 1778609137937,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.077161,
            "range": "± 0.001809",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 771.539426,
            "range": "± 14.51583",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 983.128616,
            "range": "± 58.615413",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120214,
            "range": "± 0.001263",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1221.104156,
            "range": "± 13.714398",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1616.51303,
            "range": "± 13.208116",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.479356,
            "range": "± 0.020136",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.294925,
            "range": "± 3.799295",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 275.901588,
            "range": "± 2.469455",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "26a4a113889b9a406c48dd7bdd582f6bb77b4d1d",
          "message": "feat: VDF stall detection logic tweaks (#1413)",
          "timestamp": "2026-05-12T21:56:18+01:00",
          "tree_id": "fa765e9f9be933a79f6ea14b4e80be23288f402e",
          "url": "https://github.com/Irys-xyz/irys/commit/26a4a113889b9a406c48dd7bdd582f6bb77b4d1d"
        },
        "date": 1778620365181,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.083773,
            "range": "± 0.003701",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 839.874989,
            "range": "± 20.336893",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1081.910743,
            "range": "± 45.787197",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.128461,
            "range": "± 0.004708",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1305.852567,
            "range": "± 101.187913",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1625.991332,
            "range": "± 50.53043",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.462202,
            "range": "± 0.022552",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 219.896795,
            "range": "± 1.302453",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 279.01334,
            "range": "± 1.978111",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "39a69b166ecdebbc7c397d05ab1f567bad8ef784",
          "message": "feat: preval perf (#1414)\n\n* perf(preval): use long-lived rayon pool for VDF checkpoint validation\n\nlast_step_checkpoints_is_valid was building a fresh rayon::ThreadPool\ninside spawn_blocking on every block. The function is now sync and\ntakes &rayon::ThreadPool, mirroring vdf_step_batch_is_valid in state.rs.\n\nBlockDiscoveryServiceInner owns an Arc<rayon::ThreadPool> sized to\nparallel_verification_thread_limit, built once at node startup. It is\nthreaded through prevalidate_block and wrapped in spawn_blocking at the\nsingle VDF call site for now; subsequent commits will reuse the same\npool for transaction-signature and ingress-proof verification.\n\n* perf(preval): borrow PoA chunk instead of cloning before SHA-256\n\nThe PoA chunk is up to 256 KiB and we never mutate it during\nprevalidation. Borrow it as &[u8] for both the chunk-hash SHA-256 and\nsolution_hash_link_is_valid, saving one allocation + memcpy per block.\n\n* perf(preval): parallelize transaction signature verification\n\nvalidate_transactions ran tx.is_signature_valid() (ECDSA recover +\nkeccak, ~50-100us per tx) in a serial for-loop. For a full block this\nserial cost dominates prevalidation. Convert to a rayon par_iter on the\nshared BlockDiscovery pool, short-circuiting on the first failure via\ntry_for_each. Applies to both data-ledger and commitment-ledger txs.\n\n* perf(preval): parallelize ingress proof ECDSA recovery\n\nproof.pre_validate runs a secp256k1 signer recovery per ingress proof\nper published tx, previously in a serial nested loop. Split into two\npasses: a sequential collect (get_ingress_proofs is non-crypto and runs\nfine serially), then a parallel try_for_each on the shared pool that\nshort-circuits on the first signature failure.\n\n* docs(preval): expand pool doc-comment to cover all uses\n\nThe pool is now used for VDF checkpoints, transaction-signature ECDSA,\nand ingress-proof ECDSA recovery — not just VDF checkpoints. Also drop\n\"vdf\" from the construction-site expect message.\n\n* refactor(vdf): add build_verification_pool helper to dedupe construction\n\nPool construction (ThreadPoolBuilder::new().num_threads(...).build())\nwas repeated across 10+ sites (production, tests, bench). Centralize in\nirys_vdf::build_verification_pool(&VdfConfig).\n\nAlso: chain-tests now sources the thread count from config rather than\nhardcoding num_threads(2), and irys-chain / irys-chain-tests no longer\nneed a direct rayon dependency (only the helper is called).\n\n* refactor(preval): pre-size ingress_pairs and drop narration comments\n\n- Vec::with_capacity for ingress_pairs using publish_ledger's\n  required_proof_count when present, avoiding ~log2(n) reallocations on\n  a hot path.\n- Drop \"First pass / Second pass / parallel ECDSA / cold path\" comments\n  that narrated what the code does. The remaining comment captures the\n  reason for flattening: the parallel pass fans out across every proof.\n\n* fix(preval): split internal task-join failures from consensus rejections\n\nThe spawn_blocking wrapping last_step_checkpoints_is_valid was mapping\ntokio::task::JoinError (a panic in the verifier thread) into\nPreValidationError::VDFCheckpointsInvalid. That conflates a local\nruntime failure with a consensus-level \"block is invalid\" verdict —\ncatastrophic in a chain context: an honest peer's valid block could be\nrejected and the peer penalised because our own thread panicked.\n\n- Add PreValidationError::InternalTaskJoin for spawn_blocking join\n  failures, with a SAFETY-CRITICAL doc comment on the enum spelling out\n  the invariant: non-validation errors MUST NEVER be mapped to\n  consensus-validation variants.\n- Add PreValidationError::is_internal_failure() classifier.\n- block_pool now routes internal failures to OtherInternal (matching the\n  treatment of ParentNotInCache: the peer is innocent), not BlockError.\n- block_discovery's prevalidation metric tags internal failures as\n  \"internal_error\" so the rejection-rate counter isn't inflated by\n  unrelated runtime issues.\n- Log the JoinError with structured context before mapping.\n- Tests covering the classifier behaviour.\n\n* fix(block_pool): keep cached block on internal prevalidation failure\n\nFor PreValidationError::is_internal_failure() (currently just\nInternalTaskJoin, i.e. a verifier panic captured by spawn_blocking),\nthe failure is in our local verifier thread, not in any shared in-memory\nstate. Removing the block forces a refetch round-trip even though we\nstill have the bytes and our state is intact.\n\nSwitch to flipping `is_processing` back to false instead. The next\ngossip arrival or orphan-resolve on a child block will retry\nprevalidation against the same cached block. Retry rate is bounded by\ngossip arrival, so a deterministic verifier panic on adversarial input\nisn't a tight loop.\n\nOther failure paths are unchanged: FatalCacheCorruption (genuinely\nunrecoverable) and ParentNotInCache (deliberately drops orphans, since\nthey'll be reprocessed when the parent arrives) keep removing.\n\n* chore: fmt\n\n* chore: add TODO",
          "timestamp": "2026-05-13T11:50:14+01:00",
          "tree_id": "694a12a0caa28529f65fd782dcf043b394bab69a",
          "url": "https://github.com/Irys-xyz/irys/commit/39a69b166ecdebbc7c397d05ab1f567bad8ef784"
        },
        "date": 1778670283354,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "vdf_sha/testing",
            "value": 0.082614,
            "range": "± 0.002657",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 830.518219,
            "range": "± 19.50037",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 985.469473,
            "range": "± 27.84369",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.12019,
            "range": "± 0.001044",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1227.634046,
            "range": "± 94.804736",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1567.131036,
            "range": "± 15.505603",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034424,
            "range": "± 0.001111",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.826025,
            "range": "± 1.449277",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.216987,
            "range": "± 3.195252",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000119,
            "range": "± 0.000002",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "4a08aa380a9a7b6da48215662542ef58425d9393",
          "message": "perf(efficient-sampling): skip HashMap on validation reconstruct path (#1418)\n\nBlock validation only needs the final recall range, but the freestanding\nget_recall_range was driving Ranges::reconstruct which inserts each\nintermediate pick into last_recall_ranges and updates last_step_num. On\nmainnet (64_840 ranges per partition) that's ~64k wasted HashMap inserts\nper recall_range_is_valid call.\n\nExtract the pure swap-pick into Ranges::pick_next (no HashMap, no\nlast_step_num) and have the freestanding get_recall_range loop on it\ndirectly, keeping only the final value. The mining path\n(Ranges::next_recall_range, used by partition_mining_service) is\nunchanged - it still goes through pick_next and then updates its\nbookkeeping. get_last_recall_range was orphaned by the change and is\nremoved.\n\nBench (crates/efficient-sampling/benches/recall_range.rs, criterion,\np < 0.05 at every size):\n\n  steps      before      after       delta\n  100        12.77 us    10.23 us    -20.9%\n  1000       132.5 us    102.9 us    -23.0%\n  10000      1.327 ms    1.048 ms    -20.4%\n  64840      9.30 ms     6.82 ms     -26.6%\n\nThe win scales with N as expected (HashMap rehash cost dominates the\ntail). Existing determinism/uniqueness proptests confirm behavior is\nunchanged.",
          "timestamp": "2026-05-13T21:35:09+01:00",
          "tree_id": "f5152bed17f21714b9dc85cfaac8e3a5883236e5",
          "url": "https://github.com/Irys-xyz/irys/commit/4a08aa380a9a7b6da48215662542ef58425d9393"
        },
        "date": 1778705431018,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015633,
            "range": "± 0.000728",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.156961,
            "range": "± 0.003039",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.602502,
            "range": "± 0.040545",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.698956,
            "range": "± 0.235592",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083356,
            "range": "± 0.001224",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 779.306915,
            "range": "± 34.248714",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 974.269544,
            "range": "± 13.217994",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.123681,
            "range": "± 0.003193",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1230.294701,
            "range": "± 96.343181",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1542.41204,
            "range": "± 14.240158",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034156,
            "range": "± 0.001321",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.214735,
            "range": "± 1.686418",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.402562,
            "range": "± 1.428315",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000117,
            "range": "± 0.000001",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "47923f41902e11765b36d14b409dac2b60db0ba3",
          "message": "fix: block pool race (#1417)\n\n* fix(block_pool): wait for in-tree pending parent instead of orphan re-pull\n\nThe old `block_status_provider` collapsed every in-tree block into\n`ProcessedButCanBeReorganized`, so `block_pool::process_block` could\nnot tell \"parent in tree, validation still pending\" from \"parent in\ntree, validated\". Descendants of an unvalidated parent fell through to\n`block_discovery.handle_block`, which added them to the tree, scheduled\ntheir validation, and left the parent-wait to `validation_service`'s\n`wait_for_parent_validation`. If the parent then failed validation it\nwas removed from the tree, and every descendant already added\ncascade-failed with `Parent block not found` → `Cancelled` → invalid →\nalso removed. On the testnet `GogWkmVot…` divergence this produced\n628 orphan events / 354 redundant validation cycles for a single\nnot-yet-validated parent.\n\nSurface the parent's `ChainState` through `BlockStatus`:\n\n- `BlockStatus::InTreePendingValidation` for `ChainState::NotOnchain`\n  /`Validated` paired with `Unknown` or `ValidationScheduled`.\n- `is_in_tree()` predicate covering `InTreePendingValidation |\n  ProcessedButCanBeReorganized | Finalized`. `is_processed()` stays\n  strict (only the validated/finalized/pruned-fork set).\n- `block_status()` reads `BlockTree::get_block_and_status` and maps\n  `ChainState` into the new variants.\n\nIn `block_pool::process_block`:\n\n- Gate the orphan re-pull branch on `!is_in_tree()` instead of\n  `!is_processed()`. The orphan branch now only fires when the parent\n  is genuinely missing.\n- When the parent is `InTreePendingValidation`, hold the child via a\n  new `wait_for_parent_validation` helper. The helper subscribes to\n  `service_senders.block_state_events` *before* re-reading parent\n  state (so a transition between the initial check and the subscribe\n  is not lost), tolerates `Lagged` events by re-reading from the\n  tree, and treats `Closed` as `Invalid`.\n- On `Valid` the child resumes the normal `process_block` path. On\n  `Invalid` the child is removed from `blocks_cache` with a new\n  `FailureReason::ParentValidationFailed` and the sync state records\n  the failure. The descendant never reaches the tree, so a failed\n  parent does not cascade through its children.\n- `check_block_status` adds an `InTreePendingValidation` arm returning\n  `Advisory::AlreadyProcessed` (gossip handlers must not re-enter\n  `process_block` for an already-in-tree block).\n- `is_block_processing_or_processed` includes `is_in_tree()` so gossip\n  handlers also skip blocks that are in the tree but still pending.\n\nInline unit tests in `block_status_provider` cover the three status\ntransitions (in-tree pending → validated → missing).\n\n* test(block_pool): cover InTreePendingValidation wait-for-parent path\n\nFour functional tests against the real `BlockPool` (with a mocked\nblock_discovery / mempool / service_senders) plus two helpers:\n\n- `process_block_does_not_re_pull_parent_in_tree_pending_validation`:\n  adds a parent to the mock tree (default state = InTreePendingValidation),\n  spawns `process_block(child)`, asserts no orphan-cascade message\n  (`RequestBlockFromTheNetwork` / `AttemptReprocessingBlock`) reaches\n  the sync channel during the wait and that the child has not fallen\n  through to block_discovery. Then fires a `BlockStateUpdated` event\n  promoting the parent to `Validated(ValidBlock)`, verifies the\n  spawned future resolves with `Processed`, and that the child finally\n  reaches block_discovery.\n- `process_block_drops_child_when_pending_parent_validation_fails`:\n  removes the parent from the tree mid-flight and broadcasts a\n  discarded `BlockStateUpdated`, asserts `process_block` returns\n  `BlockError(... failed validation ...)`, child is not in\n  block_discovery, child is not in the block_pool cache, and no\n  orphan-cascade message was emitted.\n- `process_block_rejects_block_already_in_tree_pending_validation`:\n  exercises `check_block_status`'s new arm — a process_block call for\n  a block already in the tree as InTreePendingValidation returns\n  `Advisory::AlreadyProcessed` and does not re-enter block_discovery.\n- `is_block_processing_or_processed_true_for_in_tree_pending_validation`:\n  exercises the gossip-handler gate — before adding to the tree the\n  predicate is false; after, it is true.\n\n`build_test_pool` collapses the 8-line per-test setup, and\n`is_orphan_cascade_message` is the narrow predicate the wait-path\ntests use against the sync channel (allowing the expected\n`BlockProcessedByThePool` notification to pass through).\n\n* test(vdf): wire up stalled-peer integration test via direct tree injection\n\n`heavy_test_vdf_progress_check_fails_stalled_peer` was added in\ne9bca3026 as `#[ignore]`d documentation, pending a way to deliver a\nblock to the peer whose parent is in the tree but whose VDF steps\nwere never fast-forwarded into the peer's `vdf_state`. The original\n\"gossip the head only\" setup cannot exercise the progress check on\nits own: when the head's parent is `NotProcessed`, `block_pool` runs\nthe orphan-fetch cascade, each fetched ancestor's\n`ensure_vdf_is_valid` calls `fast_forward_validated_steps`, and by the\ntime the head reaches VDF validation the gap has been bridged.\n\nRework the test to use direct tree injection:\n\n- Enable `irys-domain`'s `test-utils` feature in `chain-tests/Cargo.toml`\n  so `BlockTreeReadGuard::write()` is available outside `#[cfg(test)]`.\n- Mine the head's parent privately on genesis, then drop it straight\n  into peer's `block_tree` via `add_block` + `mark_block_as_validation_scheduled`\n  + `mark_block_as_valid`. Snapshots are inherited from the grandparent\n  (peer's height-2 tip) — valid for a no-commitment, non-epoch block.\n  Crucially, this skips block_discovery / block_tree_service /\n  validation_service entirely for the parent, so its VDF steps are\n  never fast-forwarded into peer's `vdf_state`.\n- Mine the head privately, then deliver it via `send_full_block`. That\n  call bypasses `block_pool` and the orphan cascade; block_discovery\n  finds the parent already in the tree, schedules `ValidateBlock` for\n  the head, and `ensure_vdf_is_valid` enters Stage A\n  (`wait_for_step(prev_output_step)`). Peer cannot reach that step\n  (mining is stopped, no fast-forward source for the intermediate\n  steps), and after `progress_timeout_secs` the wait bails with\n  `WaitForStepError::Stalled`.\n\nUpdate the assertion to match the current consensus-safety contract:\na `Stalled` wait must panic per the never-mislabel rule (see\n`active_validations.rs:150` and `design/docs/vdf-validation-stall-detection.md`).\nThe cleanest in-test signal is the validation_service task's `mpsc`\nreceiver being dropped when it unwinds — poll\n`service_senders.validation_service.is_closed()`. The test also\nsubscribes to `block_state_events` as a regression detector: a `Valid`\nevent for the head would mean fast-forward bridged the gap somehow,\nand is treated as a loud test failure. As defense-in-depth, the test\nalso asserts peer's `global_step` did not advance during the wait.\n\nStable: 5/5 passes in ~9s each.\n\n* fix: peer_base_url_format proptest\n\n* feat: add wait_for_parent_validation metrics/logging\n\n* test(block_pool): cover Validated(_) pending mapping and Lagged event path\n\nFills two of the four gaps called out in review.md P2:\n\n- Two unit tests for `block_status`: locally-produced blocks inserted as\n  `Validated(Unknown)` and `Validated(ValidationScheduled)` must map to\n  `InTreePendingValidation`, the new branch added in b0b8db85c.\n- One integration test that deterministically drives\n  `broadcast::RecvError::Lagged` inside `wait_for_parent_validation` by\n  burst-sending 201 events synchronously into the 100-cap channel before\n  promoting the parent to valid — verifies the wait loop re-reads tree\n  state on lag and still exits cleanly.\n\nAdds `BlockStatusProvider::add_block_mock_with_state` test helper that\nwraps `BlockTree::add_common` so tests can plant arbitrary `ChainState`.\n\nTwo gaps deferred: `RecvError::Closed` (Sender lives inside Arc held by\nthe running BlockPool; not testable as black-box without DI refactor)\nand the subscribe-vs-initial-check race window (µs-scale; safety is\nstructural ordering in source).\n\n* docs: scrub external report references from in-tree comments\n\nReplaces \"Fix 2\" / \"commit b0b8db85c\" / numbered-issue references with\nself-contained summaries describing what the code actually does. The\nexternal post-mortem doc those numbers were keyed to lives outside this\nrepo, so the citations would rot. Comment-only; no behavior change.\n\n* feat: re-do the fix\n\n* fix: address feedback\n\n* fix(test): gate validation_service closure on tree presence, not BlockStateUpdated\n\nThe prior `saw_head_update` gate required a `BlockStateUpdated{block_hash:\nhead_hash}` event before accepting `validation_service.is_closed()` as\nsuccess. That event is never emitted in the Stalled-panic path this test\nexercises: `BlockStateUpdated` only fires from `on_block_validation_finished`,\nbut the VDF-wait panic in `active_validations.rs` re-unwinds through the\nvalidation_service select loop via `std::panic::resume_unwind` (per the\n\"never mislabel\" rule), so validation never \"finishes\". The gate therefore\nsat unsatisfied until the 23s deadline and the test asserted failure.\n\nReplace with a `block_tree.get_block(&head_hash).is_some()` check — proves\nprevalidation reached peer's tree (i.e. the head was scheduled into\nvalidation_service) before declaring its closure attributable to our\nscenario. Addresses the reviewer's underlying concern (rule out unrelated\npanics) using a signal that actually exists in this flow.\n\n* fix: address feedback",
          "timestamp": "2026-05-14T15:16:20+01:00",
          "tree_id": "128519c0a7a0e13f24ffd2a63112ae7893e44aa1",
          "url": "https://github.com/Irys-xyz/irys/commit/47923f41902e11765b36d14b409dac2b60db0ba3"
        },
        "date": 1778769104809,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015311,
            "range": "± 0.000336",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.125547,
            "range": "± 0.003824",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.258821,
            "range": "± 0.02195",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.853734,
            "range": "± 0.133154",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078966,
            "range": "± 0.000624",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 776.130622,
            "range": "± 19.269645",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 984.504879,
            "range": "± 18.478321",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.147864,
            "range": "± 0.004396",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1206.588365,
            "range": "± 130.544275",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1563.884936,
            "range": "± 17.247889",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034317,
            "range": "± 0.001526",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.200682,
            "range": "± 1.193731",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.509775,
            "range": "± 1.763668",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000118,
            "range": "± 0.000004",
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
          "id": "1df9e88009491e756e2a84cd333f30ad1b157d0c",
          "message": "refactor(telemetry): remove Axiom log broadcasting (#1420)",
          "timestamp": "2026-05-14T19:35:47+01:00",
          "tree_id": "e0c8ef42e3dbb86e59785d81b47cf7dea0b99552",
          "url": "https://github.com/Irys-xyz/irys/commit/1df9e88009491e756e2a84cd333f30ad1b157d0c"
        },
        "date": 1778784662518,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015307,
            "range": "± 0.000306",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.126065,
            "range": "± 0.00369",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.284002,
            "range": "± 0.014121",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.349129,
            "range": "± 0.141339",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075568,
            "range": "± 0.001543",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 761.838653,
            "range": "± 28.470832",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 981.391292,
            "range": "± 22.522578",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.147832,
            "range": "± 0.00269",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1202.277946,
            "range": "± 26.341195",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1562.28336,
            "range": "± 11.045859",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.03198,
            "range": "± 0.002412",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.26765,
            "range": "± 1.882062",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 275.301377,
            "range": "± 2.083133",
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
          "id": "acbf85f92f7b611c001caec3169f51645baafdd1",
          "message": "fix(p2p): break body-pull circuit-breaker cascade (#1412)\n\n* fix(p2p): break body-pull circuit-breaker cascade\n\n* fix(debug-utils): add bs58/hex deps and satisfy clippy for inspect_tx\n\n* fix(p2p, debug-utils): address review feedback\n\n* fix(debug-utils): tidy inspect_tx docs and reject extra CLI args\n\n* fix(debug-utils): make lca_height an optional CLI arg in inspect_tx\n\nReplace the hardcoded `lca_h: u64 = 832972` (an incident-specific block\nheight from the divergence post-mortem) with an optional third CLI arg.\nThe LCA-dump section is skipped when omitted, so the tool remains useful\non unrelated DBs without producing misleading `LCA = NONE` output.\n\n* docs(p2p): document pull-loop invariants and assert CB recovery in hydrate test\n\nPost-review clarifications to the rewritten `pull_data_from_network`:\n\n- Restore the re-gossip rationale on the `None =>` arm (the peer is kept\n  for future rounds because gossip may deliver the data between attempts).\n- Document the at-most-once invariant for `handshake_retry_candidates`:\n  the arm moves `peer` and does not push to `next_retryable`, so a peer\n  appears in the post-loop retry at most once per call. Note what would\n  break if someone added a `next_retryable.push(peer.clone())` there.\n- Note that `errors_by_peer` overwrites on re-insert (last error per peer\n  wins) — intended for the failure summary, not historical record.\n\nExpand `hydrate_marks_cb_open_peer_offline` with two recovery invariants:\nthe offline peer remains enumerable via `all_peers_sorted_by_score`, and\nafter `record_success` clears the CB, `is_available` returns true so the\nnext hydrate cycle's `check_health` will actually attempt the request\ninstead of short-circuiting.\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-05-15T11:06:59+01:00",
          "tree_id": "e2a271cb16ed4fec20ce686289abacf98e59a851",
          "url": "https://github.com/Irys-xyz/irys/commit/acbf85f92f7b611c001caec3169f51645baafdd1"
        },
        "date": 1778840404096,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015155,
            "range": "± 0.002311",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.15386,
            "range": "± 0.064055",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.577134,
            "range": "± 0.160424",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.452055,
            "range": "± 0.457771",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083357,
            "range": "± 0.00098",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 842.419363,
            "range": "± 12.6734",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1082.243239,
            "range": "± 29.443693",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.138647,
            "range": "± 0.008162",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1387.864123,
            "range": "± 93.5051",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1681.489089,
            "range": "± 141.449351",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.098857,
            "range": "± 0.023671",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 234.165697,
            "range": "± 13.440736",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.015324,
            "range": "± 2.887973",
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
            "name": "JesseTheRobot",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "jesse.cruz.wright@gmail.com",
            "name": "JesseTheRobot",
            "username": "JesseTheRobot"
          },
          "distinct": true,
          "id": "3293b7514cda2e2293f1bc9f3cc6f9519c232bd2",
          "message": "fix: small metrics followup for #1412",
          "timestamp": "2026-05-15T10:21:02Z",
          "tree_id": "6ad167951e6af4d02c0d320813c877f6c47f222a",
          "url": "https://github.com/Irys-xyz/irys/commit/3293b7514cda2e2293f1bc9f3cc6f9519c232bd2"
        },
        "date": 1778841453816,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015249,
            "range": "± 0.001093",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.124482,
            "range": "± 0.003069",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.225812,
            "range": "± 0.008394",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.833678,
            "range": "± 0.294041",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078818,
            "range": "± 0.000465",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 765.522302,
            "range": "± 11.200423",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 996.219334,
            "range": "± 48.654719",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120412,
            "range": "± 0.001968",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1202.959378,
            "range": "± 37.081795",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1943.842038,
            "range": "± 81.562609",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.136384,
            "range": "± 0.042269",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 219.417039,
            "range": "± 12.335743",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 284.423924,
            "range": "± 2.371555",
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
          "id": "2953962ba54710ddb0203cdfcc0f3b27297cf3f5",
          "message": "feat(snapshot): portable chain-state import/export tooling (#1419)\n\n* feat(snapshot): add portable chain state import/export\n\n* fix(snapshot): close import-path correctness gaps from review\n\n* refactor:review comments\n\n* refactor: address review comments\n\n* fix(snapshot): harden import validation from review comments",
          "timestamp": "2026-05-20T16:48:47+01:00",
          "tree_id": "4396d36b2ab18fa23405e6f3363f9f05ee23111f",
          "url": "https://github.com/Irys-xyz/irys/commit/2953962ba54710ddb0203cdfcc0f3b27297cf3f5"
        },
        "date": 1779293170260,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012192,
            "range": "± 0.000103",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.120643,
            "range": "± 0.002185",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.203149,
            "range": "± 0.018769",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.937612,
            "range": "± 0.234207",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.074797,
            "range": "± 0.000433",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 761.96408,
            "range": "± 20.79968",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1004.674012,
            "range": "± 37.88272",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.148112,
            "range": "± 0.009616",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1335.62094,
            "range": "± 81.487067",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1872.588724,
            "range": "± 176.755392",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.032603,
            "range": "± 0.003298",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 217.320443,
            "range": "± 17.322312",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 280.870055,
            "range": "± 2.28951",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00011,
            "range": "± 0.000001",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f5f31b306a08fb8410295e680ec3c720e3331aae",
          "message": "fix: validation (#1425)\n\n* fix(preval): classify pure-internal error variants as is_internal_failure\n\nExpand PreValidationError::is_internal_failure() beyond the bootstrap\nInternalTaskJoin entry to cover every variant whose construction sites\nare unambiguously local/runtime failures:\n\n- DatabaseError — every site wraps an MDBX db.tx()/view() failure\n- PreviousTxInclusionsFailed — block_tree channel/actor failure\n- AddBlockFailed, UpdateCacheForScheduledValidationError — local\n  in-memory block-tree cache mutation\n- ValidationServiceUnreachable — local actor channel dead\n- SystemTimeError — OS clock failure\n- RewardCurveError — local arithmetic on height-derived inputs\n  (peer-supplied reward is checked separately via RewardMismatch)\n- FeeCalculationFailed — local arithmetic on config inputs (per-tx\n  comparisons use the Insufficient*Fee variants)\n- EmaSnapshotError — local snapshot computation, no real failure path\n\nThese previously routed through block_pool as `CriticalBlockPoolError::\nBlockError`, which is peer-attributed bogus data. With this change they\nroute through OtherInternal (peer is innocent) and the cached block is\nretained for retry via the path added for InternalTaskJoin.\n\nAlso delete two dead variants flagged during the audit\n(TransactionFetchFailed in PreValidationError and\nCommitmentTransactionFetchFailed in ValidationError) which have zero\nconstruction sites in the workspace.\n\nVariants still requiring source-side disambiguation before adding\n(handled in subsequent commits): BlockEmaSnapshotNotFound,\nParentEpochSnapshotNotFound, BlockBoundsLookupError.\n\n* fix(preval): rename and classify Local{Ema,Epoch}SnapshotMissing as internal\n\nBlockEmaSnapshotNotFound and ParentEpochSnapshotNotFound only fire from\none construction site each, inside data_txs_are_valid, when the parent\nblock's snapshot has fallen out of the in-memory block-tree window. This\nis a race against eviction (parent existed at prevalidation time and was\nverified there) — the block's validity is unknown locally, not bad.\n\nThe old names suggested a consensus failure (\"not found\"). Rename to\nmake the locality explicit and add doc comments capturing the race\nsemantics. Classify both as is_internal_failure so block_pool routes\nthem through OtherInternal (peer innocent) instead of BlockError.\n\nField renamed from `block_hash` to `parent_hash` so the variant body\nunambiguously identifies which block's snapshot is missing.\n\n* fix(preval): disambiguate BlockBoundsLookupError at PoA validation site\n\nBlockBoundsLookupError had three construction sites, one of which was\noverloaded: the PoA validation call to block_index.get_block_bounds\ncould fail either because the peer's offset is past the chain (a\nconsensus failure) or because of empty index / DB I/O / ledger missing\n(internal).\n\nPre-check at the call site: read the latest block_index item's\ntotal_chunks for the ledger; if the peer-supplied ledger_chunk_offset\nis past it, reject as PoAChunkOffsetOutOfBlockBounds (existing consensus\nvariant). Only after that pre-check do we call get_block_bounds, so any\nerror it surfaces from this site is now unambiguously local.\n\nThe other two construction sites (in get_assigned_ingress_proofs) feed\nblock hashes already present in our own DB cache, so failures there are\nalready pure DB consistency issues.\n\nClassify BlockBoundsLookupError as is_internal_failure now that every\nconstruction site is unambiguously local. Add a classifier test.\n\n* fix(validation): split internal failures from consensus rejections via From\n\nPreviously, every Err returned from the concurrent validation tasks\n(data_txs_are_valid, poa_is_valid, etc.) was wrapped into\nValidationResult::Invalid by hand at the call site. That meant a local\nrace — e.g. block_tree evicting the parent's snapshot mid-validation,\nsurfacing as PreValidationError::LocalEmaSnapshotMissing — would mark\nthe block as consensus-invalid and BlockTreeService would discard it.\n\nMake ValidationResult variant selection a single From impl:\n\n- ValidationError gains is_internal_failure(), delegating to\n  PreValidationError::is_internal_failure() for the PreValidation\n  variant and additionally flagging TaskPanicked, ValidationCancelled,\n  ParentCommitmentSnapshotMissing, ParentEpochSnapshotMissing,\n  ParentBlockMissing.\n- New ValidationResult::InternalFailure(ValidationError) variant.\n- From<ValidationError> for ValidationResult dispatches on the\n  classifier; From<PreValidationError> chains through.\n- ValidationResult::metric_label() returns \"valid\"/\"invalid\"/\"internal_error\".\n\nEvery call site in block_validation_task.rs that built a\nValidationResult::Invalid is refactored to use .into() so the dispatch\nis automatic. Concurrent-stage fan-in prefers an explicit Invalid over\nan InternalFailure (consensus rejection is the stronger signal).\n\nBlockTreeService::on_block_validation_finished handles InternalFailure\nexplicitly: log clearly, keep the block in cache, emit BlockStateUpdated\nwith discarded=false. The block can be re-validated on next gossip or\nwhen the underlying cause clears.\n\nTests cover both From paths and the metric_label dispatch.\n\n* fix(validation): structurally seal ValidationResult::InternalFailure payload\n\nValidationResult::InternalFailure previously accepted any ValidationError,\nrelying on a SAFETY-CRITICAL doc comment and the From impl to enforce the\ninvariant that only locally-classified errors live inside it. Nothing\nprevented manually writing\nValidationResult::InternalFailure(ValidationError::ShadowTransactionInvalid(_))\n— which would tell BlockTreeService \"this consensus failure is just an\ninternal blip, keep the block in cache and retry\" and is exactly the\nmis-classification the rest of this branch is designed to prevent.\n\nReplace the variant payload with a sealed wrapper `InternalFailureError`\nwhose only field is private and whose only construction path is the\nexisting `From<ValidationError> for ValidationResult` impl (which checks\n`is_internal_failure()` before wrapping). Direct construction outside\nthe module is now a compile error. Consumers read the underlying error\nvia `inner.err()` and the wrapper Displays through to the inner.\n\nUpdates: 7 consumer sites in block_validation_task.rs (sub-variant\nmatching for metric labels + fan-in clone), 1 in block_tree_service.rs\n(uses Display directly). New test confirming `err()` exposes the\nexpected inner.\n\nAlso clean up a doc-string imprecision flagged in review:\nValidationError::is_internal_failure delegates to (not \"mirrors\")\nPreValidationError::is_internal_failure for the PreValidation arm.\n\n* fix(validation): sub-classify ValidationCancelled reasons for correct dispatch\n\nThe previous commit on this branch reclassified ValidationCancelled as\nis_internal_failure() == true, lumping every cancellation into\nValidationResult::InternalFailure. That broke\nheavy_block_validation_discards_a_block_if_its_too_old: the new\nInternalFailure path deliberately keeps the block in cache and emits\ndiscarded=false, but cancellations driven by \"the chain has moved on\"\n(height_diff, or parent evicted mid parent-wait) are semantically\n\"give up, discard\" — retry can never succeed.\n\nIntroduce ValidationCancelReason {HeightDifference, ParentMissing,\nChannelClosed} and replace ValidationCancelled's `reason: String` with\nit. ValidationError::is_internal_failure delegates to\nValidationCancelReason::is_internal — today every reason is\nnot-internal (the wrapper routes back to Invalid → discard, matching\npre-branch behavior), but the sub-classifier is in place so a future\nreason (transient I/O race during a stage handoff, etc.) can opt into\nretry-on-cache without churning the existing sites.\n\nCancellation-time ParentMissing is intentionally NOT classified the\nsame as prevalidation-time ValidationError::ParentBlockMissing:\n\n  - Prevalidation parent-missing fires before the parent-wait stage;\n    parent could have been there moments ago and got evicted in a\n    race. Validity unknown, retry plausible → internal.\n  - Cancellation parent-missing only fires *after* prevalidation\n    observed the parent. The parent disappearing mid-wait implies the\n    cache pruned past it (tip advanced beyond block_tree_depth), so\n    the block can no longer become canonical → discard.\n\nDoc comments at each of {ValidationCancelReason, its ParentMissing\nvariant, ValidationError::ParentBlockMissing, is_internal_failure's\nparent-snapshot arm, and the parent_chain_state_check construction\nsite} cross-reference the distinction.\n\nPlumbing: ParentValidationResult::Cancelled now carries the reason;\nexit_if_block_is_too_old returns the appropriate variant at each of\nits three return points (height_diff, parent_missing, channel_closed)\nand the parent-wait closure threads it through. The metric-label\nmatch restores the explicit \"cancelled\" sub-label on the Invalid arm\n(previously moved to InternalFailure).\n\nTests added for the reason-level classifier and the From dispatch.\n\n* chore: fmt\n\n* feat: address feedback\n\n* fix: use correct hardfork activation check logic\n\n* fix(validation): panic on node-fault internal failures\n\nAdds is_node_fault() as a strict subset of is_internal_failure() and\npanics immediately when validation hits a genuine node fault (verifier\npanic, MDBX I/O, local arithmetic bug, poisoned lock, internal channel\ndead, OS clock failure). Soft eviction races (LocalEmaSnapshotMissing,\nLocalEpochSnapshotMissing, ParentNotInCache, parent-snapshot/parent-block\nmissing) stay on passive-recovery — they're recoverable via depth-prune\nplus re-gossip.\n\nThe never-mislabel rule applies: a node fault tells us nothing about\nblock validity, so any consensus verdict would risk forking off the\nnetwork. setup_panic_hook converts the panic to SIGINT, arms the\nforce-abort watchdog, and the supervisor restarts the node clean.\nSame precedent as the VDF stall watchdog in validation_service.\n\nWired at on_block_validation_finished (block_tree_service) and the\nblock_pool prevalidation-result branch. The existing is_fatal_corruption\ngraceful path for CachePoisoned (FatalCacheCorruption gossip handler)\nis preserved.\n\n* fix(validation): address review feedback on shadow_tx, broadcast lag, cascade_active\n\n- shadow_tx_task no longer mis-classifies infra/eviction failures as\n  consensus rejection. Parent commitment-snapshot eviction now surfaces\n  the typed `ParentCommitmentSnapshotMissing` (already internal); a\n  missing reth payload becomes the new `ExecutionPayloadUnavailable`\n  variant classified as `is_node_fault` so the handler aborts +\n  supervisor restart instead of peer-attributing. `shadow_transactions_are_valid`\n  takes `execution_data: &ExecutionData` so the caller controls fetch\n  classification.\n- `wait_for_parent_validation` splits `broadcast::recv` errors: `Lagged`\n  re-polls (the loop re-reads tree state on the next iteration), only\n  `Closed` cancels. Previously the wildcard collapsed both, which under\n  load could discard a valid block as `Invalid`.\n- `data_txs_are_valid` drops the `cascade_active: bool` parameter and\n  derives it from the parent epoch snapshot it already fetches —\n  single source of truth.\n- `block_pool::is_block_processing_or_processed` simplifies\n  `is_processed() || is_in_tree()` to `is_in_tree()` (strict superset).\n\n* refactor(validation): lift parent snapshot reads to caller; flag fork-determinism gap\n\n- data_txs_are_valid now takes parent_epoch_snapshot + parent_ema_snapshot\n  as parameters instead of fetching them from block_tree internally. The\n  caller (block_validation_task) reads both under a single block_tree.read()\n  in the outer scope, giving one lock acquisition and one eviction-race\n  classification site instead of three.\n- Each async-move task that needs the snapshots clones the Arc up-front\n  (cheap atomic bump). Removes duplicate snapshot reads across shadow_tx,\n  data_txs, and the dropped outer-scope cascade_active computation.\n- Tests can now construct Arc<EpochSnapshot> / Arc<EmaSnapshot> directly\n  without populating a block-tree.\n- Document the fork-determinism gap in get_assigned_ingress_proofs: the\n  CachedDataRoots-driven walk pulls block hashes across all observed\n  forks, which biases the slot-intersection check away from pure\n  parent-deterministic validation. Pre-existing behavior — tracked as a\n  separate design pass.\n\n* chore: fix typos flagged by typos lint\n\n`mis-classified` / `mis-bucketed` → `misclassified` / `misbucketed` to\nmatch the repo convention (no hyphen, matches `misclassification`\nelsewhere) and satisfy the `typos` allow-list.\n\n* fix(validation): type shadow-tx + reth submission, anchor PoA on parent\n\nCloses the remaining error-classification leaks in the validation\npipeline that the broader disambiguation work hadn't touched, plus a\nsmall fork-determinism guard on PoA pre-validation.\n\nshadow_transactions_are_valid + generate_expected_shadow_transactions\nnow return typed `ValidationError` instead of `eyre::Result`. Local\nDB/mempool/snapshot failures inside shadow-tx generation surface as\n`ShadowTxGenerationFailed` (new variant, classified `is_internal_failure`\nbut not `is_node_fault`); parent lookup races surface as the existing\n`ParentBlockMissing`; payload-structure mismatches and the actual-vs-\nexpected comparison stay as `ShadowTransactionInvalid`. Previously\nevery error path here was stringified into `ShadowTransactionInvalid`\nand routed to `Invalid` → discard, which peer-attributed the block on\nany local hiccup. The new typing routes each path to the correct\ndispatch (InternalFailure for local, Invalid for consensus).\n\nsubmit_payload_to_reth returns a new `SubmitPayloadError` enum that\nsplits local engine-RPC transport failures (LocalTransport) from\ngenuine consensus rejections (PayloadRejected/PayloadStructure). The\ncaller maps LocalTransport to the new\n`ValidationError::ExecutionLayerTransportFailed`, which is classified\nas both `is_internal_failure` and `is_node_fault` so a broken local EL\naborts and restarts cleanly instead of bucketing a transient reth\nhiccup as a consensus rejection.\n\nThe six-stage validation merger in `validate_block` now includes the\nshadow_tx outcome. Previously a shadow_tx Err short-circuited the join,\nso when shadow_tx returned InternalFailure while another stage already\nreturned Invalid the block lingered in ValidationScheduled until prune.\nShadow_tx is now folded into the merger and Invalid-over-Internal\npreference applies uniformly; the reth submission still fires only\nwhen all six stages are Valid.\n\npoa_is_valid now takes `parent_height: u64` and uses\n`index.get_item(parent_height)` instead of `get_latest_item()` for the\nPoA chunk-offset / ledger-active pre-check. The previous use of the\nlocal tip's latest indexed item meant two honest peers near a sync\nboundary could disagree on `PoAChunkOffsetOutOfBlockBounds` /\n`PoALedgerInactive` purely based on which had migrated further. The\nparent anchor is fork-deterministic; if the parent isn't yet in the\nindex (still in the block tree window) the pre-check is skipped and\nthe lookup falls through to `get_block_bounds`, matching prior\nbehaviour for that case.\n\nA residual fork-determinism gap in get_assigned_ingress_proofs\n(`CachedDataRoots.block_set` is fork-spanning) is documented but\naddressed on a separate branch — the fix touches the publish-proof\nassignment path and warrants its own review surface.\n\n* fix(validation): address P1 review findings on error classification\n\nSix follow-up fixes from the post-review audit, all targeting residual\ngaps in the internal-failure vs consensus-rejection split:\n\n- shadow_tx_task: drop the unconditional map_err that was re-wrapping\n  typed internal-failure variants (ParentBlockMissing,\n  ShadowTxGenerationFailed, ParentEpochSnapshotMissing) as\n  ShadowTransactionInvalid. The typed ValidationError now propagates to\n  the outer .into() dispatcher unchanged.\n\n- is_parent_ready: tighten from Validated(_) to Validated(ValidBlock)\n  only. Matches the InTreePendingValidation boundary in\n  block_status_provider and prevents premature parent-ready signal for\n  locally-produced or mid-validation parents in Unknown/ValidationScheduled.\n\n- send_validation_result: recover the unsent payload on channel send\n  failure; if the payload is_node_fault(), panic locally so the\n  setup_panic_hook SIGINT path still fires. A dropped block_tree channel\n  can no longer mask a node fault into silent continuation.\n\n- ShadowTxNodeFault: new ValidationError variant classified as both\n  is_node_fault and is_internal_failure. Reclassify the parent-header\n  DB lookup in generate_expected_shadow_transactions from soft\n  ShadowTxGenerationFailed to ShadowTxNodeFault so a corrupt MDBX read\n  at this site triggers panic+restart instead of accumulating blocks in\n  cache.\n\n- PoA parent-anchored bounds: new BlockIndex::get_block_bounds_at_height\n  and new PreValidationError::ParentNotIndexedYet variant replace the\n  latest-tip fallback in poa_is_valid. Recent-parent PoA bounds lookups\n  are now fork-deterministic regardless of how far the local index has\n  advanced.\n\n- PoA spawn_blocking abort: plumb an Arc<OnceLock<AbortHandle>> from\n  execute_concurrent through validate_block so the outer select cancel\n  arm can abort() the blocking PoA task. Previously a cancelled\n  validation detached the JoinHandle, silently swallowing any PoA panic\n  that would otherwise route to TaskPanicked -> node-fault -> SIGINT.\n\n* test(validation): cover P1 fixes; fix block_bounds edge cases\n\nAdds focused unit tests for the just-landed P1 fixes and surfaces two\nrelated bugs in the binary-search bounds lookup that the new tests\nexposed; production fix to get_block_bounds(_at_height) included so the\ntests pass.\n\nTests:\n- is_parent_ready: 7-case rstest covering every (Validated|NotOnchain)\n  BlockState combination, locking in that Unknown/ValidationScheduled\n  must wait. Lifted is_parent_ready to a free function (no &self\n  dependency) so it's testable without a full BlockValidationTask.\n- ShadowTxNodeFault round-trip: is_node_fault + is_internal_failure +\n  .into() dispatch + inner-error preservation; contrast case for\n  ShadowTxGenerationFailed proves the soft/hard split is intact.\n- send_validation_result: 3 tests covering the panic-on-node-fault\n  delivery failure path. Extracted the body into a free helper\n  send_validation_result_via(&Sender, ...) so a controlled channel can\n  drive the failure path without building a full ValidationServiceInner.\n- get_block_bounds_at_height: 5 tests covering happy-path equivalence\n  with get_block_bounds, anchor-below-latest restriction, and the two\n  edge cases (ledger introduced post-genesis, genesis offset) that the\n  prior review flagged.\n\nProduction fix:\n- get_block_bounds and get_block_bounds_at_height: the binary search\n  errored when probing a height where the target ledger didn't yet\n  exist (introduced post-genesis), and the prev_total lookup errored\n  at genesis (height-0 case where saturating_sub(1) collapses prev\n  onto found) and on the first block introducing a ledger. Treat\n  missing-ledger as total_chunks=0 in the search (moves right) and\n  short-circuit prev_total to 0 at height 0 / missing predecessor.\n  Documents the genesis short-circuit in comments. Same fix in both\n  functions to keep them in agreement.\n\n* fix(validation): merger preserves node-fault InternalFailure over Invalid\n\nExtract the concurrent-stage merger into `merge_stage_results` and apply\na three-tier priority order so a node-fault `InternalFailure`\n(`TaskPanicked`, `ExecutionPayloadUnavailable`,\n`ExecutionLayerTransportFailed`, `ShadowTxNodeFault`) wins over a sibling\nstage's `Invalid`. Previously the first `Invalid` was returned and the\nnode fault was discarded, silently breaking the panic+SIGINT invariant\nin `is_node_fault()` exactly when supervisor restart matters most.\n\nPriority is now:\n1. Node-fault `InternalFailure` (restart the node)\n2. `Invalid` (consensus rejection, peer attribution)\n3. Soft `InternalFailure` (eviction race, park in cache for retry)\n4. Defensive `Other(\"consensus validation failed\")` fallback\n\nAdds rstest-driven unit tests covering both orderings (node-fault first\nvs Invalid first), the soft-internal-vs-Invalid case, the two-Invalids\nregression case, soft-only, and the all-Valid defensive fallback.\n\n* fix(validation): pre-spawn snapshot fetch so early-return cannot detach PoA JoinHandle\n\n* fix(validation): propagate parent InternalFailure into child cancellation reason\n\nWhen the parent stalls in ValidationScheduled due to a local InternalFailure\n(soft eviction race, transient I/O hiccup, etc.), the child's parent-wait\nloop trips HeightDifference or ChannelClosed and routes through\nValidationCancelReason -> is_internal() == false -> Invalid, discarding the\nchild as consensus-bad. The only root cause was a local cascade — the child\nitself has no peer-attributable defect.\n\nTrack the parent's last observed ValidationResult inside exit_if_block_is_too_old\nand add a new ValidationCancelReason::ParentInternalFailure (is_internal() = true).\nA new cancel_reason_for_parent_state helper upgrades the cancel reason at the\nHeightDifference and ChannelClosed sites IFF the parent's last result was\nInternalFailure. ParentMissing keeps the discard outcome — by the time the\nparent is pruned from the tree, the chain has advanced past block_tree_depth\nand retry can no longer help.\n\nTests:\n- block_validation::prevalidation_error_classification_tests:\n  - validation_cancel_reason_classifier_dispatch (extended) covers the new\n    variant's is_internal() and is_internal_failure() classification.\n  - validation_cancel_reason_roundtrip_through_dispatcher (new) asserts the\n    From<ValidationError> for ValidationResult dispatcher lands on\n    InternalFailure for ParentInternalFailure and Invalid for the others.\n  - validation_error_node_fault_dispatch (extended) covers !is_node_fault()\n    for ParentInternalFailure (soft cascade, not a node fault).\n- validation_service::block_validation_task::cancel_reason_for_parent_state_tests\n  (new module, 8 rstest cases) covers the upgrade matrix across both base\n  reasons (HeightDifference, ChannelClosed) and all parent-last-result kinds\n  (Valid, Invalid, InternalFailure, None).\n\n* test(validation): reproduce data-PoA ParentNotIndexedYet at tip\n\n* fix(validation): block_tree fallback in poa_is_valid for un-migrated parents\n\nData-PoA validation at the tip previously stalled because block_index\nonly contains migrated blocks; parent H-1 is not indexed until canonical\ntip reaches H + block_migration_depth. poa_is_valid now falls back to\nblock_tree when the parent is not yet indexed, walking the parent chain\nto find the block that introduced the chunk; older history continues to\nuse block_index's binary search.\n\n* refactor(validation): tighten poa_is_valid fallback descend invariants\n\nRestructure the parent-chain walk in get_data_poa_bounds_with_block_tree_fallback\nso the descend step reuses the predecessor's ledger entry rather than re-looking\nit up with defensive defaults. The defaults were dead in well-formed execution\nbut would have silently surfaced as MerkleProofInvalid (peer attribution) if\never reached — violating the never-mislabel rule. Pure internal cleanup; no\nAPI change.\n\nAddresses code-review findings on commit 6a3e522ab.\n\n* fix(p2p): include PartOfAPrunedFork in is_block_processing_or_processed\n\n`is_in_tree()` covers `InTreePendingValidation | ProcessedButCanBeReorganized\n| Finalized` and intentionally gained `InTreePendingValidation` for the\norphan-cascade fix — but the switch from `is_processed()` silently dropped\n`PartOfAPrunedFork`. Callers (`gossip_data_handler.rs:507,602`, `chain_sync.rs:1412`)\nwere re-entering `process_block` for stale-fork tips peers keep advertising\nevery gossip cycle. Not a soundness bug (the block re-classifies as pruned)\nbut wasted bandwidth + CPU.\n\nRestores coverage as `is_in_tree() || is_a_part_of_pruned_fork()` — the full\n\"already-known locally\" predicate. Comment rewritten to describe the union\nexplicitly (NotProcessed is now the only false case).\n\nAdds regression coverage for all four BlockStatus variants that should\nshort-circuit gossip re-entry plus the NotProcessed must-proceed case.\n\n* fix(validation): split BlockBoundsLookupError to spare pruned side-fork lookups\n\n`get_assigned_ingress_proofs` walks `CachedDataRoots.block_set`, which is\nexplicitly fork-spanning (see the function's own doc) — it accumulates\nevery block hash referencing a given `data_root` across all observed forks.\nWhen a side-fork block in the set is later pruned from both `block_tree`\nand the database, `get_ledger_range` returns `Ok(None)` or `Err(_)`, both\nof which were classified as `BlockBoundsLookupError(String)` — a\n`is_node_fault = true` variant that panics + SIGINTs the node.\n\nThe doc-comment justification (\"the get_assigned_ingress_proofs sites only\noperate on block hashes already present in our DB\") was wrong: `block_set`\nis fork-spanning by design.\n\nSplits the variant:\n- `BlockBoundsLookupError(String)` (unchanged classification: node fault).\n  Used by the PoA-anchored bounds binary search, where peer-supplied offsets\n  and ledger ids are pre-filtered before this variant can fire.\n- `AssignedProofBlockMissing { block_hash, tx_id }` (new, soft internal,\n  NOT node fault). Both `Ok(None)` and `Err(_)` arms in\n  `get_assigned_ingress_proofs` route here — same root cause: a fork-\n  spanning hash no longer resolvable.\n\nDoc-comment rewritten to describe both variants' rationale separately.\n\nAdds:\n- Classification round-trip test for the new soft variant\n- Regression test confirming the PoA-anchored variant retains node-fault\n  classification (guards against accidental \"soften everything\")\n- `From<PreValidationError> for ValidationResult` dispatch test confirming\n  the new variant routes to `InternalFailure` (block parks in cache,\n  `send_validation_result` panic-guard does not fire)\n\n* fix(validation): route wait_for_payload cache eviction as soft internal failure\n\n`ExecutionPayloadCache::wait_for_payload` previously returned `Option`, and\nthe only path producing `None` was the `payload_senders` LRU evicting our\noneshot sender (capacity `PAYLOAD_RECEIVERS_CAPACITY = 1000`) or an explicit\n`remove_payload_from_cache`. Under heavy catch-up sync — >1000 concurrent\npayload waits in flight — the eviction surfaces in\n`block_validation_task::shadow_tx_task` as `None`, which was mapped to\n`ValidationError::ExecutionPayloadUnavailable` (a `is_node_fault = true`\nvariant). Net effect: a healthy node self-DoS'd via panic+SIGINT at the\nmoment it was catching up.\n\nDisambiguates the cache disruption from a real EL fault:\n\n- `wait_for_payload` / `wait_for_sealed_block` now return\n  `Result<_, ExecutionPayloadWaitError>` (new error type with a single\n  `ReceiverDisrupted { evm_block_hash }` variant documenting that the only\n  trigger today is local cache teardown).\n- `ValidationError::ExecutionPayloadUnavailable` is removed (no construction\n  sites remain; `ExecutionLayerTransportFailed` already exists for genuine\n  local-EL RPC transport failures and is unchanged).\n- New `ValidationError::ExecutionPayloadCacheEvicted { evm_block_hash }`\n  with `is_internal_failure = true`, `is_node_fault = false`. Block parks\n  in cache for retry; gossip re-entry recovers.\n- The shadow_tx_task caller maps `ReceiverDisrupted` → the new soft\n  variant. The block-pool repair-path caller maps the same error to its\n  existing `OtherInternal` bucket.\n\nAdds:\n- Domain-level regression: `wait_for_payload` returns `ReceiverDisrupted`\n  when the sender is dropped (exercises `block_receiver` +\n  `remove_payload_from_cache` for fast deterministic coverage).\n- Classification round-trip test: `ExecutionPayloadCacheEvicted` routes to\n  `InternalFailure` with the wrapped `is_node_fault() == false`, so\n  `send_validation_result`'s panic-guard does NOT fire.\n\n* fix(validation): seed parent validation result on entry to close broadcast race\n\n`exit_if_block_is_too_old` subscribes to `block_state_updates` and then\ninitializes `parent_last_validation_result = None`. `tokio::sync::broadcast`\ndoes not replay past events: if the parent's\n`BlockStateUpdated { validation_result: InternalFailure(..) }` was broadcast\nBEFORE the subscription was created — common when the child gossiped in\nafter the parent stalled, OR on re-entry to the second wait at\n`block_validation_task.rs:223-225` (a fresh subscriber is created each time)\n— the child never observes the parent's InternalFailure. A subsequent\nheight-diff cancel then returns `HeightDifference` and the child is\nmisattributed as `Invalid` rather than the correct `ParentInternalFailure`\ncascade.\n\nThe prior fix (`d28287b9e`) only caught the case where the child was\nalready in the wait loop when the parent failed — the minority case.\n\nAdds a bounded `LruCache<BlockHash, ValidationResult>` (capacity 1024)\nto `ServiceSenders` that records the most-recent validation result per\nblock. Every `BlockStateUpdated` construction site in\n`block_tree_service.rs` now calls `record_validation_result` BEFORE the\nbroadcast send so a subscriber waking from `recv()` can read it\nsynchronously. `exit_if_block_is_too_old` subscribes FIRST, then seeds\n`parent_last_validation_result` from the store — making it impossible\nto miss the parent's last result regardless of relative timing:\n\n- Parent event before child subscribe → not delivered via `recv()` but\n  the store-write already landed; seed picks it up.\n- Parent event after child subscribe → delivered via `recv()`; loop\n  body updates `parent_last_validation_result`.\n- Concurrent → at least one path delivers.\n\nLock is a leaf `std::sync::RwLock` (no other locks held while reading\nor writing it; no `.await` across); store is purely advisory (poison\nrecovery logs and returns `None`, which can only fail to upgrade a\ncancel, never produce a wrong answer).\n\nTests cover:\n- Store round-trip, LRU eviction bound, overwrite-on-same-key\n- Parent-event-via-recv path (round-2 baseline still works)\n- Parent-event-BEFORE-child-subscribe → cancel upgrades to\n  ParentInternalFailure (the bug closed by this commit)\n- Second-wait re-entry with fresh subscriber seeded from store\n\n* fix(validation): remove soft-failed blocks and reclassify ParentMissing as internal\n\nThe soft-internal handler used to leave the block parked in ValidationScheduled,\nrelying on depth-prune to eventually evict it. But depth-prune only fires when\nthe canonical tip changes — and if the failing block IS the tip candidate, no\ntip change is possible. The canonical chain wedges until restart.\n\nMirror the Invalid path: call cache.remove_block on soft failure too (recursive,\nso in-tree children parked on this parent are swept along). Recovery is via\npeers re-gossiping the block; gossip provides the natural rate-limit against\ntight loops if the local race keeps re-occurring.\n\nParentMissing flips to is_internal() = true. The previous discard-and-blame-peer\nbehavior assumed \"parent absent ⇒ chain moved on past block_tree_depth\", which\nno longer holds once the soft handler can proactively remove parents. Parent\nabsence is never peer-attributable anyway: block_pool failed to gate, depth-\nprune, or the soft handler — all local.\n\n* fix(validation): route get_ledger_range DB errors as node-fault BlockBoundsLookupError\n\nea32b61c added AssignedProofBlockMissing as a soft sibling of BlockBoundsLookupError\nfor the fork-spanning CachedDataRoots.block_set walk, and routed both Ok(None) and\nErr(_) from get_ledger_range to it. The commit message argued they \"describe the\nsame root cause: a no-longer-resolvable side-fork hash\" — but that's only true for\nOk(None).\n\nget_ledger_range's Err arms cover real local faults:\n  - get_block_by_hash propagates MDBX read errors\n  - explicit eyre!(\"...data corruption\") when block_total_chunks < prev_total_chunks\n\nBoth are local-state breakage that the branch's node-fault policy says must panic\n+ supervisor restart, not silently park the block. Narrow the catch so only\nOk(None) → AssignedProofBlockMissing; Err(_) → BlockBoundsLookupError (already\nnode-fault).\n\n* refactor(validation): drop dead recent_validation_result machinery\n\nWith soft-failed blocks now removed from the cache (and ParentMissing routing\nthrough is_internal() = true), the replay buffer + cascade-detection upgrade\nthat 5b2aae5f added has no surface to fire on:\n\n  - Parent fails soft → parent removed → in-tree children swept recursively.\n  - Late-arriving children hit ParentMissing in the wait loop, which is\n    already soft.\n  - There is no window where \"parent stalled in ValidationScheduled with\n    InternalFailure\" survives long enough for a child to time out waiting\n    on it, so the cancel_reason_for_parent_state upgrade has no input.\n\nRemove the now-dead surface:\n\n  - ServiceSenders::{recent_validation_result, record_validation_result}\n    and the LRU store they wrap.\n  - The four record_validation_result call sites in block_tree_service.\n  - cancel_reason_for_parent_state, parent_last_validation_result tracking,\n    and the seed-read / in-loop-update bookkeeping in exit_if_block_is_too_old.\n  - ValidationCancelReason::ParentInternalFailure (now unreachable; the only\n    constructor was the upgrade helper).\n  - All tests covering the removed surface (~15 tests).\n\n* refactor(validation): unify error classification via single classify()\n\nReplace four parallel exhaustive matches (`is_node_fault` and\n`is_internal_failure` on `PreValidationError` and `ValidationError`)\nwith a single `classify() -> ErrorClass { NodeFault | SoftInternal |\nConsensus }` per enum plus two-line wrapper predicates. No `_`\nwildcards — adding a new variant without classifying it is now a\ncompile error, replacing the prior doc-comment-only safety rule.\n\nDelete three dead variants surfaced by the cleanup:\n- `PreValidationError::ParentNotIndexedYet`: the data-PoA path now\n  falls back to `block_tree` for the un-migrated window, so this\n  variant has zero production construction sites. Kept \"for future\n  fallback failures\" — exactly the speculative pre-abstraction the\n  project guidance prohibits.\n- `PreValidationError::LocalEpochSnapshotMissing`: only ever\n  constructed in tests; the real epoch-missing site at\n  `block_validation_task.rs:469` already used\n  `ValidationError::ParentEpochSnapshotMissing`.\n- `PreValidationError::LocalEmaSnapshotMissing`: migrated to the new\n  sibling `ValidationError::ParentEmaSnapshotMissing` to mirror the\n  epoch path, resolving a design inconsistency where the two parallel\n  parent-snapshot races at `block_validation_task.rs:469/482` returned\n  different variant families.\n\nStale comments in `chain-tests/src/validation/poa_cases.rs` referencing\n`ParentNotIndexedYet` rewritten to describe the block_tree fallback\nthat replaced it.\n\nAll variant classifications preserved (cross-checked against the prior\nmatches). Test coverage unchanged: round-trip tests still pass via the\nwrapper predicates.\n\n* refactor(validation): mechanical cleanups from branch review\n\nPure-mechanical simplifications surfaced by the review pass — no\nbehaviour changes. Net -146 LOC across the bundle (311 insertions,\n457 deletions).\n\nblock_index.rs (-80 LOC): collapse get_block_bounds into a 5-line\nwrapper over get_block_bounds_at_height. The two had near-identical\nbinary-search + prev_total + found-ledger logic; only the empty-index\nprecheck differs and now lives in the wrapper. Wrapper reads\nblock_index_latest_height directly via db.view_eyre, preserving DB-\nerror semantics (self.latest_height silently maps None to 0).\n\nblock_pool.rs / block_validation.rs (D5, net -4 LOC): replace the\n4-tier predicate ladder (is_fatal_corruption / is_node_fault /\nParentNotInCache / is_internal_failure) with two if-let short-circuits\n(CachePoisoned → graceful shutdown, ParentNotInCache → orphan path)\nabove a match on pre_err.classify() with three exhaustive arms. The\nclassification table stays single-sourced in block_validation.rs;\nadding a new variant cannot drift between dispatch sites. Deletes\nis_fatal_corruption() and its dedicated tests — the dispatch ladder\nwas its only production caller.\n\nblock_tree_service.rs (D4, -13 LOC): extract discard_and_broadcast\nhelper from on_block_validation_finished. The soft-InternalFailure\nand Invalid arms shared cache write-lock → height/state lookup →\nrecursive remove_block → BlockStateUpdated broadcast. Per-arm log\nwording and diagnostic-record format selected via a private\nDiscardKind enum. is_node_fault() panic stays inline above the call.\n\nblock_tree_service.rs (B2, B3): fix stale \"DO NOT remove\" intro\ncomment to state the actually-universal invariant (never mark Invalid).\nDocument the BlockStateUpdated.state field's pre-removal staleness on\ndiscarded paths and name its sole reader (a tracing::info! in\nvdf_validation_progress.rs:269 that logs it alongside discarded).\n\nblock_pool tests (D8, -20 LOC): compress 5 is_block_processing_or_\nprocessed_* tests into one #[rstest] over (setup_fn, expected_status,\nexpected_predicate). 5 cases → 5 parameterized cases.\n\nblock_status_provider tests (D9, -28 LOC): compress 3 block_status_\nreturns_in_tree_pending_validation_* tests into one #[rstest] over\nChainState inputs.\n\n* refactor(validation): seal ValidationResult::Invalid payload\n\n`ValidationResult::InternalFailure` was already sealed via the\n`InternalFailureError` newtype, only constructible inside this module\nthrough the `From<ValidationError>` dispatcher. The sibling `Invalid`\nvariant carried a bare `ValidationError`, so anyone with enum access\ncould construct `Invalid(ValidationError::TaskPanicked { ... })` —\npeer-attributing a local fault. The invariant held by convention\n(every site routed via `.into()`), not by types.\n\nMirror the seal:\n\n- New `ConsensusRejectionError(crate::block_validation::ValidationError)`\n  newtype, derives + accessors identical to `InternalFailureError`:\n  private inner field, `err() -> &ValidationError` accessor, `Display`\n  delegates to the inner error.\n- `ValidationResult::Invalid(ConsensusRejectionError)`. Both payloads\n  are now sealed wrappers; constructing either with a misclassified\n  variant is structurally impossible outside this module.\n- The `From<ValidationError> for ValidationResult` dispatcher is the\n  only path that constructs either wrapper.\n\nMigrated direct construction sites and pattern matches:\n\n- `block_validation_task.rs`: test fixture `invalid()` now uses\n  `.into()` instead of `Invalid(...)`. Three nested matches\n  (`Invalid(ValidationError::ValidationCancelled { .. })`,\n  `Invalid(ValidationError::Other(_))`, label dispatch) flatten to\n  `Invalid(rejection) if matches!(rejection.err(), ...)`.\n- `block_validation.rs`: two `Invalid(ValidationError::SeedDataInvalid(_))`\n  test matchers flatten the same way.\n- `chain-tests/.../utils.rs`: extract inner via `.err().clone()` when\n  boxing into `BlockValidationOutcome::Discarded`.\n- `chain-tests/.../vdf_validation_progress.rs`: rename bound var, route\n  `to_string` through the wrapper, switch nested match to accessor.\n\nNo backwards-compat shims: the only public read path is `.err()`. The\n`From<ValidationError>` impl handles every site that needs to lift a\nclassified error into the result type.\n\n* fix(tests): chain-tests catch up to typed-variant + internal-failure dispatch\n\nTwo chain-tests were failing on this branch but passing on master. Bisect\nfound the root cause is `761fb22f6` (\"fix(validation): address P1 review\nfindings on error classification\") — both failures are tests that were\nwritten against the old fuzzy variant landscape and never updated when\ntyped dispatch landed. The blocks are still correctly rejected; only the\ntest assertions and harness needed to catch up.\n\nread_block_from_state: extract validation error from BOTH dispatch arms.\n`Invalid` carries consensus rejections; `InternalFailure` carries local\nruntime issues including cancel reasons like `ParentMissing` that were\nreclassified `is_internal()=true` in `216666206`. The harness only\ncaught `Invalid` events, so tests where the block is discarded via the\n`InternalFailure` path timed out after 50s with a misleading\n`Other(\"Timeout waiting for block validation\")` outcome. Both arms now\nsurface to tests as `BlockValidationOutcome::Discarded(error)`.\n\n  Fixes:\n  - validation::heavy_block_validation_discards_a_block_if_its_too_old\n    (block discarded via `ValidationCancelled { reason: ParentMissing }`\n    which routes through InternalFailure since 216666206)\n\nheavy_block_insufficient_perm_fee_gets_rejected: update assertion to\nmatch the typed `PreValidation(InsufficientPermFee { .. })` variant.\nBefore 761fb22f6, an unconditional `map_err` in `shadow_tx_task` was\nre-wrapping every error from `shadow_transactions_are_valid` as the\ncatch-all `ShadowTransactionInvalid(e.to_string())` — peer-attributing\nlocal DB/snapshot failures. Removing the catch-all is a real consensus-\nsafety fix; the test had been relying on the fuzzy wrapping rather than\nthe underlying typed error.\n\n  Fixes:\n  - validation::data_tx_pricing::heavy_block_insufficient_perm_fee_gets_rejected\n\nVerified: full `cargo nextest run -p irys-chain-tests validation` and\nmulti_node::validation subsets pass (86/86). The other 12 sites using\n`matches!(e, ValidationError::ShadowTransactionInvalid(_))` still match\ncorrectly — they hit explicit `reject(...)` calls inside\n`shadow_transactions_are_valid` for payload-structure rejections\n(EIP-4844 blobs, EIP-7685 requests, withdrawals, timestamp mismatch,\nshadow-tx-match failures via `validate_shadow_transactions_match`).\n\n* chore: fmt\n\n* fix(validation): address round-4 multi-reviewer findings\n\n- block_validation: bounds-fallback walk distinguishes invariant-violation\n  `None` from `block_index.get_item(curr_height-1)` (now routes to\n  `BlockBoundsLookupError` node-fault) from a legitimate missing ledger\n  entry (preserved as `prev_total = 0`). The two were collapsed via\n  `.unwrap_or(0)`, silently producing a consensus-valid `BlockBounds`\n  rooted at offset 0 on a corrupted node.\n- shadow_tx_generator: publish-ledger / perm_fee_refund invariant is\n  now enforced at the validation call site\n  (`generate_expected_shadow_transactions`), routing violations through\n  `ShadowTransactionInvalid` (peer-attributable consensus rejection)\n  rather than soft `ShadowTxGenerationFailed`. The constructor keeps\n  an identical guard as defence-in-depth for non-validation callers\n  (block_producer + tests).\n- validation_service: concurrent-task `JoinError::is_cancelled()` no\n  longer routes through `TaskPanicked` -> `NodeFault` -> supervisor\n  restart. Tokio runtime hiccups (sibling-task worker panic, runtime\n  shutdown racing the loop) now log, release the validation slot, and\n  leave the block in cache. Symmetric with the VDF arm's existing\n  cancelled-requeue handling.\n- block_validation: `calculate_expired_ledger_fees` errors classified\n  as `ShadowTxNodeFault` rather than `ShadowTxGenerationFailed`. Every\n  error path the helper emits is either a real MDBX I/O failure or\n  internal in-memory state inconsistency (epoch-snapshot or\n  block-index/mempool/DB triple in a broken state); both are\n  retry-futile and not peer-attributable.\n- block_validation: fast-path in `get_data_poa_bounds_with_block_tree_fallback`\n  now `debug_assert_eq!`s `parent_item.block_hash == parent_block_hash`\n  to lock down the \"reorgs past migration_depth abort the node\"\n  invariant in tests.\n- block_tree_service: shutdown drain inspects `InternalFailure` results\n  and panics on `is_node_fault()` so supervisor restart still fires for\n  faults produced during/just-before shutdown drain.\n\n* fix(validation): address round-6 multi-reviewer findings\n\n- block_validation: PoA fast-path `debug_assert_eq!` (compiled out in\n  release) replaced with a runtime parent-hash check that falls through\n  to the block_tree walk on mismatch. A peer-submitted block whose\n  `parent_block_hash` is a non-canonical sibling at a migrated\n  `parent_height` would otherwise have its PoA bounds computed against\n  the canonical anchor — wrong fork. `block_tree`'s reorg-abort backstop\n  fires later, but the PoA verdict would already have been derived from\n  a fork-mismatched view.\n- shadow_tx_generator: replace `eyre::Result` with typed\n  `ShadowTxGenError` {SnapshotInvariant, TreasuryArithmetic, Structural,\n  Soft}. Producer-side `?` lifts via eyre's blanket `From<E:\n  std::error::Error>` impl.\n- commitment_refunds: replace `bail!` with typed\n  `CommitmentRefundError::SnapshotInvariant`.\n- block_validation `generate_expected_shadow_transactions`: route each\n  `ShadowTxGenError` variant to its consensus-appropriate\n  `ValidationError`: `SnapshotInvariant` → `ShadowTxNodeFault` (node\n  fault → panic+restart), `TreasuryArithmetic`/`Structural` →\n  `ShadowTransactionInvalid` (consensus rejection),\n  `Soft` → `ShadowTxGenerationFailed` (soft retry).\n  `CommitmentRefundError::SnapshotInvariant` likewise routes to\n  `ShadowTxNodeFault`. Adds 6 unit tests covering the dispatch.\n- shadow_tx_generator: TODO documents that\n  `deduct_from_treasury_for_payout`'s snapshot-derived call sites\n  (expired-ledger / commitment-refund phases) are arguably node-fault-\n  shaped but conservatively kept as `TreasuryArithmetic` (consensus)\n  to avoid restart loops on local-state corruption.\n- execution_payload_cache: close the lost-notify window in\n  `wait_for_sealed_block` by re-checking the cache after\n  `block_receiver` registers our oneshot. On second-check hit, pop our\n  entry from `payload_senders` so repeated race wins don't leak sender\n  slots into the 1000-slot LRU and evict legitimate waiters.\n- validation_service: concurrent-task cancel arm now requeues via\n  `submit_task` instead of soft-discarding. `concurrent_tasks.abort_all()`\n  only fires from `shutdown()` (which runs after the main loop breaks),\n  so a `JoinError::Cancelled` reaching the result handler is an\n  unexpected runtime hiccup, and soft-discarding loses information\n  about a validation we never decided on. Preserves the original\n  `enqueued_at` so end-to-end latency metrics aren't double-counted.\n- block_validation_task: new `new_with_enqueued_at` constructor for the\n  requeue path so the resubmitted task carries the original gossip-to-\n  enqueue timestamp.\n- active_validations: extend `concurrent_task_blocks` value tuple to\n  carry `Arc<SealedBlock>` + `skip_vdf_validation: bool` so the cancel\n  arm can reconstruct the task.\n- block_validation_task: replace `_ => \"internal_error\"` fallback in\n  `result_label` with an exhaustive OR-chained match over every\n  `ValidationError` variant, factored through a `label_for(err,\n  default)` closure. Adding a new `ValidationError` variant now\n  produces a compile error until its label is decided.\n\n* test(data-tx-pricing): accept ShadowTransactionInvalid for insufficient perm_fee\n\nAfter round-6's typed `ShadowTxGenError::Structural` classification,\n`PublishFeeCharges::new` rejecting a peer's underfunded `perm_fee` now\nsurfaces as `ValidationError::ShadowTransactionInvalid` (consensus\nrejection) rather than the prior soft-internal stringified shape. Two\nstages can now both reject the same defect with `Invalid`:\n- `data_txs_are_valid` → `PreValidation(InsufficientPermFee)`\n- `generate_expected_shadow_transactions::ShadowTxGenerator::new` →\n  `ShadowTransactionInvalid`\n\nThey run concurrently and either may win the merge race; the prior\ntest asserted on the typed `InsufficientPermFee` shape based on an\nimplicit stage-ordering tiebreak that no longer holds.\n\nAccept either error shape: the test pins \"block IS rejected via a\nconsensus error\" rather than the incidental stage-ordering. Same\nupdate applied to both `heavy_block_insufficient_perm_fee_gets_rejected`\nand `same_block_promoted_tx_with_ema_price_change_gets_rejected`.\n\n* test(validation): rstest-ify ValidationCancelReason classifier tests\n\nThe two `for reason in [...]` table-driven tests collapsed failures\nacross variants into a single test report — when one variant\nregressed, the failure message identified only \"one of three reasons\nfailed\" rather than which one. Convert both to `#[rstest]` with one\n`#[case]` per `ValidationCancelReason` variant so per-variant failures\nsurface as distinct nextest reports.\n\nMatches the existing repo convention used by\n`is_parent_ready_chain_state_dispatch` and\n`block_status_returns_in_tree_pending_validation`.\n\nThe roundtrip test now carries a small `ExpectedRoundtripShape` enum\nin the test module to express the per-variant expected\n`ValidationResult` shape — clearer than a stringly-typed comparison\nand survives renames via the type system.\n\nSkipping the line-number-specific suggestion: the finding referenced\n`crates/actors/src/block_validation.rs:1424-1442` and `:1448-1501`,\nbut those line ranges contain production code (`last_diff_timestamp_is_valid`,\n`cumulative_difficulty_is_valid`) on this HEAD. The named tests live\nat `:1635` and `:1667`; converted those.\n\n* fix(validation): address round-7 multi-reviewer findings\n\n- shadow_tx_generator: new `SnapshotTreasuryUnderflow` variant covering\n  the snapshot-derived payout path in `deduct_from_treasury_for_payout`\n  (only ever called from `try_process_expired_ledger` /\n  `try_process_commitment_refunds`, both of which derive amounts from\n  local snapshots — not peer-supplied data). Underflow on this path is\n  a local-state inconsistency; the prior `TreasuryArithmetic` →\n  consensus-rejection classification could silently fork the node off\n  the canonical chain instead of triggering supervisor restart.\n- block_validation `classify_shadow_err`: routes\n  `SnapshotTreasuryUnderflow` → `ShadowTxNodeFault` (panic+restart) via\n  the existing typed-dispatch path. Removes the conservative-stance\n  TODO at the helper's doc-comment. Two-honest-node disagreement on\n  treasury balance is structurally impossible, so node-fault is the\n  unambiguous classification.\n- gossip_client `pull_data_from_network`: track HandshakeRequired\n  peers in their own `handshake_required_peers` collection (dedup'd\n  via `HashSet`). The post-loop one-shot retry now runs against this\n  collection regardless of whether sibling peers hit transient\n  failures — previously, any non-handshake failure flipped\n  `all_failures_were_handshake = false` and dropped handshake-required\n  peers permanently for the call. Removes the `all_failures_were_handshake`\n  and `had_any_attempts` flags; surfaces the right `last_error` if\n  the post-retry also fails.\n- gossip_client `pull_data_from_network`: filter CB-open peers before\n  the shuffle/truncate(5) candidate selection. If the filter empties\n  the working set, fall back to `candidate_pool` with a structured\n  `warn!` and a `CB_BYPASS_TRIGGERED` counter so sustained\n  all-CB-open state surfaces operationally. `CircuitBreakerOpen` is\n  no longer requeued onto `next_retryable` (CB state cannot change\n  within a single `pull_data_from_network` call, so requeue is busy\n  work that crowds out healthy peers).\n- validation_service: cancel-requeue arm gains observability — a\n  structured `warn!` plus a `concurrent_cancel_requeued_total`\n  counter so sustained `JoinError::Cancelled` outside intentional\n  shutdown is visible. Behavior unchanged; the requeue remains the\n  defensive default.\n- block_validation: new `ValidationError::metric_label()` method\n  returning a 5-way partition (`cancelled` / `panicked` /\n  `node_fault` / `internal_error` / `invalid`). New\n  `ValidationResult::granular_metric_label()` delegates through the\n  sealed `Invalid` / `InternalFailure` wrappers. All seven per-stage\n  `metrics::record_validation_result(...)` call sites (recall_range,\n  poa, shadow_tx, seeds×2, commitment_ordering, data_txs,\n  reth_submission) now produce the granular label instead of\n  collapsing every `Err` to `\"invalid\"`. The R6 `label_for` closure\n  is kept and rewritten to delegate to `metric_label()`, collapsing\n  the new granular labels back to the closure's `default` so\n  production dashboards keep their existing label vocabulary.\n- block_tree_service: `InternalFailureError` derives `PartialEq, Eq`\n  to match `ConsensusRejectionError` — both wrappers serve symmetric\n  roles in the sealed `ValidationResult` and were asymmetrically\n  derived by accident.\n\nTests added: SnapshotTreasuryUnderflow dispatch round-trip (2),\nHandshakeRequired peer retry scenarios (3 — mixed-failure regression,\nall-handshake backward-compat, retry-dedup correctness), CB-open\npeer filtering scenarios (3 — alternative-exists, all-cb-open\nbypass, no-requeue), per-stage metric_label exhaustive coverage (2).\nRequired new test scaffolding: `ScriptedMockServer` for per-request\nmock peer responses (the existing `MockHttpServer` was single-shot).\n\nVerified workspace-wide: `cargo xtask check`, `cargo clippy\n--workspace --tests --all-targets`, `cargo fmt --all`. irys-actors\n226/226 nextest pass; irys-p2p 291/291 pass.\n\nDeferred from this round with rationale (see REVIEW_R7.md): the\nPoA `spawn_blocking` panic-detachment-after-cancel residual is\nobservability-only, not correctness — `poa_is_valid` is pure CPU on\nread-only inputs with deterministic panic sources, and re-validation\non a non-cancel path will trip the same panic. The existing\npanic hook covers thread-level diagnostics.\n\n* docs(operator): require auto-restart supervisor for node process\n\nThe node is expected to crash on unrecoverable local faults (e.g. detecting\nthat it produced or accepted state disagreeing with the network) and relies\non the host to restart it. Document this in the mainnet beta deployment guide.\n\n* docs(validation): mark C1/C2 merge points for fix-cdr-block-set integration\n\nThree grep-clean markers (`MERGE-BLOCKER(C1)` / `MERGE-PAIR(C1)` /\n`MERGE-NOTE(C2)`) so the agent integrating with branch\n`fix-cdr-block-set` (4e21e25a) handles the audit findings correctly:\n\n- `MERGE-BLOCKER(C1)` on the `None =>` arm in `get_block_bounds`:\n  the height-only canonical-index fallback is NOT fork-deterministic\n  and the OOR case escalates to `BlockBoundsLookupError` ->\n  `NodeFault` panic on a peer-controllable side-fork lookup. Spells\n  out the replacement shape (mirror\n  `tx_inclusion::find_canonical_ledger_range`, parent-anchored +\n  `ChainState::Onchain`, return `Ok(None)` for off-lineage, NEVER\n  route as NodeFault).\n\n- `MERGE-PAIR(C1)` on the `BlockBoundsLookupError` variant doc:\n  the `is_node_fault` classification holds only if every construction\n  site is genuinely local. Reminder to re-audit all sites once the\n  walk-off-tree fallback is replaced.\n\n- `MERGE-NOTE(C2)` on the `AssignedProofBlockMissing` variant doc:\n  lists the cdr-block-set commits that close C2 + the H1 livelock\n  (7479e10e, ed27fada, c8e79e48, 9b64d7ab, 4e21e25a), and instructs\n  the merger to verify no construction sites remain and delete the\n  variant + classifier entries + metric tags if so.\n\nComments only; no logic change. Confirmed `cargo check -p irys-actors`\nstill passes.\n\n* fix(validation): close H2 — preserve node-fault over cancellation\n\nThe 2026-05-20 audit identified that the outer\n`futures::future::select(validate_block, wait_for_parent_validation)`\nin `execute_concurrent` could drop `validate_block` while a stage\nhad already produced a NodeFault `InternalFailure`, silently\ndemoting it to `ValidationCancelled` (which for `HeightDifference`\n/ `ChannelClosed` routes through `ValidationResult::Invalid` —\npeer-attributed). A prior investigation confirmed Option 3\n(biased select + drain-poll) cannot close the window in the\ngeneral case, because the PoA `spawn_blocking` JoinHandle becomes\nReady on a separate OS thread at arbitrary wall-clock time while\nsibling stages are still suspended on awaits; a single drain-poll\ncannot synthesise the full `merge_stage_results` output.\n\nFix: move cancellation INSIDE `validate_block` and feed it as an\nadditional input to a new `merge_stage_results_with_cancel`,\nwhose priority is `NodeFault > Invalid > Cancellation > soft\nInternalFailure`. The outer `select` collapses; the\n`poa_abort_slot` `AbortHandle` machinery is preserved so the\nblocking PoA task isn't leaked. Inside, `tokio::select! { biased; }`\nraces the six-stage `tokio::join!` against\n`exit_if_block_is_too_old`; on cancel, `futures::poll!` drains\nthe join one more time, and any stage whose result is already\nready contributes to the cancel-aware merge.\n\nCoverage: new tests in the existing `merge_stage_results_tests`\nmod — `node_fault_wins_over_cancellation` (rstest matrix for\n`HeightDifference` and `ChannelClosed`),\n`invalid_wins_over_cancellation`,\n`cancellation_wins_over_soft_internal_failure`,\n`no_cancel_matches_legacy_merger`,\n`all_valid_with_cancel_returns_cancellation`.\n\nVerified: 76/76 in `block_validation`, 232/232 in `irys-actors`.\n\nResidual notes (acceptable, called out in the audit report):\n- The PoA `spawn_blocking` thread still cannot be preempted\n  mid-computation; `.abort()` prevents detach but not interrupt.\n- The drain-poll is single-shot. NodeFaults produced inside an\n  `async` body are `Ready` immediately on construction so the\n  drain catches them; this preserves the safety invariant.\n\n* obs(validation): instrument H3 soft-internal discards + gossip recovery\n\nPhase A of the H3 fix from the 2026-05-20 audit. When validation\nsurfaces a `SoftInternal` `InternalFailure`, `discard_and_broadcast`\nrecursively removes the block from `block_tree` and `block_pool`\nhas already removed it from `blocks_cache`; recovery is \"passive\nvia fresh gossip re-entering `process_block`\". The audit flagged\nthat under thinned peer sets or \"old block\" timing, peers may\nnever re-advertise and a valid subtree is permanently lost.\n\nPhase A proves or disproves that gossip-driven recovery works in\npractice before investing in explicit re-request machinery.\n\nTwo counters (`irys.block.soft_internal_discard_total{reason}` and\n`irys.block.soft_internal_recovered_total{reason}`) bracket the\ndiscard → recovery loop. A 4096-entry `LruCache<BlockHash, &'static str>`\non `BlockTreeServiceInner` records `(block_hash, reason_tag)` on\nSoftInternal discard; the Valid arm of `on_block_validation_finished`\npops the entry, increments the recovery counter with the original\nreason tag, and emits an `info!` log noting recovery for operator\nvisibility. NodeFault panics before reaching `discard_and_broadcast`;\nInvalid (peer-attributable) is gated by `DiscardKind::SoftInternal`\n— neither touches the LRU.\n\n`soft_internal_reason_tag` matches variants directly (not via\n`metric_label()`, which collapses all SoftInternal variants to\n`\"internal_error\"`). A defensive `_` fallback (`\"internal_error_other\"`)\nensures a new SoftInternal variant landing without a tag entry\nstill surfaces in the metric.\n\nA `PHASE-B(H3)` grep-clean marker on the LRU field documents the\nfollow-up: if operational tolerance for the discard/recover ratio\nis breached, implement explicit per-block-hash re-request with\nexponential backoff at the discard site.\n\nCoverage: 12 new tests — reason-tag mapping (rstest, one case per\nvariant), LRU insert on discard, pop-and-recover on Valid, no-touch\non Invalid + node-fault, capacity-bounded eviction. 14/14 H3-related\ntests pass.\n\n* fix(validation): close H4 — bound wait_for_sealed_block with config timeout\n\nThe 2026-05-20 audit identified that `wait_for_sealed_block` ran\n`receiver.await` with no timeout after the best-effort\n`request_payload_from_the_network` (10×5s retry budget). The\nreceiver only errored when the `payload_senders` LRU evicted its\nslot (capacity 1000), so a peer advertising a block header without\nserving the EVM payload caused validation to stall until 1000\nother distinct hashes pushed the slot out — essentially unbounded\nunder low load. With this branch newly classifying\n`ExecutionPayloadCacheEvicted` as `SoftInternal`, an unbounded\nstall combined with H3 (block already removed from the pool by\nthe time validation finishes) converted transient races into\npermanent stalls.\n\nFix: wrap the wait in `tokio::time::timeout(self.wait_timeout, receiver)`\nand distinguish three outcomes — payload delivered, sender dropped\n→ `ReceiverDisrupted` (LRU eviction), deadline elapsed →\n`WaitTimeout { evm_block_hash, elapsed_ms }` (new). The timeout\npath explicitly pops the orphaned `payload_senders` slot, mirroring\nthe existing cache-hit fast-path cleanup. Both error variants map\nto `ValidationError::ExecutionPayloadCacheEvicted` so the\nSoftInternal classification is preserved (no escalation to\nNodeFault panic), and each is logged distinctly at the conversion\nsite so operators can grep / count them.\n\nConfig: `SyncConfig::execution_payload_wait_timeout_millis`\n(node-local operational concern, sits beside the existing peer-\nnetwork timeouts and follows the same `_millis: u64` convention).\nProduction default 60_000 ms — longer than the ~50 s request-side\nbudget so retries finish first. Testing default 5_000 ms.\n\nPlumbed `wait_timeout: Duration` through `ExecutionPayloadCache::new`\nrather than threading the full `Config` through (the cache holds\nno other config; surgical addition).\n\nCoverage: 3 new tests in `execution_payload_cache` —\n`wait_for_payload_returns_wait_timeout_when_payload_never_arrives`,\n`wait_for_payload_returns_payload_when_it_arrives_in_time`, plus\nthe existing `wait_for_payload_returns_receiver_disrupted_on_eviction`\nupdated to the new constructor signature. 3/3 pass; full suites\nclean: irys-domain 390/390, irys-actors 243/243, irys-p2p 297/297,\nirys-types 684/684.\n\n* fix(validation): close H5 — soft-internal VDF stage B parent eviction\n\nThe 2026-05-20 audit identified that `ensure_vdf_is_valid`'s\nStage B did `.expect(\"previous block should exist\")` inside a\n`tokio::task::spawn_blocking` closure. A parent-eviction race\n(depth-prune / reorg) between VDF task queuing and Stage B\nexecution would panic the blocking thread; the `JoinError`\npropagated via `.await??` → `resume_unwind`, escaped\n`ensure_vdf_is_valid`, and was eventually caught by\n`setup_panic_hook` → SIGINT → supervisor restart. A recoverable\nrace became a self-DoS, contradicting how every other site in\nthis branch classifies parent eviction (`SoftInternal`).\n\nFix: lift the parent lookup OUT of `spawn_blocking` to the async\ncaller and surface the missing-parent case through a typed\nsentinel. A new `VdfValidationResult::ParentMissing { parent_hash }`\nvariant is downcast by `PreemptibleVdfTask::execute` and mapped\nin the dispatch loop to `ValidationError::ParentBlockMissing`,\nwhich `classify()` already routes as `SoftInternal`. The blocking\nclosure no longer captures `block_tree_guard` — only the cloned\nparent header. Mirrors the existing `seeds_validation_task`\npattern in `block_validation_task.rs`.\n\nA dedicated `ParentMissing` variant (rather than reusing\n`Cancelled`) avoids the `Cancelled`-path's unconditional task\nrequeue, which would loop indefinitely if the parent stayed\nevicted. `ParentMissing` produces a single SoftInternal verdict\nthat the block-tree layer handles per existing policy.\n\nCoverage: 3 new unit tests in `validation_service` —\n`returns_sentinel_when_parent_absent`,\n`returns_cloned_parent_when_present`,\n`sentinel_round_trips_through_eyre_report`. New\n`vdf_parent_missing` cancellation metric tag in\n`active_validations`. Full irys-actors: 247/247 pass.\n\n* fix(validation): close H6 — reclassify ingress-proof treasury underflow as NodeFault\n\nThe 2026-05-20 audit identified that\n`shadow_tx_generator::try_process_publish_ledger`'s ingress-proof\nreward `checked_sub` on the treasury constructed\n`TreasuryArithmetic` on underflow, which the validator's\n`classify_shadow_err` mapped to `Consensus` (peer-attributed).\nBut by that point in the iterator the treasury balance had\nalready been mutated upstream by `try_process_commitment_refunds`\n(Phase 4 — `ExpiredLedgerFees`), which deducts amounts derived\nfrom LOCAL SNAPSHOTS. An underflow at the publish-ledger phase\ncould therefore reflect a local snapshot divergence, not a peer\nfault — and the existing classification would silently\npeer-attribute a node fault, banning honest peers and risking a\ncanonical fork.\n\nAudited all six `TreasuryArithmetic` construction sites in\n`shadow_tx_generator.rs`. Five sites (`create_submit_shadow_tx`,\n`try_process_submit_at_index` x2, `try_process_term_only`,\n`try_process_commitment_at_index`) all operate on\npurely-peer-derived operands at a point where the running\ntreasury has NOT yet been mutated by snapshot-derived amounts\n— they remain `TreasuryArithmetic` (Consensus). The publish-\nledger underflow is the only site with snapshot-derived state\ndependency; it now constructs `SnapshotTreasuryUnderflow`\n(NodeFault → supervisor restart), preserving the never-mislabel\nrule.\n\nTightened doc comments on both variants encode the invariant\nexplicitly: `TreasuryArithmetic` requires both purely-peer\noperand AND no prior snapshot mutation; `SnapshotTreasuryUnderflow`\ncovers either snapshot-derived operand OR prior snapshot\nmutation of the running treasury.\n\nCoverage: new regression test\n`publish_ingress_underflow_after_expired_ledger_payout_is_snapshot_underflow`\ndrives the full iterator with a small initial treasury + a\nsnapshot-derived expired-ledger payout that drains it below the\ningress reward, asserting `SnapshotTreasuryUnderflow` (not\n`TreasuryArithmetic`). 248/248 irys-actors pass.\n\n* obs(validation): close M1 — delegate metric_label for PreValidation arm\n\nThe 2026-05-20 audit identified tha…",
          "timestamp": "2026-05-26T10:17:18+01:00",
          "tree_id": "3536e378de491d736e6508d7591b6eea3ec61157",
          "url": "https://github.com/Irys-xyz/irys/commit/f5f31b306a08fb8410295e680ec3c720e3331aae"
        },
        "date": 1779788203172,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012653,
            "range": "± 0.00046",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.121742,
            "range": "± 0.003894",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.213928,
            "range": "± 0.040973",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.348829,
            "range": "± 0.474393",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.07559,
            "range": "± 0.000808",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 775.19518,
            "range": "± 24.115827",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 971.855943,
            "range": "± 5.174803",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.121373,
            "range": "± 0.001814",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1233.067303,
            "range": "± 11.375537",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1593.81753,
            "range": "± 15.409089",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035067,
            "range": "± 0.000755",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 208.809749,
            "range": "± 0.191158",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 271.307603,
            "range": "± 0.226427",
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
          "id": "b721ad75e0b1ecf844ed675f2794b446264b656b",
          "message": "feat: more debug-utils",
          "timestamp": "2026-05-26T10:07:04Z",
          "tree_id": "7e64609cf0e83ef4dac2e94bdb0767250801da08",
          "url": "https://github.com/Irys-xyz/irys/commit/b721ad75e0b1ecf844ed675f2794b446264b656b"
        },
        "date": 1779790906162,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015101,
            "range": "± 0.001129",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.152563,
            "range": "± 0.004194",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.541863,
            "range": "± 0.061575",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.587324,
            "range": "± 0.459656",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083371,
            "range": "± 0.000642",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 764.307207,
            "range": "± 8.837379",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 984.459132,
            "range": "± 7.134448",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.128826,
            "range": "± 0.005321",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1355.157976,
            "range": "± 100.531036",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1568.066576,
            "range": "± 14.193508",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.036258,
            "range": "± 0.003033",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.554022,
            "range": "± 1.104603",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 275.187795,
            "range": "± 1.734602",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000117,
            "range": "± 0.000003",
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
          "id": "0aa2edfe9a9badf81b8fc2b0d7168e7008f19674",
          "message": "feat: improve coverage gh-pages",
          "timestamp": "2026-05-26T10:22:21Z",
          "tree_id": "4c73c041aafdbc84af589a20048ba2fd077fa041",
          "url": "https://github.com/Irys-xyz/irys/commit/0aa2edfe9a9badf81b8fc2b0d7168e7008f19674"
        },
        "date": 1779791856853,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.01544,
            "range": "± 0.000702",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.152939,
            "range": "± 0.007517",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.565613,
            "range": "± 0.024499",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.545981,
            "range": "± 0.243142",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.084153,
            "range": "± 0.001818",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 781.010596,
            "range": "± 36.386015",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 973.473419,
            "range": "± 5.675942",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120664,
            "range": "± 0.003524",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1225.964281,
            "range": "± 90.988315",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1570.003489,
            "range": "± 12.897389",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034339,
            "range": "± 0.002564",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 212.134275,
            "range": "± 1.541709",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.321139,
            "range": "± 1.29395",
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
          "id": "30a6ac6ab50a0852063db7f0d61fbb0e5bf35070",
          "message": "feat: obs follow-ups and Reth metrics bind hardening (#1426)\n\n* feat(obs): land block-tree, mempool, and block-pool primitives\n\n* feat(metrics): default Reth Prometheus endpoint to 127.0.0.1\n\n* fix(metrics): restore `pub` on record_chain_sync_block_rejected",
          "timestamp": "2026-05-27T08:40:43+01:00",
          "tree_id": "47594f972352ca0b061302c0df1e550215572126",
          "url": "https://github.com/Irys-xyz/irys/commit/30a6ac6ab50a0852063db7f0d61fbb0e5bf35070"
        },
        "date": 1779868518835,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015676,
            "range": "± 0.000772",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.153365,
            "range": "± 0.003286",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.568767,
            "range": "± 0.047323",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.496399,
            "range": "± 0.221126",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083413,
            "range": "± 0.002332",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 780.170571,
            "range": "± 14.989568",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 994.364925,
            "range": "± 15.592278",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.126962,
            "range": "± 0.004115",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1337.794775,
            "range": "± 99.811445",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1617.398543,
            "range": "± 141.59093",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.14092,
            "range": "± 0.035818",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 214.216496,
            "range": "± 2.265751",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 277.843383,
            "range": "± 3.440427",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000119,
            "range": "± 0.000004",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e67df7dd32a9968b560d9f70efb129901f2555b5",
          "message": "feat: release process (#1393)\n\n* feat(ci): rewrite release workflow for atomic publishing\n\n- Version is now extracted from the irys-chain crate via cargo metadata\n  instead of the workspace Cargo.toml, giving a precise per-crate check\n- Docker image is built before any git tags are pushed, so a failed build\n  never leaves an orphaned tag in the repository\n- Cleanup steps delete the release git tag (and head-tracking tag) if any\n  publish step fails after the tag has already been pushed\n- RC releases are auto-published as prereleases; mainnet releases are\n  created as drafts requiring manual publication\n- Head-tracking tags (latest / rc-latest) are updated on both git and\n  Docker registry after every successful release\n- Changelog uses the commit SHA as the range end so it works correctly\n  in dry-run mode where the version tag does not yet exist\n\n* fix(ci): address release workflow review findings\n\n- Quote heredoc delimiters ('ENDBODY') in both release creation steps to\n  prevent shell interpretation of changelog content and backtick sequences\n- Save the previous head-tracking git tag position before moving it, then\n  restore it on failure rather than blindly deleting (handles first-release\n  edge case where no previous tag existed)\n- Add || true to docker system prune so cleanup never fails the workflow\n- Emit a clear error annotation when rootless Docker fails to start within\n  30 seconds\n\n* feat(ci): add docker-retag workflow for rollback\n\nManual-dispatch workflow that re-tags an existing Docker image in GHCR\nwithout rebuilding. Optionally moves the corresponding git tag to the\nsame commit as the source tag. Supports testnet and mainnet rollback by\npointing latest or rc-latest at a known-good prior release tag.\n\n* fix(ci): finalize docker.yml as manual rebuild workflow\n\nAdd missing build args (GIT_SHA, GIT_HAS_TAG, GIT_DIRTY) for\nDockerfile.release parity. Workflow is now manual dispatch only.\nAlso add || true to docker system prune and diagnostic message to\nDocker daemon startup timeout.\n\n* fix(ci): always run full CI on release branches\n\nRelease branches must have full CI coverage since they are the source of testnet and mainnet deployments.\n\n* docs: update release process with rollback, atomicity, head-tracking tags\n\nDocuments RC version semantics (orphaned RCs are expected), head-tracking\ntags (rc-latest/latest for both git and Docker), draft release flow for\nmainnet (auto-publish for RC, manual publish for production), rollback\nprocedure via docker-retag workflow, atomicity guarantee (image built before\ntags are pushed, cleanup on failure), and canonical version source\n(crates/chain/Cargo.toml).\n\n* feat: improvements\n\n* docs: fmt\n\n* feat: use git cliff for changelogs\n\n* feat(ci): switch release workflow to testnet/mainnet deployment branches\n\nRenames release_type choices to testnet/mainnet, validates commits are on\ndeployment/<env>/<major>.x branches, splits docker image streams\n(irys-testnet, irys-mainnet) with plain SemVer tags inside each stream,\nand uses env-prefixed git tags (testnet-X.Y.Z, mainnet-X.Y.Z) for\ndisambiguation.\n\nThe RC-tag-must-match-same-commit check is relaxed to a same-upstream-\nmerge-base check on release/<major>.x, since testnet and mainnet now ship\ndifferent deployment-patch commits but must share their release-branch\nancestor.\n\n* fix(ci): address code review on release workflow\n\n- Update commit input description to reference deployment/<env>/<major>.x\n- Update force input description to say testnet-tag validation (not RC)\n- Wrap git merge-base calls with explicit ::error:: annotations so the\n  no-common-ancestor case fails with a clear message instead of an\n  opaque bash -e exit.\n\n* feat(ci): add environment input to docker-retag workflow\n\nAdds environment choice (testnet | mainnet) so rollbacks target the\nright image stream (irys-testnet vs irys-mainnet). target_tag is now\nconstrained to latest (no rc-latest). Git tag moves use the env-\nprefixed scheme (testnet-1.0.1 -> testnet-latest).\n\n* feat(ci): add environment input to docker rebuild workflow\n\nPer-env image streams (irys-testnet, irys-mainnet, irys-devnet) require\nthe rebuild workflow to know which stream to target.\n\n* ci: run full CI on deployment/* branches\n\nDeployment branches carry real env-specific code patches, so they must\ngo through the same CI gate as release/* branches.\n\n* feat(ci): add devnet build workflow\n\nBuilds the deployment/devnet branch and pushes to\nghcr.io/<owner>/irys-devnet with tags <short-sha> and latest.\nworkflow_dispatch only -- devnet is on-demand, no semver, no GitHub\nRelease.\n\n* docs: rewrite release process for deployment-specific patches\n\nUpdates Environments, Branches, Tags, Release Flow, Atomicity, Head-\nTracking Tags, Hotfixes, Rollback, and Process in Action sections to\ndescribe the new per-major deployment branches, env-prefixed git tags,\nand split Docker image streams. Adds a new \"Authoring Deployment-\nSpecific Patches\" section.\n\n* fix: address code review on Tasks 2/5/6\n\n- docker-retag: broaden 'environment' description (was 'Environment to\n  rollback'; this workflow is also used for promotion / head-tracking\n  moves).\n- devnet: add inputs.commit SHA format validation step (parity with\n  release.yml); add comment on the GHA && / || ternary idiom.\n- RELEASE_PROCESS: in the Rollback example, clarify that source_tag is\n  the Docker tag inside the stream (e.g. 1.0.1), not the env-prefixed\n  git tag (testnet-1.0.1).\n\n* fix(cliff): update tag patterns for testnet/mainnet scheme\n\ncliff.toml's tag_pattern and ignore_tags still matched the old\nrc-X.Y.Z / X.Y.Z / rc-latest / latest scheme, so git-cliff would\nfind no matching previous tag and --unreleased would walk the entire\nhistory on every release. Update to recognise testnet-X.Y.Z /\nmainnet-X.Y.Z as versions and testnet-latest / mainnet-latest as\nhead-tracking tags to ignore.\n\n* docs: add release playbook with end-to-end operator walkthrough\n\nRELEASE_PROCESS.md describes the conceptual model (branches, tags,\natomicity, head-tracking). RELEASE_PLAYBOOK.md is the operator's\nstep-by-step companion: prep on release/<major>.x, merge forward to\ndeployment branches, dispatch the release workflow, and publish the\ndraft mainnet release with a custom changelog. Includes pre-flight\nmerge-base check, three approaches for customising the changelog body,\nand a quick decision-points table.\n\nIndexed in docs/99-reference/README.md alongside RELEASE_PROCESS.md.\n\n* fix(ci): address review findings (creds, action pinning, branch match)\n\n- Pin `orhun/git-cliff-action` to v4.8.0 SHA so a mutable upstream tag\n  cannot inject arbitrary code into the release job (which already has\n  contents:write + packages:write on a self-hosted runner).\n- Set `persist-credentials: false` on the release-workflow validate\n  checkout, the devnet build checkout, and the docker-rebuild checkout\n  — none of those jobs push to git, so there is no reason to leave the\n  GITHUB_TOKEN in .git/config.\n- Tighten the release-workflow branch validation to require an exact\n  match of origin/deployment/<env>/<MAJOR>.x with MAJOR derived from\n  inputs.version. Previously any branch under deployment/<env>/ passed,\n  so a 1.0.0 release could be cut from a commit only present on\n  deployment/testnet/2.x.\n\nThe release-job checkout still keeps the default persist-credentials:\ntrue because it pushes the version git tag and the head-tracking tag.\n\n* refactor(ci): extract docker host setup/teardown to composite actions\n\nThe four workflows (release, devnet, docker rebuild, docker-retag) each\ncopied the same dockerd-rootless start, ghcr login, and end-of-job\ncleanup blocks. Extracted into two composite actions:\n\n- .github/actions/docker-setup: starts the rootless daemon, waits up to\n  30s for it to be ready, then `docker login` via stdin.\n- .github/actions/docker-cleanup: `docker logout`, stop containers,\n  prune images and volumes, kill the daemon, remove runtime state.\n  Designed to be called with `if: always()`.\n\nBehavioral changes:\n\n- The cleanup now runs `docker logout ghcr.io` before killing the\n  daemon, so the GHCR credential is not left in `~/.docker/config.json`\n  after a release run. (Codex medium finding.)\n- The composite-action calls all pass the GH token via an env var\n  inside the action, instead of expanding `${{ secrets.GITHUB_TOKEN }}`\n  directly into a `run:` shell command in each workflow.\n\nNet diff: 4 workflows lose ~75 lines, two new composite actions add ~50.\n\n* feat(ci): gate release-touching jobs on GitHub Environments + TOML parse\n\nTwo hardening changes:\n\n1. `release.yml`, `docker.yml`, and `docker-retag.yml` now declare\n   `environment: ${{ inputs.<env> }}` on their build/publish jobs. This\n   makes the workflow consult repo Settings → Environments before\n   running — operators can configure required reviewers on the\n   `mainnet` environment to force a human approval before any mainnet\n   image stream is written to, without changing the workflow. Documented\n   the expected environments in RELEASE_PROCESS.md.\n\n2. The Cargo.toml version check in `release.yml` switches from\n   sed/grep/head/sed to a `python3 -c tomllib` one-liner. Same shape of\n   input/output, but survives comments, multi-line strings, and section\n   reorders. Runners ship Python 3.12, which has `tomllib` in stdlib.\n\nRequires the `devnet`, `testnet`, and `mainnet` GitHub Environments to\nexist before these workflows can run.\n\n* fix(ci): improve atomicity of release / retag workflows\n\nrelease.yml:\n  Reorder so the head-tracking git tag and the GH Release are written\n  BEFORE the Docker `:latest` retag, and mark the `:latest` retag as\n  continue-on-error. This closes the window where a `gh release create`\n  failure could leave Docker `:latest` pointing at a release that was\n  then rolled back by the cleanup steps. If `:latest` push fails after\n  everything else has been published, the workflow logs a warning\n  pointing the operator at docker-retag.yml and keeps the rest of the\n  release intact.\n\ndocker-retag.yml:\n  Resolve and validate the source git tag in a step that runs BEFORE\n  the Docker daemon is started. A missing source git tag now fails the\n  job without having pulled, retagged, or pushed anything. The\n  inconsistency window between Docker push and git tag move is\n  unavoidable without registry-side manifest copy tooling, but\n  fail-fast on the git side is the cheapest win.\n\nUpdated the Atomicity section of RELEASE_PROCESS.md to describe the new\npublish order and the `:latest` retag warning path.\n\n* docs(release): add initial-repo + per-release-branch setup sections\n\nReplaces the previous narrow \"Approval Gates\" section with two\nordered, prescriptive setup sections so a brand-new repo (or a fresh\noperator on an existing one) has a single doc that walks them from\nnothing to \"ready to dispatch a release\":\n\n- Initial Repo Setup: the three GitHub Environments, the self-hosted\n  runner pointer, and the first long-lived branches (master,\n  deployment/devnet).\n- Per-Release-Branch Setup: the three branches that need to exist\n  before dispatching the release workflow against a new major\n  (release/<major>.x and the two deployment/<env>/<major>.x branches),\n  with the git commands and a note that GitHub Environments are global\n  and do NOT need to be recreated per release branch.\n\n* fix(ci): correct release/rebuild workflow bugs from review\n\n- docker.yml: require an explicit commit input and build from it instead\n  of github.sha, so a dispatched rebuild reproduces the source commit\n  rather than the selected branch tip (which silently overwrote the\n  versioned image with unrelated code).\n- release.yml: configure a git identity before `git tag -a`; self-hosted\n  runners have no global user.name/email, so annotated tagging aborted\n  every non-dry-run release.\n- release.yml: drop any stale local tag before creating it and in the\n  failure-cleanup step, so a previously-failed run on the same runner\n  can't block a retry of the same version.\n- docker-setup: make `username` required and pass `github.actor`\n  explicitly from all four callers, rather than relying on expression\n  expansion in a composite-action input default.\n\n* fix(ci): harden release/rebuild workflows against stale tags and rebuild gaps\n\nSecond-pass review fixes:\n- Sync tags from origin (tags-only refspec) before every tag-dependent step\n  (release validate gate, release save_head, retag resolve) so stale local\n  tags on reused self-hosted runners can't false-fail the tag-exists check or\n  move a head-tracking tag to an unpublished commit. The validate checkout\n  re-enables its read-only token to read authoritative remote tag state.\n- docker.yml: add concurrency group:release so a manual rebuild can't race a\n  release/rollback; add commit-provenance + Cargo.toml-version validation\n  (mirrors release.yml/devnet.yml) so it can't publish a mislabeled image;\n  set GIT_HAS_TAG=true for testnet/mainnet rebuilds so the embedded version\n  matches the original release.\n- cliff.toml: use_branch_tags=true so changelog tag boundaries stay within the\n  deployment branch being released (diverged testnet/mainnet lineages).\n\n* fix(ci): close rebuild provenance gap and harden release tooling\n\nReview-driven fixes across the release workflows:\n\n- docker.yml: require a testnet/mainnet rebuild commit to be exactly the\n  commit its <env>-<tag> release tag points at (resolve + compare), so a\n  rebuild can't overwrite a published image with bits never released as\n  that version. Adds an authoritative tag-sync and read-only checkout\n  creds for the check. Drops the redundant version-extraction step.\n- release.yml: preflight an existing GitHub release/draft in `validate`\n  so a name collision fails before any tag/image is published.\n- docker-setup: defensively reset leftover rootless-Docker state before\n  starting, so a prior job that skipped teardown can't contaminate the\n  next sequential job on a reused runner.\n- docker-cleanup: guard the XDG_RUNTIME_DIR wipe so an unset var can't\n  abort teardown after a successful publish.\n- devnet.yml: route publishing through the `devnet` GitHub Environment\n  for consistency with the other publishing workflows.\n- cliff.toml: add build/revert/style commit parsers.\n- docs: correct the playbook publish order (:latest moves last,\n  non-fatally) and document the cancel-pending-release-before-rollback\n  step for the shared release concurrency group.\n\n* refactor(ci): extract shared image-name and commit-provenance composites\n\nThe image-name derivation was copy-pasted across four workflows and the\ncommit-provenance checks were near-duplicated between release.yml and\ndocker.yml — drift between those copies is what let docker.yml's rebuild\npath ship without the release-tag check (fixed in the previous commit).\n\nExtract two composite actions and have every workflow call them:\n\n- image-name: derives ghcr.io/<owner>/irys-<env> once (owner read via env\n  rather than interpolated into a run block).\n- verify-commit-provenance: single home for \"commit exists + on the\n  matching deployment branch + (testnet/mainnet) Cargo version matches +\n  optionally the <env>-<version> release tag points at it\". The\n  require-release-tag flag distinguishes the mint path (release.yml, tag\n  must not exist yet) from the rebuild path (docker.yml, tag must match).\n\nNo behavior change intended; consolidates logic so the strong provenance\ncheck can't drift between workflows again.\n\n* fix(ci): require full commit SHA, correct release docs\n\nTighten workflow_dispatch commit validation to a full 40-char SHA in the\nrelease/devnet/docker workflows. actions/checkout does not resolve\nabbreviated SHAs, so a 7-39 char input passed validation then failed at\nthe publish-job checkout (after the mainnet approval gate); reject it up\nfront with a clearer message.\n\nAlso fix two release-doc issues: Docker Retag only moves :latest and\ncannot remove an orphaned versioned image (point to manual GHCR cleanup\nor a docker.yml rebuild), and the playbook's nested code fence rendered\nbroken (use a four-backtick outer fence).\n\n* refactor(ci): pin checkout, dedupe build/tag-sync, harden provenance\n\nReview follow-ups (F4-F8):\n- Pin actions/checkout to the v4.3.1 commit SHA in all four release\n  workflows (matches the existing git-cliff-action SHA pin).\n- Extract the repeated docker build from Dockerfile.release into a\n  docker-build composite (keeps the git build-args consistent across\n  release/devnet/docker) and the tag-sync git fetch into a sync-tags\n  composite (4 call sites).\n- Collapse release.yml's two near-identical GitHub Release steps into one\n  that selects --prerelease (testnet) vs --draft + Summary (mainnet).\n- Pass docker.yml's tag/image through env vars instead of inline\n  interpolation; add an empty-VERSION guard to verify-commit-provenance;\n  document why docker-retag persists checkout credentials.\n\nBehavior preserved; all workflows + composites parse clean.\n\n* fix(ci): harden release workflows; align Docker GIT_SHA to short hash\n\nReview-driven fixes to the release tooling:\n\n- release.yml: skip the environment approval gate on dry-run via an\n  empty-string environment (inverted GHA ternary, since `a && '' || b`\n  can never select the empty branch) so a preflight needs no reviewer and\n  won't hold the shared `release` concurrency slot.\n- Add restrictive top-level `permissions: {}` to release/docker/\n  docker-retag/devnet so future jobs default to minimal scope.\n- Route the image name through an `IMAGE` env var in docker-retag and the\n  release docker push steps (consistency; silences zizmor injection lint).\n- Document why the publish-job checkout persists credentials (tag pushes\n  + failure-cleanup) so it isn't \"hardened\" into breakage.\n- build.rs: truncate the Docker-provided GIT_SHA to 7 chars so the env\n  path matches the git path (`--short=7`) and the init_version 7-char\n  short-hash contract; previously devnet images embedded the full 40-char.\n- Clarify verify-commit-provenance precondition: tag-sync is only required\n  for testnet/mainnet (devnet does only the branch check).\n\n* docs(release): document dry-run testing from current repo state\n\nAdd a Dry-run testing section to the playbook covering what dry_run\nvalidates (inputs, provenance, version, real Docker build, changelog) vs.\nskips, and the prerequisites to run one today: release workflows must be on\nthe default branch to be dispatchable, a deployment/<env>/<major>.x branch\nmust exist (flat legacy branches don't satisfy provenance), and a Docker-\ncapable misc-runner. Includes testnet (minimal) and mainnet (force=true)\ncommand walkthroughs.\n\nNote in RELEASE_PROCESS.md that dry_run resolves environment to empty and\nskips the approval gate, so it needs no reviewer and no preconfigured envs.\n\n* fix(ci): correct first-release rollback and devnet rebuild concurrency\n\n- release.yml: save_head used `git rev-parse \"$HEAD_TAG\"`, which echoes the\n  literal tag name to stdout when the tag is missing, leaving `prev` non-empty.\n  On an environment's first release, a mid-publish failure would then re-pin\n  <env>-latest to the failed release instead of deleting it. Use\n  `git rev-parse --verify --quiet` so a missing tag yields an empty string.\n\n- docker.yml: a devnet rebuild ran in the `release` concurrency group, so a\n  tag_latest=true rebuild could race devnet.yml over irys-devnet:latest. Route\n  devnet rebuilds into devnet.yml's `devnet` group via an env-aware group\n  expression; testnet/mainnet rebuilds still serialize with release.yml.\n\n* docs(release): make dry-run prerequisite branch-agnostic\n\nThe dry-run prerequisites hardcoded the transient `feat/release-process`\nbranch name and \"merge that branch into master first\", which goes stale once\nthis branch merges. State the durable requirement instead: release.yml must be\non the default branch before workflow_dispatch works. Keeps the note that the\n`commit` input (not the workflow's branch) decides what gets built.\n\n* fix(ci): align devnet short SHA with build.rs and tidy review nits\n\n- devnet.yml: derive the image tag's short SHA with `cut -c1-7` instead of\n  `-c1-12` so it matches the 7-char SHA that crates/chain/build.rs embeds into\n  the binary version (build.rs truncates GIT_SHA to 7), making the devnet image\n  tag equal the SHA the running binary reports.\n- release.yml: route TAG through `env:` in the \"Check tag does not already\n  exist\" step, matching the sibling release/draft check and the env-passing\n  pattern used elsewhere.\n- RELEASE_PROCESS.md: tag the Release Flow ASCII fence as `text` (the only\n  untagged fence in a file that tags all others).\n\n* fix(ci): clearer error when deployment branch is missing locally\n\nverify-commit-provenance: pre-check that the expected\nrefs/remotes/origin/deployment/<env>/<major>.x ref exists before the\n--contains test, so a missing/un-created deployment branch fails with an\nactionable message instead of a misleading \"commit not on branch\" error.",
          "timestamp": "2026-05-27T12:19:44+01:00",
          "tree_id": "01e922c892420349810bbeb5ac2c174df87fe10e",
          "url": "https://github.com/Irys-xyz/irys/commit/e67df7dd32a9968b560d9f70efb129901f2555b5"
        },
        "date": 1779881707645,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015313,
            "range": "± 0.000398",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.125173,
            "range": "± 0.003752",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.264258,
            "range": "± 0.034014",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.352882,
            "range": "± 0.037955",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078313,
            "range": "± 0.00195",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 758.374957,
            "range": "± 11.214568",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 997.681717,
            "range": "± 22.6876",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.148875,
            "range": "± 0.007739",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1206.552927,
            "range": "± 20.247315",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1575.427086,
            "range": "± 18.758104",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035256,
            "range": "± 0.001257",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 212.032025,
            "range": "± 1.799434",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 276.292013,
            "range": "± 1.742684",
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
          "id": "76f9ed4987ca6c34e81876f0a0f70d3208eb73d9",
          "message": "feat: move from deployment/ to release/",
          "timestamp": "2026-05-27T11:38:53Z",
          "tree_id": "ed441174d7a08ba79b5a68cf334fd24c9001c15e",
          "url": "https://github.com/Irys-xyz/irys/commit/76f9ed4987ca6c34e81876f0a0f70d3208eb73d9"
        },
        "date": 1779882789260,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011903,
            "range": "± 0.000186",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.118914,
            "range": "± 0.000888",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.195599,
            "range": "± 0.010881",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.871804,
            "range": "± 0.08137",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078896,
            "range": "± 0.00062",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 795.483128,
            "range": "± 35.827078",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1075.912606,
            "range": "± 29.789473",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.149313,
            "range": "± 0.005525",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1301.236135,
            "range": "± 106.409743",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1600.315936,
            "range": "± 108.129942",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034066,
            "range": "± 0.000508",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.928574,
            "range": "± 1.317254",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.357485,
            "range": "± 2.205534",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000112,
            "range": "± 0.000002",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "435dea4b84af724a8a939b1c44d1caa9aa2c868b",
          "message": "fix(ci): drop literal expression from docker-setup input description (#1427)\n\n* fix(ci): drop literal expression from docker-setup input description\n\nThe `username` input description contained a literal ${{ github.actor }}\nexample. GitHub's action manifest loader evaluates expressions even inside\ndescription text, and the github context is unavailable at load time, so the\nwhole composite action fails to load with \"Unrecognized named-value: 'github'\".\n\nThis broke the \"Start rootless Docker and log in to GHCR\" step in every job\nthat uses the action (release, docker, devnet) — caught by a testnet dry-run.\nRefer to the context by name only; the real `username: ${{ github.actor }}`\nlives in the calling workflows, where it's valid.\n\n* ci(release): run git-cliff with -vv so skipped commits are logged\n\nThe changelog step logs \"N commit(s) skipped due to parse error\" without\nnaming them. Pass -vv so git-cliff logs each non-conventional/unparseable\ncommit (e.g. a commit missing a `type:` prefix) to stderr — visible in the\nActions log. Output is unaffected; the changelog still goes to OUTPUT.\n\n* ci(release): scope changelog trace to git-cliff via RUST_LOG (drop -vv)\n\n-vv enables global TRACE, which floods the changelog log with thousands of\nhyper/HTTP lines from git-cliff's GitHub remote-data fetch and truncates before\nthe useful line. RUST_LOG=git_cliff_core=trace names each skipped/non-conventional\ncommit (e.g. the missing-`type:` commit) without the noise.\n\n* fix(release): bound changelog to the same env's previous release\n\nEach release's notes should span since the previous release of the SAME\nenvironment: testnet-X.Y.Z since the last testnet-*, mainnet-X.Y.Z since the\nlast mainnet-*.\n\nThe prior `--ignore-tags ^testnet-` (mainnet) didn't achieve this: git-cliff's\n--unreleased anchors the commit range to the latest reachable version tag, and\n--ignore-tags only relabels the \"previous\" link without moving the range. With\nboth testnet-0.2.0 and the newer mainnet-0.1.3 reachable from the 3.x line, a\ntestnet release wrongly spanned since mainnet-0.1.3.\n\nOverride tag_pattern per env via GIT_CLIFF_TAG_PATTERN so --unreleased anchors to\nthe env's own previous tag (verified: testnet-3.0.0 -> previous testnet-0.2.0).\nFirst release of an env (no prior tag) spans full history, as expected.",
          "timestamp": "2026-05-27T14:05:49+01:00",
          "tree_id": "23f391d1426ce892ce112d000594f2efdd747f69",
          "url": "https://github.com/Irys-xyz/irys/commit/435dea4b84af724a8a939b1c44d1caa9aa2c868b"
        },
        "date": 1779888035661,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015207,
            "range": "± 0.000551",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.153216,
            "range": "± 0.005237",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.558462,
            "range": "± 0.09328",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.530837,
            "range": "± 0.448349",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.08324,
            "range": "± 0.002054",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 849.69895,
            "range": "± 14.187389",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1109.18922,
            "range": "± 23.182703",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.142115,
            "range": "± 0.011335",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1469.455042,
            "range": "± 119.311035",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1713.053059,
            "range": "± 164.736103",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035795,
            "range": "± 0.004572",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 221.276822,
            "range": "± 14.574637",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 278.126054,
            "range": "± 3.369609",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f26fa19715c007cec909ea1376a1eed0d1f7058b",
          "message": "fix(release): disable git-cliff GitHub remote fetch (panics on flaky API) (#1428)\n\nThe changelog step panicked (changelog.rs:493 -> exit 101) fetching every\ncommit + every closed PR from the GitHub API and hitting a truncated response\n(\"end of file before message length reached\"). That remote data is unused —\nthe cliff.toml template references only local commit fields.\n\nDrop GITHUB_REPO from the step env; git-cliff v2.13.1 (pinned by the action)\nwon't fetch without an explicit repo and doesn't auto-detect from origin\n(verified). The action still supplies a token for its own binary download.",
          "timestamp": "2026-05-27T15:37:15+01:00",
          "tree_id": "6fc0c7fe1649334ff41d47ca8ad47f6b6628b871",
          "url": "https://github.com/Irys-xyz/irys/commit/f26fa19715c007cec909ea1376a1eed0d1f7058b"
        },
        "date": 1779893541458,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015325,
            "range": "± 0.000421",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.153468,
            "range": "± 0.007079",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.579885,
            "range": "± 0.11797",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.67997,
            "range": "± 0.764737",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078787,
            "range": "± 0.001322",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 786.499138,
            "range": "± 20.286735",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1067.017054,
            "range": "± 38.199516",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.128074,
            "range": "± 0.003733",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1307.01394,
            "range": "± 94.208354",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1860.503902,
            "range": "± 155.023091",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035025,
            "range": "± 0.001696",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 213.272092,
            "range": "± 1.298065",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.853925,
            "range": "± 2.119266",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000115,
            "range": "± 0.000007",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "b603f1e98cba5fa7b7c66cb0b66e2826351eec76",
          "message": "feat: improved multiversion tests (#1404)\n\n* fix(p2p): restore v1 wire compatibility for older nodes\n\nTwo on-the-wire shapes drifted between d071fc03 and HEAD in ways that\nbreak cross-version gossip on the receive side:\n\n* `RejectionReason::HandshakeRequired` was a unit variant on older\n  nodes (`\"HandshakeRequired\"`) but became a newtype variant carrying\n  an `Option<HandshakeRequirementReason>` on HEAD\n  (`{\"HandshakeRequired\": null}`). The diagnostic payload is purely\n  advisory — older peers cannot deserialize the newtype shape.\n\n  Replace the derived `Serialize`/`Deserialize` for `RejectionReason`\n  with custom impls. Serialize emits the v1 unit-string form for\n  `HandshakeRequired` (dropping the `Option` payload on the wire),\n  keeps the newtype shape for `UnsupportedProtocolVersion(u32)`\n  whose payload is load-bearing, and emits all other variants as\n  unit strings. Deserialize accepts both unit-string and single-key\n  object forms, with an `IgnoredAny` tolerance for legacy emissions\n  of object-shaped unit variants.\n\n* `/v1/protocol_version` returned a single `u32` on older nodes but\n  HEAD's `get_protocol_versions` deserializes the response body as\n  `Vec<u32>`. Add an untagged `ProtocolVersionsRepr` enum that\n  accepts either `[1, 2]` or bare `1` and normalizes to a `Vec<u32>`,\n  preserving the `MAX_PROTOCOL_VERSIONS` DDoS guard.\n\nRegenerate `fixtures/gossip_fixtures.json` to reflect the v1 wire\nform of the four `gossip_response_rejected_handshake_required_*`\nfixtures.\n\n* feat(multiversion-tests): cross-version harness with config templates, data tx flow, and promotion verification\n\nThe harness used to verify cross-version upgrades by checking that\nnodes booted, gossiped, and converged. That gave a thin signal: a\nbinary swap could leave silent data-shape corruption on disk and the\ntests would still pass. This change extends the harness so every\ntest path actually drives transactions through the cluster and\nstrictly validates what every node serves back, across binary\nboundaries.\n\nCross-version cluster configuration\n-----------------------------------\n* `BinaryKind` (`Old` | `New`) is attached to every `ResolvedBinary`,\n  letting the cluster route per-node config generation through the\n  right base template when OLD and NEW disagree on `NodeConfig`\n  schema.\n* `ClusterSpec` carries `base_config_old` + `base_config_new` instead\n  of a single template. `Cluster::upgrade_node` regenerates the\n  on-disk config from the new template before respawn, so the new\n  binary never sees the old-shaped file. For peer nodes the new\n  template's `[consensus.Custom]` block (when present) is overlaid\n  on top of what the running genesis serves at `/v1/network/config`\n  via `patch_peer_consensus(template_overlay=...)`, letting users\n  backfill new-only fields without losing the genesis-provided\n  values for shared fields.\n* `xtask multiversion-test --base-config-old/--base-config-new`\n  point at TOML templates, exported as `IRYS_BASE_CONFIG_OLD/NEW`\n  env vars to the test process. Sample templates for the\n  d071fc03 ↔ HEAD span are committed under `examples/`.\n\nRun config (`--run-config`)\n---------------------------\n* New `run_config.rs` module parses an optional TOML run config\n  with three sections:\n    [tx_header]     aliases + skip lists for the strict tx-header\n                    diff\n    [block_header]  same shape, applied to the cross-node block\n                    consistency check\n    [tx_build]      `keep_default` list of header fields to leave\n                    at default at signing time (for spans where a\n                    non-default value would change the canonical\n                    signature prehash on the older side).\n* Surfaced via `--run-config` and the `IRYS_TEST_RUN_CONFIG` env\n  var. Default is the empty config — the right baseline for\n  adjacent-release tests where renames are rare. The d071fc03 ↔\n  HEAD example config lives at\n  `examples/run-config-d071fc03.toml`.\n\nData tx submission and strict verification\n------------------------------------------\n* New `data_tx.rs` module submits signed Publish-ledger data\n  transactions over plain HTTP (`/v1/anchor`, `/v1/price`,\n  `/v1/tx`, `/v1/tx/{id}`). It uses HEAD's `irys-types` for\n  signing — the wire-shape compatibility of the request/response\n  body is what we're actually testing.\n* `submit_data_tx` sets `header_size` to a non-default sentinel\n  before signing so the on-disk `Compact` encoding actually\n  exercises non-zero payload bytes. `metadata_format` (the\n  renamed field) is opt-in non-default via `tx_build.keep_default`.\n* `assert_tx_matches_original` does a strict full-header round\n  trip against `/v1/tx/{id}` on every node, comparing every field\n  in the JSON object via `compare_full_object`. Aliased rename\n  pairs (`bundleFormat` ↔ `metadataFormat`) check presence only;\n  value comparison is skipped because the rename also changes\n  types.\n* `Cluster::assert_block_index_consistent` enumerates\n  `/v1/block-index?height=0&limit=500` from the genesis, then\n  for each block hash fetches `/v1/block/{hash}` from every node\n  and diffs the responses against the genesis's view via\n  `compare_full_object`. Catches PoA / ledger-metadata /\n  signature drift independently of the tx-header layout.\n\nChunk upload and promotion verification\n---------------------------------------\n* `upload_chunks_for_tx` POSTs every `UnpackedChunk` to\n  `/v1/chunk` so storage nodes can produce ingress proofs and\n  promote the tx from Submit to Publish.\n* `wait_for_promotion` polls `/v1/tx/{id}/promotion-status`\n  until `promotion_height` is set, on both OLD and NEW nodes\n  (same response shape on both).\n* `Cluster::submit_promote_and_verify` strings the whole flow\n  together: submit, upload chunks, wait for genesis to promote,\n  then wait for the chain to advance past\n  `block_migration_depth + 2` so every peer's\n  `block_migration_service` durably commits the promotion to\n  `IrysDataTxMetadata`.\n* `assert_tx_promoted_on_all_nodes` polls every running node\n  for non-`None` `promotion_height` — catches loss of promotion\n  state across binary swaps.\n\nTest wiring\n-----------\n* All three e2e tests now submit + promote + verify, not just\n  produce blocks.\n* All four upgrade tests submit + promote + verify before the\n  first transition; after each transition they re-verify the\n  full tx history is visible everywhere; the strict full-header\n  round-trip and block-index sweep run after every binary swap;\n  promotion preservation is asserted as well.\n* `rolling_upgrade_all_nodes` does its end-to-end promotion\n  check once at the end of the loop — per-step checks raced the\n  just-restarted miner.\n* `rollback_after_upgrade` waits past `block_migration_depth + 4`\n  blocks before each binary swap so on-disk records are actually\n  populated when the migrations run.\n\nDependency additions\n--------------------\n* `irys-types` (with `test-utils`) for `IrysSigner`, `BoundedFee`,\n  `DataTransaction`, `H256`, `U256`.\n* `irys-api-client` for typed wire shapes shared with chain-tests.\n* `k256` for constructing the signer from the funded dev key.\n* `hex`, `eyre` for support.\n\n* docs(multiversion-tests): document cross-version configuration and future improvements\n\nBring the README in line with the new harness shape:\n\n* Add `## Cross-Version Configuration` covering the\n  `--base-config-old` / `--base-config-new` template flow and the\n  `--run-config` TOML structure (tx-header / block-header alias and\n  skip lists, `tx_build.keep_default`). Includes a fully-worked\n  invocation for the d071fc03 ↔ HEAD span and the simpler\n  adjacent-release case.\n* Document the three-layer cross-version assertion stack\n  (visibility, strict tx-header round-trip, cross-node block-header\n  consistency, plus end-to-end tx promotion).\n* Update the env-vars table to include `IRYS_BASE_CONFIG_OLD/NEW`\n  and `IRYS_TEST_RUN_CONFIG`, and pair every variable with its\n  matching xtask flag.\n* Refresh the architecture tree (new `examples/` dir, new\n  `data_tx.rs` and `run_config.rs` modules, tests now live under\n  `src/tests/` rather than a separate `tests/` directory).\n* Refresh the test-suite tables to reflect that every test now\n  submits + promotes + verifies, and explicitly call out the\n  rollback test's `block_migration_depth` wait and end-of-loop\n  promotion check on rolling upgrades.\n* Add `## Future Improvements` at the bottom enumerating concrete\n  follow-ups: chunk-level read-back, non-default values for\n  renamed fields on adjacent-release runs, long-running rollback\n  populations, true gossip-isolated rollback (requires a\n  bootstrap-from-disk flag on the node binary), partition + upgrade\n  combinations, commitment-tx coverage, parallelization, build\n  cache reuse, multi-mismatch reporting in `compare_full_object`,\n  and a per-tx block-signature round-trip.\n\n* fix: update readme\n\n* feat: improvements\n\n* feat: node info now exposes correct version info\n\n* fix: wire protocol compatability",
          "timestamp": "2026-05-27T22:48:43+01:00",
          "tree_id": "1a2bc844934e2b4aafc8b6d70145aa2a434b6607",
          "url": "https://github.com/Irys-xyz/irys/commit/b603f1e98cba5fa7b7c66cb0b66e2826351eec76"
        },
        "date": 1779919902695,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012356,
            "range": "± 0.000248",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.128474,
            "range": "± 0.005132",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.211047,
            "range": "± 0.04803",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.137889,
            "range": "± 0.429319",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078403,
            "range": "± 0.000648",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 799.952228,
            "range": "± 34.210783",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 972.436852,
            "range": "± 6.091398",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.118327,
            "range": "± 0.002596",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1201.96851,
            "range": "± 9.471773",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1559.705431,
            "range": "± 10.667757",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034391,
            "range": "± 0.001466",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.956017,
            "range": "± 1.116313",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.612709,
            "range": "± 1.390396",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.000002",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "04bfa3f73f1e7ae6a59505f9cf0e550ad842a045",
          "message": "fix: CI flakes and irys-reth test memory cap (#1430)\n\n* fix(validation): exempt finished VDF task handle from stall watchdog\n\n`abort_stalled_current` measures wall-clock time since the last\n`record_vdf_task_progress`, but on a heavily-loaded CI runner the select\nloop can be starved past `hard_timeout` *between* a task transitioning\nto `Completed` and `poll_vdf` reaping its `JoinHandle`. In that window\nthe task's result is already sitting ready for collection — the very\nfact that the watchdog itself is running again proves the loop has\nresumed — so aborting would discard a valid, already-computed\nvalidation result and crash an otherwise-healthy node.\n\nSkip the abort when `JoinHandle::is_finished()` is true. Genuine\ndeadlocks, poisoned locks, and runaway loops keep the handle\nunfinished and still trip the watchdog as before. Adds a regression\ntest and updates the design doc to spell out the carve-out.\n\n* fix(p2p/tests): hold gossip listener across run_service to close port race\n\n`random_free_port` binds 127.0.0.1:0, reads the port number, then drops\nthe listener before returning. `GossipServiceTestFixture::run_service`\nlater re-binds that exact port — under parallel test load a sibling\ntest can grab the port in the gap and panic the fixture with\n`AddrInUse`.\n\nIntroduce `bind_random_free_port`, which returns the listener\n*together with* its port and is stored on the fixture until\n`run_service` consumes it. The socket stays open across the previously-\nracy window, so no other process can claim it.\n\n* test: reclassify two flaky tests as heavy3\n\nBoth `pending_chunks_test` (multi-node mempool) and\n`heavy_should_broadcast_chunk_data` (p2p gossip) stand up enough\nconcurrent nodes/services that running them at the default\n`threads-required = 1` (or `2`) lets nextest schedule too many\nneighbours alongside, producing intermittent timeouts in CI.\n\nRename to `heavy3_*` so the existing nextest override in\n`.config/nextest.toml` reserves 3 thread slots each, reducing\nco-scheduled load. Test bodies are unchanged.\n\n* perf(irys-reth/tests): cap cross-block cache at 64 MB in test harness\n\nReth's `engine.cross_block_cache_size` defaults to 4 GiB per node, and\nthe payload processor allocates proportionally to that budget once it\nstarts executing payloads. The result is that every irys-reth test\nthat mines even one block paid ~2.3 GB of peak RSS for cache the test\nnever actually populated — and tests that mine on both nodes and then\nfork (e.g. `heavy_rollback_state_revert_on_fork_switch`) doubled that\nto ~4.5 GB.\n\nOverride the budget to 64 MB inside `setup_irys_reth`. The function\nlives in `#[cfg(any(feature = \"test-utils\", test))] pub mod test_utils`\nand has no production callers, so the change is behavior-test-only.\n\nMeasured per-test peak RSS on representative tests (3-run average):\n\n  test_fee_only_shadow_tx::case_1_unpledge          2.3 GB → 188 MB  (12.2x)\n  test_fee_only_shadow_tx::case_2_unstake_debit     2.3 GB → 185 MB  (12.4x)\n  heavy_test_pledge_balance_decrement               2.3 GB → 211 MB  (10.9x)\n  heavy_rollback_state_revert_on_fork_switch        4.5 GB → 276 MB  (16.3x)\n\nAll 39 irys-reth tests pass under the smaller cap. Eliminates the 2T\ntier's 29.6 GB worst-case memory contention previously surfaced by\n`nextest-report analyze`, since the ~50 `tests::heavy_*` entries at the\nold 2.3 GB plateau now sit near 250 MB.",
          "timestamp": "2026-05-28T13:29:35+01:00",
          "tree_id": "bb62969a8082d68d410dce6b7638a025402b3d24",
          "url": "https://github.com/Irys-xyz/irys/commit/04bfa3f73f1e7ae6a59505f9cf0e550ad842a045"
        },
        "date": 1779972400964,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011886,
            "range": "± 0.000265",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.120318,
            "range": "± 0.001518",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.219405,
            "range": "± 0.031576",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.88086,
            "range": "± 0.062348",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.074781,
            "range": "± 0.000934",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 757.672032,
            "range": "± 4.452048",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 979.443273,
            "range": "± 10.966032",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.146049,
            "range": "± 0.001705",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1202.555523,
            "range": "± 139.585042",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1541.543664,
            "range": "± 17.688206",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034456,
            "range": "± 0.001082",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.275664,
            "range": "± 0.73421",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.985767,
            "range": "± 1.575868",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000128,
            "range": "± 0.000002",
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
          "id": "f1b379109e866d8b3c12b4815d24810281ba2f1b",
          "message": "refactor(block_tree): compute reorg split via parent-walk LCA (#1432)\n\n* refactor(block_tree): compute reorg split via parent-walk LCA\n\n* fix(block_tree): abort deep reorgs before prune evicts the fork point\n\n* fix: address review findings",
          "timestamp": "2026-05-29T10:37:15+01:00",
          "tree_id": "e53e9257438adfeff8fd8d3b40dff12d0b6fe5c1",
          "url": "https://github.com/Irys-xyz/irys/commit/f1b379109e866d8b3c12b4815d24810281ba2f1b"
        },
        "date": 1780048436990,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012539,
            "range": "± 0.000451",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.126028,
            "range": "± 0.003613",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.285912,
            "range": "± 0.017206",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.04255,
            "range": "± 0.1399",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.076929,
            "range": "± 0.001747",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 761.473716,
            "range": "± 31.740215",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 980.498885,
            "range": "± 7.76988",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120086,
            "range": "± 0.003215",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1278.743037,
            "range": "± 100.774249",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1568.68169,
            "range": "± 16.196062",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035052,
            "range": "± 0.001124",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.112625,
            "range": "± 3.899796",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 323.990314,
            "range": "± 18.055859",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000152,
            "range": "± 0.000006",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "8431d9a480e3adb85a1fb8ffcbba7e791936198d",
          "message": "release 3.0.1 (#1433)\n\nfeat: bump chain version",
          "timestamp": "2026-05-29T10:53:43+01:00",
          "tree_id": "f5ffb70407d889eb024496b27911f26e3c20d822",
          "url": "https://github.com/Irys-xyz/irys/commit/8431d9a480e3adb85a1fb8ffcbba7e791936198d"
        },
        "date": 1780049384774,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012638,
            "range": "± 0.000409",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.130856,
            "range": "± 0.006129",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.564953,
            "range": "± 0.086425",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.809271,
            "range": "± 0.32312",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.07866,
            "range": "± 0.001249",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 828.301058,
            "range": "± 18.702317",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1028.65286,
            "range": "± 43.400371",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.128396,
            "range": "± 0.014742",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1538.890023,
            "range": "± 74.42555",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1763.204531,
            "range": "± 175.473398",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.132967,
            "range": "± 0.03775",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 244.004463,
            "range": "± 6.049873",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 280.540575,
            "range": "± 4.353969",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000117,
            "range": "± 0.000006",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "a045ba99ae387d589abdeefa37c7ed8b2cbc4e6e",
          "message": "feat: Per-version frozen release branches + tag-trust rebuild provenance (#1434)\n\n* fix(release): trust release tag on rebuild provenance, skip rebased-away branch check\n\n* feat(release): push frozen release/<env>/<version> branch on publish\n\n* fix(release): fail-safe devnet provenance guard; sharpen frozen-branch comments\n\n- verify-commit-provenance: explicitly reject devnet + require-release-tag=true\n  (would otherwise skip both the branch and version/tag checks, leaving only\n  commit-existence; unreachable today but now fails safe on its own).\n- release.yml: correct the frozen-branch comments — immutability rests on the\n  validate job's tag-existence gate (the non-force push is only a non-fast-forward\n  backstop), and the branch's unique benefit is clearing GitHub's orphaned-commit\n  flag (the tag already provides reachability/GC-safety).\n\n* fix(release): surface blocked frozen-branch rollback-delete instead of swallowing it\n\nThe 'release branches' ruleset still blocks github-actions[bot] from deleting\nrelease/<env>/<version> on rollback. Removing the '|| true' makes that leave-behind\nfail loudly with an actionable message (admin must remove the stray branch) rather\nthan silently passing. Local branch -D stays best-effort (missing local ref is benign).",
          "timestamp": "2026-05-29T14:20:52+01:00",
          "tree_id": "2d20dd021674bd1d2e68f08a62776a14861faaa4",
          "url": "https://github.com/Irys-xyz/irys/commit/a045ba99ae387d589abdeefa37c7ed8b2cbc4e6e"
        },
        "date": 1780062368613,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012533,
            "range": "± 0.000398",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.125549,
            "range": "± 0.0045",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.267793,
            "range": "± 0.026965",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.294281,
            "range": "± 0.311395",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.07859,
            "range": "± 0.001221",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 767.512108,
            "range": "± 20.732908",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 974.364772,
            "range": "± 50.149861",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.12034,
            "range": "± 0.000686",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1212.105711,
            "range": "± 11.132751",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1573.20746,
            "range": "± 5.07036",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034648,
            "range": "± 0.001641",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.097664,
            "range": "± 1.988014",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.971745,
            "range": "± 1.845003",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0.000004",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "12614c191aab4c66e708b1cabf4bd8e4985332c6",
          "message": "feat(p2p): hard-reject handshakes from a different chain_id (#1435)\n\n* feat(p2p): hard-reject handshakes from a different chain_id\n\nPreviously a handshake declaring a different chain_id was accepted with\nonly an advisory consensus-config-hash mismatch log. Now both the V1 and\nV2 handshake handlers hard-reject when the peer's chain_id differs from\nours, checked before signature verification as a cheap network-membership\ngate. The consensus-config-hash mismatch stays advisory.\n\nAdds a dedicated RejectionReason::ChainIdMismatch variant, kept distinct\nfrom ProtocolMismatch (a chain mismatch is not a protocol mismatch) and\nfrom the advisory config-hash check. It serializes as a bare unit string\nfor v1 wire compatibility and is only ever emitted to peers that declared\na different chain_id, so same-chain peers running older (e.g. Dec-2025)\nbuilds never receive it and are unaffected.\n\nWhen we are rejected with ChainIdMismatch during our own handshake it maps\nto a terminal NetworkMismatch rejection rather than a retryable request\nerror, so we stop announcing to cross-chain peers instead of retrying.\n\n* test(p2p): assert chain_id is checked before signature verification\n\nAdd a handshake case with a foreign chain_id and a zeroed (invalid)\nsignature, asserting the rejection is ChainIdMismatch rather than\nInvalidCredentials. Guards against ever reordering the chain_id check\nafter signature verification.",
          "timestamp": "2026-06-01T22:24:47+01:00",
          "tree_id": "69ef4d01512fbc73a96016e86446e6d24c6816cb",
          "url": "https://github.com/Irys-xyz/irys/commit/12614c191aab4c66e708b1cabf4bd8e4985332c6"
        },
        "date": 1780350331527,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012592,
            "range": "± 0.000486",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.120043,
            "range": "± 0.002043",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.221836,
            "range": "± 0.029055",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.205747,
            "range": "± 0.36871",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075365,
            "range": "± 0.000642",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 775.176716,
            "range": "± 23.777338",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 971.248261,
            "range": "± 2.264051",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120027,
            "range": "± 0.00163",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1219.874574,
            "range": "± 14.365903",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1591.080425,
            "range": "± 13.569885",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034886,
            "range": "± 0.001867",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.687363,
            "range": "± 1.509687",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.610274,
            "range": "± 1.752653",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "4773009ca0b8e436f94c1e3e60990691a1c2adb5",
          "message": "fix(block_producer): count test block budget by height, not blocks produced (#1436)\n\n`blocks_remaining_for_test` is meant to bound how many canonical *height\nincrements* a test mines (every caller — `mine_blocks`/`mine_block_with_payload`\n— waits for `start_height + n`). It was decremented once per block *produced*.\n\nUnder autonomous test mining (`start_mining()` + multiple partitions + fast\nVDF) the producer can build two blocks on the same parent before the first is\nvalidated and becomes the canonical tip — a duplicate-height sibling. The\nsibling consumed a budget slot without advancing the height, so production\nstopped one height short of the target and `mine_blocks` /\n`wait_for_block_at_height` hung until the 60s nextest kill. This surfaced as a\nCI flake in `heavy3_pending_chunks_test` (FAIL on attempt 1, PASS on re-run).\n\nDecrement the budget only when a produced block advances to a height not seen\nbefore in the current mining phase, tracked by `highest_test_block_height_produced`\n(reset whenever the budget is (re)set). The sibling is still produced/gossiped\nexactly as before — only the budget accounting changes — so fork-choice and\ngossip behaviour are untouched.\n\nAdds `apply_test_budget_after_production` (pure, unit-tested) with a regression\ntest replaying the exact CI sequence (heights 2,3,3,4,5,6,7 with budget 6 still\nreaches height 7).",
          "timestamp": "2026-06-02T14:18:09+01:00",
          "tree_id": "f65b855482ec8accc0fbda39ed4d20fb8d7146bf",
          "url": "https://github.com/Irys-xyz/irys/commit/4773009ca0b8e436f94c1e3e60990691a1c2adb5"
        },
        "date": 1780407689811,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011957,
            "range": "± 0.000218",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.122743,
            "range": "± 0.002741",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.26227,
            "range": "± 0.03556",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.21921,
            "range": "± 0.328673",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.076493,
            "range": "± 0.001625",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 783.690637,
            "range": "± 31.476664",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 970.549715,
            "range": "± 4.686806",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.118957,
            "range": "± 0.001413",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1207.873491,
            "range": "± 12.670862",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1583.419983,
            "range": "± 19.766776",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034153,
            "range": "± 0.00189",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.683423,
            "range": "± 1.871831",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.842106,
            "range": "± 3.240497",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "b6b5133e447054850b6226d0e2375b1ea38c2e2c",
          "message": "docs(block_producer): correct test-budget mechanism narrative (#1438)\n\n* docs(block_producer): correct test-budget mechanism narrative\n\nFollow-up to #1436. The fix landed correct, but its comments, test names, and\ndescription attributed the duplicate-height production to \"two valid solutions\nbuilt on the same parent before the first was validated — a sibling.\"\n\nTracing the CI logs shows the real mechanism: the first height-3 block was\n*published* (consuming a budget slot), then failed prevalidation with\n`DuplicateTransaction` because it re-included a transaction already confirmed in\na recent block (the producer's mempool view hadn't caught up under fast mining),\nand was rebuilt at the same height. The parent-selection tracker is not at\nfault — it correctly reverts to the prior tip when a published block is rejected.\n\nComments and test names only; no behaviour change.\n\n* docs(block_producer): use consistent 'replacement' terminology in budget comment\n\nAddress review on #1438: the inline comment in the SolutionFound handler still\ncalled the rejected block a 'sibling', inconsistent with the 'same-height\nreplacement' wording used in the surrounding doc comments and tests. Reword it\n(and the lone remaining 'sibling' in a regression-test assertion) so the\nnarrative is consistent; still describes a published block failing its own\nprevalidation. Comments/test-message only; no behaviour change.",
          "timestamp": "2026-06-02T18:24:09+01:00",
          "tree_id": "65a1d4005c0288a766f6e4c68e3001801bff879f",
          "url": "https://github.com/Irys-xyz/irys/commit/b6b5133e447054850b6226d0e2375b1ea38c2e2c"
        },
        "date": 1780422054160,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015308,
            "range": "± 0.000283",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.125799,
            "range": "± 0.005085",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.282258,
            "range": "± 0.060547",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.101749,
            "range": "± 0.253179",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075993,
            "range": "± 0.001183",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 753.091946,
            "range": "± 7.422324",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 982.234811,
            "range": "± 8.727628",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.119917,
            "range": "± 0.003058",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1218.857434,
            "range": "± 108.484616",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1545.777315,
            "range": "± 9.943561",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.03485,
            "range": "± 0.001061",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.341872,
            "range": "± 1.015887",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 274.237135,
            "range": "± 2.834533",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.000003",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "60908709ecfc3922e6af3ed3e941f9ef1f1115f1",
          "message": "feat(p2p): evict peers on chain_id (network) mismatch handshake rejection (#1437)\n\n* feat(p2p): evict peers on chain_id (network) mismatch handshake rejection\n\nA handshake rejected for a chain_id mismatch (mapped to NetworkMismatch) only\nrecorded a failed announcement and left the peer in the cache. The gossip data\nplane (check_peer_v*) trusts cache membership rather than handshake outcome, so\nthe rejected peer stayed fully trusted and the node kept exchanging gossip with\na different network.\n\nNow a NetworkMismatch handshake rejection evicts the peer from the in-memory\ncache (all lookup maps) and deletes it from the persistent peer DB, so a node\nre-announcing to its cached peers while on the wrong chain is isolated after the\nstartup announce round.\n\n- domain: PeerList::remove_peer_by_api_address removes a peer from the cache and\n  every index map, emitting PeerRemoved.\n- database: delete_peer_list_item removes a peer from the PeerListItems table.\n- p2p: evict_peer_on_network_mismatch wired into the outbound-announce rejection\n  path; no-op for any non-network rejection reason.\n\nIncludes a design doc covering the diagnosis and the larger follow-up\n(session-scoped handshake + authenticated handshake response).\n\n* fix(p2p): route chain-mismatch eviction delete through flush to avoid resurrect race\n\nevict_peer_on_network_mismatch deleted the peer in its own db.update_scoped\ntransaction, which raced with the periodic flush: if flush had already\nsnapshotted persistable_peers before the eviction removed the peer from the\ncache, flush's insert re-inserted the just-deleted peer, resurrecting it in the\nDB (reloaded on the next restart).\n\nMake flush the sole peer-DB writer. Eviction now removes the peer from the\nin-memory cache and stages its peer_id in PeerNetworkServiceState.pending_db_removals;\nflush drains the set and, within its single transaction, skips re-inserting any\nstaged peer and deletes them. The in-memory eviction stays immediate, so the\ngossip data plane stops trusting the peer at once.\n\n* fix(p2p): propagate flush DB errors and retry staged peer removals\n\nflush() discarded the inner transaction Result via `let _ =`, silently\ndropping insert/delete failures (it could even report success after a failed\ndelete), and it `mem::take`d pending_db_removals before the write so a failed\nflush lost the staged chain-mismatch eviction deletes entirely.\n\nFlatten the nested update_scoped result so inner PeerListServiceError values\npropagate, and re-stage removals (merge, to preserve evictions staged during\nthe lock-free write window) when the flush fails so the next flush retries.\nDeletes are idempotent, so the retry is safe.\n\n* fix(p2p): take flush peer snapshot and staged removals under one lock\n\nflush() snapshotted persistable_peers (peer-list lock) and drained\npending_db_removals (state lock) separately, so a concurrent NetworkMismatch\neviction could be observed half-applied and a just-evicted peer re-persisted.\n\nTake both under the same state lock, and perform the eviction's cache removal and\nremoval staging under that same lock, so flush always sees a consistent\n(snapshot, removals) pair. Lock ordering is state -> peer_list on both paths\n(peer-list methods never acquire the state lock), so no deadlock. The lock is\nreleased before the DB write; an eviction during that lock-free write can still\nre-persist a peer for one flush cycle, which is benign (the in-memory eviction is\nalready in effect and the next flush deletes it).\n\n* docs: reflect staged-removal flush design for chain-mismatch eviction\n\nThe near-term design's step 3 described an inline DB delete in the eviction path.\nThe implementation defers the delete: eviction removes the peer from the cache\nand stages its id in pending_db_removals (under the same state lock flush takes),\nand flush — the sole peer-DB writer — applies delete_peer_list_item in its\ntransaction. Update step 3 to match.\n\n* fix(p2p): record handshake rejections as failed, not successful, announcements\n\nannounce_yourself_to_address sent AnnouncementFinished{success:true} for any\ntransport-level Ok response, including PeerResponse::Rejected, so a rejection\n(e.g. NetworkMismatch) was cached in successful_announcements and suppressed\nfuture announce attempts as if it had succeeded.\n\nMove the success notification into the Accepted arm; the Rejected arm now reports\nsuccess:false, retry:false (terminal — re-announcing won't change the peer's\nmind) before evicting on NetworkMismatch and returning PeerHandshakeRejected.\n\n* test(domain): cover gossip-index clearance and PeerRemoved event on eviction\n\nremove_peer_by_api_address_clears_all_lookups now also asserts the gossip-address\nindex returns None after eviction and that a PeerRemoved event carrying the\nevicted peer_id is emitted (subscribed before the removal).",
          "timestamp": "2026-06-02T19:03:22+01:00",
          "tree_id": "b55921fb1fb630d966da7178ecbc08e012d704d8",
          "url": "https://github.com/Irys-xyz/irys/commit/60908709ecfc3922e6af3ed3e941f9ef1f1115f1"
        },
        "date": 1780424393636,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.01541,
            "range": "± 0.000973",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.157739,
            "range": "± 0.006252",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.546486,
            "range": "± 0.050209",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.485021,
            "range": "± 0.588344",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.081063,
            "range": "± 0.005738",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 783.390771,
            "range": "± 15.950145",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 989.852393,
            "range": "± 14.003064",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120406,
            "range": "± 0.001273",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1240.338794,
            "range": "± 77.524402",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1566.453597,
            "range": "± 18.295209",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034984,
            "range": "± 0.001999",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.71765,
            "range": "± 1.391869",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.026712,
            "range": "± 1.419742",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000115,
            "range": "± 0.000005",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "24f97ea779520956cc4fbc23ba044fd71e063865",
          "message": "docs: route hotfixes through the release line, not master-first (#1440)\n\nHotfixes targeted code that is already deployed (the release line, which\nsits behind master), but the docs said to fix on master first and\ncherry-pick down. Develop a fix against ahead-of-production master code\nand it may not apply cleanly downward, and it drags the fix through\nunstable, unreleased work.\n\nInvert the direction for every fix path: originate the fix on\nrelease/<major>.x (the env-agnostic source of truth), bump the version\nthere, then backport (cherry-pick) up to master and merge forward to the\ndeployment branches. Distinguish three paths in RELEASE_PROCESS.md:\nshared-code (backport to master, reach every env), env-specific (commit\non the env branch, no backport - it would contaminate shared branches),\nand emergency (force straight to mainnet, then reconcile).\n\nApply the same story to the Process-in-Action narrative and to both\nRELEASE_PLAYBOOK.md tables (Phase B 'if testnet fails' and the quick\ndecision points). The normal feature-curation flow (master -> release)\nis unchanged.",
          "timestamp": "2026-06-02T19:40:51+01:00",
          "tree_id": "36c708e88ded25c9b85e080c9cf96b799404be7e",
          "url": "https://github.com/Irys-xyz/irys/commit/24f97ea779520956cc4fbc23ba044fd71e063865"
        },
        "date": 1780426788731,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011839,
            "range": "± 0.00008",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.11876,
            "range": "± 0.001387",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.191431,
            "range": "± 0.022499",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.987778,
            "range": "± 0.191582",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.074968,
            "range": "± 0.000765",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 788.75127,
            "range": "± 14.079835",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1069.0921,
            "range": "± 25.671628",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120181,
            "range": "± 0.000569",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1205.822501,
            "range": "± 9.361914",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1576.575197,
            "range": "± 20.28366",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034149,
            "range": "± 0.001156",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.785229,
            "range": "± 2.183321",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 275.823491,
            "range": "± 1.709148",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000114,
            "range": "± 0.000003",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "401d54f7a285ed602cabb114b5a44683f09d7c39",
          "message": "feat: release 3.0.2 (#1441)",
          "timestamp": "2026-06-03T12:04:26+01:00",
          "tree_id": "2b18d107736a023cf69f601c23618244ef3590aa",
          "url": "https://github.com/Irys-xyz/irys/commit/401d54f7a285ed602cabb114b5a44683f09d7c39"
        },
        "date": 1780485687971,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015288,
            "range": "± 0.00138",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.127254,
            "range": "± 0.005647",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.271291,
            "range": "± 0.0521",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.327121,
            "range": "± 1.124641",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075222,
            "range": "± 0.001529",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 757.514103,
            "range": "± 12.74413",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 992.838869,
            "range": "± 8.918683",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.121315,
            "range": "± 0.003178",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1280.689213,
            "range": "± 113.667606",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1572.975184,
            "range": "± 109.346444",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034298,
            "range": "± 0.001338",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.542777,
            "range": "± 1.746162",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 284.738587,
            "range": "± 15.58587",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000148,
            "range": "± 0.000002",
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
          "id": "38f81f1dfb1ea2bc3fdbae2896453da146be0f22",
          "message": "feat(chain): VDF throughput check at node startup (#1424)\n\nRun a single VDF step benchmark before the node fully starts. Three\noutcomes based on the configured sha_1s_difficulty and block_time:\n\n- Full efficiency: CPU completes a VDF step within the 1s target.\n  Node starts normally.\n- Reduced efficiency: CPU keeps up with block production but VDF\n  steps take longer than 1s, reducing mining competitiveness. Logs\n  a warning with the efficiency percentage and suggests a faster CPU.\n- Cannot keep up: CPU is too slow to complete enough VDF steps within\n  the block time. Logs an error and aborts startup before spinning\n  up Reth, the block tree, or any services.\n\nAll thresholds are derived from consensus config (sha_1s_difficulty,\nblock_time, num_checkpoints_in_vdf_step) — no hardcoded values.",
          "timestamp": "2026-06-03T07:00:23-07:00",
          "tree_id": "0508604822be66b851aa6f2ac77d80ff5fba4afb",
          "url": "https://github.com/Irys-xyz/irys/commit/38f81f1dfb1ea2bc3fdbae2896453da146be0f22"
        },
        "date": 1780496255044,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015363,
            "range": "± 0.000797",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.154161,
            "range": "± 0.011381",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 2.036843,
            "range": "± 1.246836",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.446179,
            "range": "± 0.547609",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078851,
            "range": "± 0.000729",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 774.96835,
            "range": "± 26.702077",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 974.883887,
            "range": "± 10.724306",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.132277,
            "range": "± 0.00328",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1355.158729,
            "range": "± 100.339669",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1637.486164,
            "range": "± 136.309274",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.033881,
            "range": "± 0.001532",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.763441,
            "range": "± 2.43662",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.379444,
            "range": "± 1.536658",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000118,
            "range": "± 0.000003",
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
          "id": "b099906678361388b24ddef86e589da54018f40f",
          "message": "feat: Network partition recovery: deep reorg support + integration tests (#1405)\n\n* fix(error): log when a reorg is passed the migration boundary\n\n* fix(storage): map OneYear/ThirtyDay ledgers to storage modules and migrate their chunks\n\nmap_storage_modules_to_partition_assignments() only processed Publish,\nSubmit, and Capacity partitions — OneYear and ThirtyDay assignments\nexisted in the epoch snapshot but were never forwarded to the\nStorageModuleService.\n\non_block_migrated() only extracted Submit and Publish ledger transactions\nduring chunk migration, so OneYear and ThirtyDay chunks were never\nwritten to storage modules.\n\n* feat(block-tree): recover from network partition on deep reorg\n\nWhen a reorg exceeds block_migration_depth, blocks have already been\nmigrated to the block index with chunks assigned to storage module\npartitions. recover_from_network_partition() handles this by:\n\n1. Collecting orphaned block chunk ranges, tx data_roots, and rewards\n2. Clearing ChunkPathHashesByOffset and DataRootInfosByDataRoot entries\n   for orphaned offsets/transactions only (preserving common ancestor data)\n3. Marking orphaned offsets as Uninitialized so repacking can proceed\n4. Truncating the block index to the fork point\n5. Rolling back supply state cumulative_emitted\n\nThe normal migration path then re-processes the new canonical chain.\n\n* fix: Guard against stale BlockMigrated messages from orphaned forks.\n\n* test: network partition recovery test\n\n* fix(block-pool): allow deep reorg candidates through gossip\n\nThe block pool previously rejected all blocks at already-indexed heights\nwith different hashes as PartOfAPrunedFork. This prevented gossip-based\ndelivery of competing fork blocks during deep reorgs, forcing callers to\nbypass the block pool entirely via send_full_block.\n\nThe block tree already handles deep reorgs correctly via\nrecover_from_network_partition — the block pool was the only barrier.\n\nChange block_status() to classify blocks at indexed heights with\ndifferent hashes based on proximity to the latest index:\n- In the tree already → ProcessedButCanBeReorganized (fork in progress)\n- Within block_tree_depth → NotProcessed (potential deep reorg candidate)\n- Beyond block_tree_depth → PartOfAPrunedFork (DoS protection)\n\nThis resolves the TODO: \"this needs to handle migrated block reorgs\".\n\n* test(partition-recovery): switch to gossip delivery for reorg\n\nReplace send_full_block with gossip_block_to_peers for delivering the\npeer's fork blocks to genesis. This exercises the real P2P code path\nthat production nodes use when reconnecting after a network partition:\ngossip header → pull block body → pull ETH payload → validate → reorg.\n\nRemove the manual data tx/chunk pre-ingestion since the gossip pull\nmechanism handles block body (including tx headers) automatically.\n\n* test: partition assignment rollback on epoch boundary reorg\n\nAdds heavy4_partition_recovery_epoch_boundary which verifies that\npartition assignments are correctly rolled back when a reorg crosses\nan epoch boundary where each fork processed different pledge\ncommitments. Tracks the specific affected storage module through the\nreorg and asserts it gets reassigned to a canonical partition with\nEntropy intervals from re-packing.\n\n* style: fix formatting after merge\n\n* fix: clarify pledge comment in epoch boundary test\n\n* feat(block-tree): log error-level alert on network partition recovery\n\nOperators and monitoring agents need a clear signal when a node recovers\nfrom a network partition. This adds an ERROR log with structured fields\n(fork_depth, new_fork_depth, fork_height, current_height) and actionable\nguidance to investigate peer connectivity.\n\n* test: add multi-epoch reorg and deep recovery integration tests\n\nTwo new tests based on Codex security review findings:\n\n- partition_recovery_multi_epoch: reorg crossing 2 epoch boundaries,\n  verifies epoch snapshot correctness, partition assignment rollback\n  across multiple epochs, and continued mining after reorg.\n\n- partition_recovery_deep: deep reorg triggering recover_from_network_partition\n  with block_migration_depth=1, verifies block index consistency, supply\n  state rollback/re-migration, chain linkage across recovery boundary,\n  and continued block production.\n\n* refactor(tests): merge deep recovery checks into partition_recovery test\n\nRemove the redundant partition_recovery_deep test and add its unique\nchecks (block index hash verification, chain linkage across recovery\nboundary, continued mining + supply state after recovery) to the\nexisting heavy4_network_partition_recovery test.",
          "timestamp": "2026-06-03T07:01:05-07:00",
          "tree_id": "ef8d62c78f34a971ef39d24710024d04a7d2cb87",
          "url": "https://github.com/Irys-xyz/irys/commit/b099906678361388b24ddef86e589da54018f40f"
        },
        "date": 1780497155313,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011884,
            "range": "± 0.000095",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.125076,
            "range": "± 0.005322",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.207439,
            "range": "± 0.055238",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.278709,
            "range": "± 0.276389",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075332,
            "range": "± 0.001191",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 759.691921,
            "range": "± 9.980727",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 971.047977,
            "range": "± 1.360388",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.119734,
            "range": "± 0.000294",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1219.420358,
            "range": "± 115.093159",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1561.091954,
            "range": "± 6.892762",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.036193,
            "range": "± 0.000956",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 213.843736,
            "range": "± 5.559356",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.093242,
            "range": "± 14.189331",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "897cfe3b9a2f5c519896bcf8b5350b82dacab2cf",
          "message": "feat(release): gate releases on green CI for the release/<major>.x merge-base (#1443)\n\nThe validate job verified provenance, version, and tag freshness, but never\nthat the code being released passed CI — that rested entirely on deployment-\nbranch protection at PR-merge time. Add a defense-in-depth gate: resolve the\nrelease/<major>.x merge-base of the release commit (the substantive upstream\ncode, env-patches aside) and require its CI to be fully green via a new\nrequire-green-ci composite action.\n\nA commit is fully green when it has at least one successful check run (or a\ngreen combined commit status), nothing still running, and no failed/cancelled/\ntimed-out conclusions; skipped/neutral are non-blocking, matching GitHub's\nrequired-status-check semantics. A commit with no CI results at all is a hard\nfailure — absence of a run is not proof of a passing run.\n\nThe gate applies to both testnet and mainnet releases. force=true (emergency\nhotfixes) bypasses it; dry_run=true skips it because the documented dry-run\nsetup may run before release/<major>.x exists. The validate job gains\nchecks:read and statuses:read to query the APIs.",
          "timestamp": "2026-06-04T13:09:43+01:00",
          "tree_id": "e645b40597d2c82c1607193ef543429059dedc7f",
          "url": "https://github.com/Irys-xyz/irys/commit/897cfe3b9a2f5c519896bcf8b5350b82dacab2cf"
        },
        "date": 1780576112649,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012565,
            "range": "± 0.000547",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.123029,
            "range": "± 0.00382",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.258368,
            "range": "± 0.028677",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.333035,
            "range": "± 0.145859",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078045,
            "range": "± 0.000717",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 782.931373,
            "range": "± 22.669294",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1021.354058,
            "range": "± 39.033722",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.151246,
            "range": "± 0.038507",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1391.015851,
            "range": "± 111.302717",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1670.372911,
            "range": "± 113.688208",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035609,
            "range": "± 0.004105",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 214.293601,
            "range": "± 2.183736",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 276.772097,
            "range": "± 3.627279",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.000007",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "09718282e7adbcff36c7d02e4c344657cdcd80d8",
          "message": "fix: backport CI hardening and tooling fixes from gateway (#1444)\n\n* ci: harden workflows with least-privilege tokens and injection-safe refs\n\nBackported from the gateway repo's shared CI setup:\n\n- drop persisted git credentials on all checkouts that don't need them\n  (the rust.yml gate keeps them — it does authenticated git fetches);\n  submodules in cargo-check clone anonymously (both are public)\n- explicit least-privilege permissions + per-ref concurrency on\n  conventional-pr; pull-requests:read on rust.yml for the gate's PR\n  description/association API queries\n- pass PR base/head refs to the gate script via env vars instead of\n  interpolating them into the script body (branch names may contain\n  shell metacharacters)\n- guard the associated-PRs jq query against non-array API error\n  responses so the gate doesn't hard-fail on permission hiccups\n- bump gate checkout to actions/checkout@v4\n\n* fix(nextest-monitor): skip CPU sampling under heaptrack, log stats-dir read errors\n\nBackported from the gateway repo:\n\n- when heap profiling, the monitored pid belongs to heaptrack rather\n  than the test binary, so CPU samples measured the profiler; gate CPU\n  monitoring on !heap_profile, mirroring the existing RSS behavior\n- AggregatedStats::load_or_default swallowed every stats-dir read error\n  via unwrap_or_default; keep the NotFound -> empty default but log\n  other IO errors, since they mean stats data is silently being lost\n\n* chore: lint all targets in xtask clippy/fmt, bump rustfmt edition to 2024\n\nBackported from the gateway repo:\n\n- xtask clippy (and clippy --fix) now pass --all-targets, picking up\n  benches and examples that were previously unlinted; matches the\n  documented pre-push check in CLAUDE.md and propagates to local-checks\n- xtask fmt --check now passes --all for parity with the fix path\n- rustfmt.toml edition 2021 -> 2024, matching the workspace edition;\n  verified no formatting churn (cargo fmt --all --check passes)\n\n* fix(xtask): shell-quote flaky runner args, stop masking pipeline failures\n\nBackported from the gateway repo:\n\n- shell-quote all args interpolated into the bash -c command lines of\n  the flaky runner (args with spaces/metacharacters were re-split or\n  executed by the shell)\n- set -o pipefail so a cargo-flake failure is no longer masked by tee's\n  exit status; `cargo xtask flaky --save` now exits nonzero when flaky\n  tests are found (flaky.yml already tolerates this via set +e +\n  continue-on-error)\n- only fall back to plain tee when the `script` binary is actually\n  missing — with pipefail, a genuine test failure also surfaces as an\n  error, and rerunning the whole flaky suite on it would be expensive\n- route the bash -c invocations through remove_ring_env_vars like the\n  other cargo invocations\n- generate_nextest_config: fail fast on configs with an `experimental`\n  key missing \"wrapper-scripts\" instead of warning and emitting a\n  config nextest rejects (unreachable with our checked-in nextest.toml)",
          "timestamp": "2026-06-04T14:53:21+01:00",
          "tree_id": "19b370d3a200025c6da1579bf8f1d1a591138afb",
          "url": "https://github.com/Irys-xyz/irys/commit/09718282e7adbcff36c7d02e4c344657cdcd80d8"
        },
        "date": 1780582097516,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.014833,
            "range": "± 0.00011",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.155141,
            "range": "± 0.007197",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.56032,
            "range": "± 0.047112",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.481933,
            "range": "± 0.33328",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.082999,
            "range": "± 0.001172",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 817.325943,
            "range": "± 31.50565",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 975.203851,
            "range": "± 5.408295",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.118488,
            "range": "± 0.002075",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1202.299594,
            "range": "± 83.132347",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1538.28661,
            "range": "± 14.671171",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034513,
            "range": "± 0.002087",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.737986,
            "range": "± 0.9648",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 272.821148,
            "range": "± 1.296459",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.000002",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "distinct": true,
          "id": "8dee75faf4a28dc1a59d2794965e7d8a1e79c2e8",
          "message": "release: 3.0.4\n\nRelease version 3.0.4",
          "timestamp": "2026-06-11T10:42:32+01:00",
          "tree_id": "714a0fcb583cf84f932c263083a737a761a42ae4",
          "url": "https://github.com/Irys-xyz/irys/commit/8dee75faf4a28dc1a59d2794965e7d8a1e79c2e8"
        },
        "date": 1781171864543,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015787,
            "range": "± 0.001053",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.152177,
            "range": "± 0.003709",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.561092,
            "range": "± 0.030633",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.987088,
            "range": "± 0.500002",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.07876,
            "range": "± 0.000878",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 836.902272,
            "range": "± 19.657704",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1082.378201,
            "range": "± 36.149397",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.126906,
            "range": "± 0.000335",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1235.938106,
            "range": "± 18.216931",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1594.791022,
            "range": "± 103.299742",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.03449,
            "range": "± 0.000716",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.403923,
            "range": "± 1.387599",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.303673,
            "range": "± 1.593717",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000119,
            "range": "± 0.000004",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fe96706fcd2562671da34990e8e7607f30cff7e8",
          "message": "fix: clock drift/monotonic timers (#1402)\n\n* fix: harden logic against clock drift\n\nIn certain conditions, i.e WSL2, wallclock time can notably drift - this causes test failures.\nThis commit hardens relevant logic so that we use monotonic clocks where possible.\n\n* fix: update multiversion testing to not fail fast\n\n* docs: fix missing line in README\n\n* fix: unchecked cast\n\n* fix: pass multiversion old_ref via env to prevent shell injection\n\nold_ref was interpolated directly into the run: script, so a maliciously named git ref could inject shell. Route it through an env var and reference it as a quoted variable.\n\n* fix: harden wait_for_wallclock against final-poll lurch and tight deadline\n\nRe-check realtime after the hold loop so a backward lurch in the last poll interval restarts the wait instead of returning a false success. Raise the deadline 60s -> 120s to absorb large WSL2 time-sync lurches.\n\n* refactor: type rate-limiter dedup window as Duration\n\nThread Duration through check_request / handle_get_data* and the default constant instead of u128 milliseconds, removing the per-call try_from().expect() conversion.\n\n* refactor: use saturating_duration_since in cleanup_if_needed\n\nUse the documented never-panic API instead of relying on duration_since's\ncurrent saturating behavior, which std docs warn may reintroduce a panic\nin future versions. Makes the elapsed-or-zero intent explicit and\nconsistent with the sibling .elapsed() calls. No behavioral change.",
          "timestamp": "2026-06-16T17:25:16+01:00",
          "tree_id": "535af0f938fa58c0fa2972284677b0b6cc4257aa",
          "url": "https://github.com/Irys-xyz/irys/commit/fe96706fcd2562671da34990e8e7607f30cff7e8"
        },
        "date": 1781628066108,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.01532,
            "range": "± 0.000414",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.152872,
            "range": "± 0.003777",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.559771,
            "range": "± 0.041093",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.500449,
            "range": "± 0.515754",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078852,
            "range": "± 0.000965",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 778.271037,
            "range": "± 25.404372",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 977.617892,
            "range": "± 7.000562",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.128505,
            "range": "± 0.003273",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1227.323256,
            "range": "± 84.262947",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1567.584905,
            "range": "± 12.648957",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035588,
            "range": "± 0.000996",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.396057,
            "range": "± 1.151866",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 272.988639,
            "range": "± 2.012836",
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
          "id": "4c1d92850b7cf0b83502495c9441ad69075f0b97",
          "message": "fix: align genesis cascade activation with the epoch-layer predicate (#1448)\n\n* fix(consensus): align genesis cascade activation with epoch predicate\n\n* fix: address review findings\n\n* fix: address review findings\n\n* fix: address review findings\n\n* fix: address review findings\n\n* fix(data_sync): source ledger-absence check from tree consensus config\n\n* fix(consensus): dedupe term-ledger absence classification and harden lookups\n\nAddress review findings on the Cascade genesis branch:\n\n- Centralize the data-ledger present / expected-absent / unexpected-absent\n  decision in IrysHardforkConfig::classify_data_ledger (DataLedgerLookup),\n  so the data-sync orchestrator and block-tree guard share one predicate and\n  cannot drift apart (the cross-layer drift behind the 2026-06-11 incident).\n- Downgrade the unexpected-absence log error!->warn!: the check is a\n  block-timestamp proxy for an epoch-aligned invariant, and a block's ledger\n  shape is already validated upstream, so it should never fire for a valid\n  block. Correct the ledger_absence_expected doc to state the proxy caveat.\n- Make chunk_migration_service's previous-block ledger lookup panic-safe: a\n  term ledger may legitimately be absent from the predecessor at a mid-chain\n  Cascade activation boundary; treat it as zero chunks instead of indexing.\n- Rework the data_sync regression test to exercise the real\n  Some(cascade)+pre-activation path (not merely cascade=None); add a\n  classify_data_ledger unit test.\n\n* docs(chunk_migration): correct start-offset comment\n\nThe comment claimed total_chunks was decremented to compute the start\noffset, but the code uses the previous block's total_chunks directly.\nThe -1 subtraction only applies to the end offset.\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-06-17T13:34:01+01:00",
          "tree_id": "27470043da2f6376edd4c4f895e4be16dc1ee3a9",
          "url": "https://github.com/Irys-xyz/irys/commit/4c1d92850b7cf0b83502495c9441ad69075f0b97"
        },
        "date": 1781700851299,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012896,
            "range": "± 0.000323",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.127451,
            "range": "± 0.004645",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.261637,
            "range": "± 0.047834",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.411819,
            "range": "± 1.072823",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083444,
            "range": "± 0.000651",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 784.711351,
            "range": "± 26.477395",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1073.959793,
            "range": "± 57.177689",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.118129,
            "range": "± 0.00285",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1209.355137,
            "range": "± 13.677269",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1578.916629,
            "range": "± 16.821283",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.036302,
            "range": "± 0.006477",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.817534,
            "range": "± 1.550791",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 279.087364,
            "range": "± 2.027766",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0.000004",
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
          "id": "16475bdb5c3520e086a97b2cda992796c139b9ae",
          "message": "feat(consensus): add prefix_hash to data tx and fold into ledger tx_root (#1450)\n\n* feat(consensus): add prefix_hash to data tx and fold into ledger tx_root\n\nAdds a signed per-tx `prefix_hash` and folds `hash_all_sha256([data_root,\nprefix_hash])` into each tx_root leaf, so an indexer holding only the\nblock-signature-sealed tx_root can trust every tx's prefix_hash without\nverifying individual tx signatures. Softfork (no block-header change;\nempty ledgers still fold to H256::zero()).\n\n- rename header_size -> prefix_size; add prefix_hash: H256\n- block validation recomputes tx_root and rejects TxRootMismatch\n- PoA data-ledger branch recovers the owning tx's data_root (the folded\n  leaf no longer yields it) and binds it via the fold\n- storage retrieval recovers data_root from a new submodule binding\n  table (tx_path_hash -> {data_root, prefix_hash}), verified by the fold\n\n* test: cover prefix_hash fold, signing, and tx_root mismatch\n\n- block.rs: fold load-bearing, compute_tx_root == merklize root,\n  indexer reconstruction, and fold == hash_all_sha256 (gateway-compat guard)\n- signature.rs: prefix_size/prefix_hash are covered by the tx signature\n- chain-tests: a block whose tx_root doesn't match its txs is rejected\n  with TxRootMismatch\n\n* fix(consensus): classify missing-ledger PoA lookup as node fault; clarify prefix field semantics\n\n- load_owning_tx_for_poa: a missing ledger in the owning block's header is a\n  local index/header inconsistency (bounds resolution already matched it), not a\n  bad block. Surface it as BlockBoundsLookupError (NodeFault) via ok_or_else\n  instead of unwrap_or_default falling through to the consensus-reject\n  PoAChunkOffsetOutOfTxBounds.\n- Note on prefix_size/prefix_hash that the network does NOT validate them against\n  the transaction data; they are signed and committed into tx_root so an off-chain\n  indexer holding the data can validate them. Align the fold_tx_root_leaf doc.\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-06-17T07:35:01-07:00",
          "tree_id": "f1c0a8d78d20610fc3510407d759a344497defb4",
          "url": "https://github.com/Irys-xyz/irys/commit/16475bdb5c3520e086a97b2cda992796c139b9ae"
        },
        "date": 1781708029958,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012598,
            "range": "± 0.000294",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.126328,
            "range": "± 0.007179",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.2629,
            "range": "± 0.021598",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.38424,
            "range": "± 0.381848",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078795,
            "range": "± 0.000663",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 800.134998,
            "range": "± 22.607153",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1027.742702,
            "range": "± 28.394931",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.122147,
            "range": "± 0.004616",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1269.321286,
            "range": "± 114.708103",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1564.990483,
            "range": "± 25.22997",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035051,
            "range": "± 0.001088",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 208.75348,
            "range": "± 2.139003",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 271.284299,
            "range": "± 0.23358",
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
          "id": "097e4d74e43fae8a1fb613dfa2290c0a7b6f66d2",
          "message": "fix(vdf): limit run-ahead via reset-seed confirmation gate (#1449)\n\n* fix(vdf): gate reset boundary on confirmed chain step\n\n* refactor(vdf): drop gate_step alias, scale reset-seed test window\n\n* fix(vdf): rate-limit the reset-boundary-gate warning\n\n* chore: update gitignore to ignore symlinks\n\n* chore: add doc comment\n\n* feat(config): guard reset_frequency against the VDF confirmation-gate floor\n\nThe #1447 reset-boundary gate parks the VDF loop at a boundary until the rotation block is confirmed (block_migration_depth deep). If a reset window is too small relative to that confirmation lag, honest mining wedges at the boundary. Config::validate() now requires reset_frequency >= 2 * block_migration_depth * block_time, with a regression test; production configs satisfy it by a wide margin.\n\nAlso clarify consensus-critical VDF comments: why the fast-forward path needs no reset-boundary gate, the warning-throttle reset behavior, and the CanonicalVdfSnapshot step-ordering invariant.\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-06-17T20:36:19+01:00",
          "tree_id": "bd21dad73348524b93a42a630acc0dc01f872908",
          "url": "https://github.com/Irys-xyz/irys/commit/097e4d74e43fae8a1fb613dfa2290c0a7b6f66d2"
        },
        "date": 1781725873950,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015553,
            "range": "± 0.000678",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.154045,
            "range": "± 0.007474",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.576179,
            "range": "± 0.083679",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.486623,
            "range": "± 0.41433",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.08219,
            "range": "± 0.003335",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 823.712562,
            "range": "± 13.681516",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1026.766441,
            "range": "± 36.927159",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.147643,
            "range": "± 0.010243",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1264.041077,
            "range": "± 42.334602",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1576.119145,
            "range": "± 184.199182",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.032874,
            "range": "± 0.000849",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.858571,
            "range": "± 1.0046",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.354245,
            "range": "± 1.53378",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000112,
            "range": "± 0.000002",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "3a5169809d16813d8cadfcb96ce8bda1d61e22c0",
          "message": "feat(consensus): reject zero-size, oversized-prefix, and foreign-chain-id transactions (#1451)\n\n* fix(consensus): reject zero-size data txs and harden PoA owning-tx recovery\n\nA data tx with `data_size == 0` was accepted by prevalidation (validate_transactions\nchecks only count/id/signature) and would inject a zero-width leaf into the ledger\n`tx_root` tree. Because `validate_path` resolves an exact-boundary target to the\nright-hand leaf while owning-tx recovery returned the first tx at that cumulative\nstart offset, a recall chunk following a zero-size tx was mis-attributed, yielding a\nspurious `PoaTxRootLeafMismatch` that made the following tx's data permanently\nun-minable. An end-to-end test confirms such a block otherwise prevalidates fully.\n\n- prevalidate_block now rejects any data ledger containing a `data_size == 0` tx\n  (new PreValidationError::ZeroSizeDataTx), closing the class at the consensus gate\n  so a hand-crafted peer block can't smuggle one past ingress.\n- load_owning_tx_for_poa skips zero-width leaves via a shared `is_poa_owning_leaf`\n  predicate wired into both the block-tree and DB-fallback scans (defense-in-depth).\n\nTests: unit tests for the zero-size scan and the owning-leaf predicate, plus an\nend-to-end prevalidate_block rejection test.\n\n* feat(mempool): reject zero-size data txs at ingress\n\nMirror the ZeroSizeDataTx consensus prevalidation guard at the mempool ingress gate\n(`precheck_data_ingress_common`, shared by the gossip and API paths) so a data tx with\n`data_size == 0` is never admitted or gossiped onward. A zero-size tx stores no data and\nwould inject a zero-width leaf into the ledger `tx_root` tree.\n\n- New `TxIngressError::ZeroDataSize(H256)`; the API routes map it to 400 Bad Request and\n  gossip maps it to InvalidData (source-reputation penalty) via\n  `InvalidDataError::TransactionZeroDataSize`.\n\nTest: end-to-end `test_api_rejects_zero_size_data_tx` asserts a hand-crafted signed\nzero-size data tx is rejected at ingress with `ZeroDataSize` (red→green verified).\n\n* feat(consensus): reject prefix_size > data_size and foreign chain_id txs\n\nAdd two structural transaction-validation rules, each enforced at BOTH\nmempool ingress and the authoritative consensus prevalidation gate, so a\nhand-crafted peer block cannot bypass ingress:\n\n- prefix_size > data_size (data txs): prefix_hash commits to the first\n  prefix_size data bytes, so prefix_size > data_size is a structurally\n  impossible claim. New PreValidationError::PrefixSizeExceedsDataSize and\n  TxIngressError::PrefixSizeExceedsDataSize.\n\n- chain_id mismatch (data AND commitment txs): chain_id is a signed field,\n  so a tx signed for another chain is rejected. New PreValidationError\n  variants DataTxChainIdMismatch / CommitmentChainIdMismatch and a shared\n  TxIngressError::ChainIdMismatch, plus a CommitmentTransaction::chain_id()\n  getter. The commitment consensus check applies to epoch and non-epoch\n  blocks.\n\nBoth rules are surfaced through the API routes (400) and p2p gossip\n(reputation decrease). Covered by unit tests for each finder helper and\nend-to-end ingress + prevalidation integration tests for data and\ncommitment transactions.\n\n* fix(types): stamp signer chain_id in data-tx builder\n\n`IrysSigner::merklize` built data-tx headers from `DataTransactionHeader::default()`, which leaves `chain_id = 0`, and no builder ever stamped the signer's chain_id. That was harmless until this branch added consensus/ingress `chain_id` enforcement, which then rejected every builder-produced data tx (chain_id 0 != node chain_id) — breaking the API end-to-end tests with HTTP 400.\n\nStamp `self.chain_id` onto the header in `merklize` (next to data_size/data_root) so builder-produced txs target the signer's configured chain. This conforms the Rust builder to the wire contract already reflected in the gossip fixtures (`chainId` is set), and is what makes the new chain_id rule pass for honestly-built transactions.\n\n* test: assign body.data_transactions instead of push in zero-size prevalidation test\n\nAligns `test_prevalidation_rejects_zero_size_data_tx` with its sibling tests,\nwhich all set `body.data_transactions = vec![..]`. Functionally equivalent today\n(the genesis test block carries no data transactions), but keeps per-test body\nconstruction uniform.\n\n* refactor(actors): share data-tx structural checks between ingress and consensus\n\nExtract the three data-transaction structural rules (zero data_size,\nprefix_size > data_size, foreign chain_id) into a single\n`data_tx_structural_defect` predicate in a new `data_tx_validation` module.\nBoth gates now call it:\n\n- mempool ingress (`precheck_data_ingress_common`)\n- consensus prevalidation (`prevalidate_block`)\n\nEach gate maps the returned `DataTxStructuralDefect` to its own error type\n(`TxIngressError` vs `PreValidationError`), so only the *rule* is shared. This\nguarantees ingress and consensus can't drift — a tx ingress accepts is\nguaranteed to clear the same structural checks at consensus, and vice versa.\n\nAlso collapses the three separate per-check scans at the consensus gate into a\nsingle pass, and removes the three `first_*` finder helpers and their unit\ntests in favor of the shared predicate's consolidated unit tests. Commitment\nchain_id validation is left as-is (its only structural check is chain_id).",
          "timestamp": "2026-06-17T23:27:43+01:00",
          "tree_id": "0dd7bfa90220a2c0fab6801c81cbfb4aa0d82e74",
          "url": "https://github.com/Irys-xyz/irys/commit/3a5169809d16813d8cadfcb96ce8bda1d61e22c0"
        },
        "date": 1781736399430,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011876,
            "range": "± 0.000337",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.119065,
            "range": "± 0.001981",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.19677,
            "range": "± 0.018069",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.902671,
            "range": "± 0.223711",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.077211,
            "range": "± 0.001726",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 777.847315,
            "range": "± 27.33068",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 969.387729,
            "range": "± 2.578212",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.11776,
            "range": "± 0.002365",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1188.997887,
            "range": "± 8.656565",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1572.500735,
            "range": "± 19.520035",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034084,
            "range": "± 0.001403",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 208.881893,
            "range": "± 1.562083",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 271.281361,
            "range": "± 0.25982",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "1190cb816aee659d18f5bdcf1ad1f0d935a3854e",
          "message": "fix(multiversion-tests): clamp data tx prefix_size to data_size (#1454)\n\nThe harness forced `prefix_size = 64` on every submitted data tx to\nexercise a non-default Compact encoding. After #1451 added the consensus\nrule rejecting `prefix_size > data_size`, every test payload (all < 64\nbytes) was rejected at `/v1/tx` with \"has prefix_size greater than\ndata_size\", failing all 8 multiversion tests deterministically.\n\nClamp the sentinel to `data_size.min(64)` so it respects the invariant\nwhile staying non-default (payloads are >= 38 bytes), keeping the\nround-trip check meaningful.",
          "timestamp": "2026-06-18T16:43:53+01:00",
          "tree_id": "a821cb6a35f7c512b741835891aebc41f06b6d7b",
          "url": "https://github.com/Irys-xyz/irys/commit/1190cb816aee659d18f5bdcf1ad1f0d935a3854e"
        },
        "date": 1781798400035,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015204,
            "range": "± 0.000304",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.155396,
            "range": "± 0.005214",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.582692,
            "range": "± 0.197607",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.380534,
            "range": "± 0.711808",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.084155,
            "range": "± 0.002763",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 790.681416,
            "range": "± 42.018573",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1004.366894,
            "range": "± 17.170075",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.121314,
            "range": "± 0.003196",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1238.376826,
            "range": "± 81.369799",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1566.937696,
            "range": "± 16.985094",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034274,
            "range": "± 0.00123",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.126963,
            "range": "± 2.124663",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.721846,
            "range": "± 1.133249",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000121,
            "range": "± 0.000003",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "63d0572b10b239a102ea54c0437ad2c21c59e2b0",
          "message": "fix(block-producer): reject sub-difficulty solutions before production (#1452)\n\nA mining solution valid for the difficulty at solve-time could be rebuilt\nonto a newer, higher-difficulty canonical parent without re-checking the\nsolution against that parent's difficulty. The rebuilt block then failed its\nown pre-validation (SolutionHashBelowDifficulty), which the producer treated\nas irrecoverable and panicked the block producer service.\n\nAdd a difficulty gate to the production path, mirroring the validator's\nsolution_hash_is_valid (valid iff solution_diff >= parent.diff):\n\n- check_parent_and_solution_validity now reports a new\n  InvalidReason::BelowDifficulty when the tip advances onto a\n  higher-difficulty parent, so the candidate loop discards the solution at the\n  rebuild-decision point instead of building a doomed block.\n- produce_block_with_parent gates every production (initial and rebuild)\n  against the parent's diff, the single chokepoint before a block is built.\n\nA sub-difficulty solution is now discarded (Ok(None)) and re-mined rather than\nreaching pre-validation, so the producer never panics for this case.\n\nAdds two regression tests covering the initial-production gate and the\nparent-change rebuild guard.",
          "timestamp": "2026-06-18T17:21:52+01:00",
          "tree_id": "ac69348ac1fc11a8c211b58cad1bc2ffcce3a8a3",
          "url": "https://github.com/Irys-xyz/irys/commit/63d0572b10b239a102ea54c0437ad2c21c59e2b0"
        },
        "date": 1781800721103,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015364,
            "range": "± 0.000528",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.163593,
            "range": "± 0.022856",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.569192,
            "range": "± 0.073804",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.408032,
            "range": "± 0.198169",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.084853,
            "range": "± 0.002174",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 819.390203,
            "range": "± 23.499653",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 991.170262,
            "range": "± 11.851576",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.123914,
            "range": "± 0.005573",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1232.341486,
            "range": "± 68.858425",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1537.502094,
            "range": "± 18.416539",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.033829,
            "range": "± 0.001822",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.468541,
            "range": "± 1.03831",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.748939,
            "range": "± 1.575836",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000114,
            "range": "± 0.000004",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "1e21087df1a70ef7a0d901d16b065b47f165dae2",
          "message": "feat(backport): pin genesis EVM state root (genesis_evm_state) + init-state fail-fast guard (#1455)\n\n* fix(consensus): pin genesis EVM state root via genesis_evm_state config\n\nWhen a network's genesis EVM state is loaded out-of-band with `init-state`\nfrom a state dump (state too large to inline in `alloc`), every node must\nreconstruct the same genesis block. Boot derived the genesis state root\nfrom `reth.alloc`, so it diverged from the dump-derived root that init-state\nwrote to the reth DB, and reth aborted at startup with a genesis-hash\nmismatch (\"genesis hash in storage does not match the specified chainspec\").\n\nReplace `IrysRethConfig.alloc` with `GenesisEvmState { Alloc(map) |\nStateRoot(B256) }` — inline allocation or a pinned state root, mutually\nexclusive by construction. `irys_chain_spec` applies the pinned root (and\nclears alloc), so a node configured with `genesis_evm_state.state_root`\nreconstructs the same genesis the dump produced and boots cleanly. The\ngenesis state root is a boot-time config value; `init-state` itself stays\nconfig-agnostic (it derives the root from the dump).\n\nTests: pinned-root override + StateRoot/Alloc TOML round-trips; the genesis\ndump/restore integration test confirms init-state runs without a config pin.\n\n* fix(reth-bridge): fail fast when init-state runs over an existing reth DB\n\ninit-state will not overwrite an existing reth genesis, so running it over a non-wiped datadir silently leaves the stale genesis in place and the mismatch only surfaces later at boot (\"genesis hash in storage does not match the specified chainspec\"). Detect an already-initialized datadir via <datadir>/db/mdbx.dat up front and bail with instructions to wipe.\n\n* test(consensus): update consensus-hash regression for genesis_evm_state\n\nThe genesis_evm_state rename changed the canonical-JSON serialization of ConsensusConfig (reth.alloc -> reth.genesis_evm_state.alloc), so the keccak256_hash pinned in test_consensus_hash_regression no longer matched and the test failed. Update the expected value to the hash this branch's ConsensusConfig::testing() now produces.\n\n* fix(reth-bridge): re-seal genesis header in init-state; broaden db-init guard\n\ninit_state patched the genesis state root via SealedHeader::set_state_root, which mutates the inner header but leaves the cached block hash stale, so reth would persist a genesis hash that disagrees with the stored header's state root. Re-seal the header from the patched value (clone_header -> seal_slow) so the persisted genesis hash is recomputed.\n\nAlso broaden reth_db_is_initialized to treat any entry under <datadir>/db as initialized, mirroring reth's own is_database_empty check: reth writes db/database.version before the mdbx env, so checking only mdbx.dat missed a partially-initialized datadir.\n\n* test(consensus): update canonical camelCase test for genesis_evm_state\n\n`eab0064fe` moved the genesis allocation from `reth.alloc` to the\n`genesis_evm_state` enum, so the alloc map now serializes under\n`reth.genesisEvmState.alloc`. Update the stale assertion path; the\ncommit that introduced the refactor missed this test.",
          "timestamp": "2026-06-22T22:23:30+01:00",
          "tree_id": "a3b6df09c755f8901cdac6ae5974940232cdf147",
          "url": "https://github.com/Irys-xyz/irys/commit/1e21087df1a70ef7a0d901d16b065b47f165dae2"
        },
        "date": 1782164530567,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011922,
            "range": "± 0.000097",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.120283,
            "range": "± 0.002847",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.271967,
            "range": "± 0.063855",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.380339,
            "range": "± 0.277585",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075041,
            "range": "± 0.00178",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 757.080914,
            "range": "± 11.133803",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1007.745691,
            "range": "± 40.895672",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.117485,
            "range": "± 0.001087",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1184.509526,
            "range": "± 18.456",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1576.676918,
            "range": "± 26.122992",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035304,
            "range": "± 0.00067",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 208.805031,
            "range": "± 0.227763",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 271.468494,
            "range": "± 0.158847",
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
          "id": "317d58caba3182fabd5bb66c498bf9059e4a0dd1",
          "message": "feat(ledgers): anchor slot expiry to last write (Cascade-gated) (#1458)\n\n* feat(ledgers): anchor slot expiry to last write (Cascade-gated)\n\nA data-ledger slot's `last_height` was stamped only at allocation and never\nupdated, so data written late into a slot's life was evicted on the slot's\noriginal clock rather than getting its full retention window. Refresh\n`last_height` each epoch a slot receives new canonical data so a slot's\nexpiry counts from its last write.\n\nGated behind the Cascade hardfork: pre-activation chains keep allocation-time\nexpiry and replay bit-identically. Derived only from epoch-block header\n`total_chunks` deltas, so it is deterministic and replay-safe.\n\nAlso de-duplicates the perm/term slot-access match (now via a private\n`slots_mut` helper, shared by push/remove/touch) and hoists the duplicated\n`is_cascade_active_at` gate into one `cascade_active` bool.\n\n* test(cascade): mid-chain activation transition for Submit last_height\n\nAdds slow_heavy_cascade_midchain_activation_submit_last_height_transition:\nactivates Cascade mid-chain (stop -> set activation_timestamp = now ->\nrestart) with the Submit ledger already live, and asserts last_height is\nfrozen at allocation pre-activation (touch gated off) and tracks the write\nepoch post-activation. Covers the activation-epoch transition that the\ngenesis-activation tests (activation_timestamp = 0) don't exercise.\n\n* refactor(ledgers): clamp touch_filled_slots range to existing slots\n\nReplace the first..=last + get_mut(idx as usize) loop with an early\nreturn when first is past the slice, a checked clamp of last to the last\nvalid index, and direct iteration over the slot slice. last is derived\nfrom cumulative chunk counts and can point past allocated slots; this\navoids probing non-existent indices and the per-iteration cast. Behavior\nis unchanged (covered by the existing touch_filled_slots unit tests).\n\n* test(cascade): cover boundary-spanning tx in last-write expiry test\n\nRework heavy_cascade_slot_expiry_anchored_to_last_write into\nheavy_cascade_spanning_tx_last_write_expiry: a single tx now spans the\nslot 0/1 boundary. Asserts the spanning tx bumps both halves to the write\nepoch, the head slot freezes (not coupled to later tail writes), the head\nexpires exactly epoch_length after its write (full retention, no premature\nhalf-tx loss), and the tail survives until epoch_length after its own last\nwrite. Matches the start-slot ownership model in block_producer/ledger_expiry.rs.\n\n* fix(ledgers): align term-fee settlement with actual slot recycle\n\nThe term-fee model predicted the expiring set from the PARENT epoch snapshot\n(pre-touch), while the real recycle set is computed on the new snapshot\n(post-touch). A slot written in the very epoch it would expire is rescued by\nthe last_height touch (not recycled) but was still settled by the parent-based\nfee calc — so its fees would be distributed then, and again when it actually\nrecycles later (double distribution).\n\nget_expiring_partition_info now excludes slots written in the current epoch's\n[prev_total, new_total) chunk window — the same window touch_filled_slots\nbumps — so the fee-settled set equals what expire_partitions actually recycles.\nThe new total is computed by the producer (parent total + calculate_chunks_added,\nmatching the block header) and read from the header by the validator, so both\nagree.\n\nAdds heavy_cascade_expiry_fee_model_matches_actual_recycle reproducing the\ndivergence (fee set [0,1] vs actual [0]) and asserting alignment.\n\n* test(cascade): rescued slot defers both reward and refund\n\nAdds heavy_cascade_rescued_slot_defers_reward_and_refund: a Submit slot\nholding an unpromoted tx, rescued at its expiry epoch, emits neither a\nTermFeeReward nor a PermFeeRefund that epoch, and the deferred PermFeeRefund\nlands only when the slot actually recycles later. Closes the refund side of\nthe fee/recycle alignment with the same rigor as the distribution side.\n\n* test(cascade): make mid-chain activation timestamp strictly after tip\n\ncascade_at activates on inclusive equality (>=), and every pre-restart block\ntimestamp is <= now() (second-granular), so using now() directly as the\nactivation timestamp could collide with the tip's second and wrongly mark a\nhistorical epoch cascade-active during replay. Offset by +1s so activation is\nstrictly after every pre-restart block.\n\n* fix(ledgers): gate expiry-fee window exclusion on Cascade (replay-identity)\n\nThe write-window exclusion in `get_expiring_partition_info` mirrors the\nCascade-gated `last_height` touch, but was applied unconditionally — while the\nSubmit-ledger expiry-fee path runs regardless of Cascade. Pre-activation (touch\ngated off) a late-filled Submit slot still recycles on its allocation clock,\nyet the unconditional exclusion dropped it from settlement. The original\n(master) binary settled it, so a new binary could not re-validate pre-Cascade\nhistory bit-identically.\n\nThread the producing/validating block's own Cascade status\n(`is_cascade_active_at(block.timestamp)`) through `calculate_expired_ledger_fees`\n-> `collect_expired_partitions` -> `get_expiring_partition_info`; skip the\nexclusion when inactive so settlement reproduces the original master set. Gate\non the block's own timestamp rather than the parent-snapshot helper\n(`is_cascade_active_for_epoch`), which lags by an epoch and would mis-gate at\nthe activation boundary (touch on, exclusion off -> settling a slot that did\nnot recycle).\n\nAlso add a lockstep comment tying the two recycle paths (the post-touch\n`expire_ledger_slots` set and the parent-snapshot window filter) and their\nshared gate, plus a pre-activation Submit regression test — the twin of the\npost-activation fee/recycle assertion — proving the recycled slot is settled\n(not dropped) when Cascade is inactive.\n\n* test(cascade): cover Publish-ledger last-write expiry\n\nThe last_height touch runs on every active ledger — active_ledgers()\nincludes Publish — but the existing perm_ledger_expiry tests never enable\nCascade, so the gated Publish path was uncovered. Add:\n\n- a deterministic unit test proving touch_filled_slots refreshes the perm\n  (Publish) slot path via slots_mut(Publish).\n- a heavy integration test where a previously-allocated headroom perm slot,\n  filled by promotion at a later epoch, has its expiry clock advanced to its\n  write epoch and survives past the allocation-anchored expiry point (this\n  fails on master).\n\n* refactor(ledgers): dedupe block total_chunks lookups; harden slot_index conversion\n\nExtract IrysBlockHeader::ledger_total_chunks(DataLedger) and replace the four\nhand-rolled data_ledgers.iter().find(...).map(...).unwrap_or(0) copies across\nthe producer, validator, and epoch snapshot. Non-behavioral — the consensus\nconfig hash regression test is unchanged.\n\nIn the write-window exclusion, convert slot_index with u64::try_from(..).expect(..)\nrather than an `as` cast: the conversion is lossless on supported targets, but a\npanic is the right failure mode for the impossible case — a silent fallback would\ndiverge the settled set from the recycled set.\n\nRefresh the now-stale block_validation threat-model comment: the expiry-fee calc\nnow reads the candidate header's timestamp and total_chunks (both validated\nindependently; they only select which slots settle).\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-06-25T12:45:03+01:00",
          "tree_id": "43652b8e28ebd1edc54e0f0481d93ead6ddd0cc1",
          "url": "https://github.com/Irys-xyz/irys/commit/317d58caba3182fabd5bb66c498bf9059e4a0dd1"
        },
        "date": 1782388875563,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015347,
            "range": "± 0.000233",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.130498,
            "range": "± 0.012057",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.279406,
            "range": "± 0.063885",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.403995,
            "range": "± 0.220156",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.080277,
            "range": "± 0.000835",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 761.248805,
            "range": "± 31.474374",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 979.898716,
            "range": "± 9.293891",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.119366,
            "range": "± 0.003745",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1213.050606,
            "range": "± 109.000491",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1544.678132,
            "range": "± 6.667367",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034212,
            "range": "± 0.00108",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.818685,
            "range": "± 0.630831",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.194287,
            "range": "± 0.947499",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000111,
            "range": "± 0.000004",
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
          "id": "3318846bab85e44ec015bfc4505d7ab4d9e0f050",
          "message": "fix(vdf): re-anchor VDF on network-partition recovery (#1457)\n\n* fix(vdf): re-anchor VDF on network-partition recovery\n\nDeep reorgs (fork deeper than block_migration_depth) roll back the block\nindex/storage/supply but left VDF state untouched, so a minority fork that\ncrossed a reset boundary kept a poisoned step buffer and rejected the canonical\npost-boundary blocks (the #1447 wedge, beyond the #1449 gate's coverage).\n\nMake the VDF thread a supervisor loop: BlockTreeService signals vdf_reanchor\nafter recover_from_network_partition, run_vdf returns VdfExit::Reanchor, and the\nsupervisor rebuilds VdfState from the truncated canonical index and restarts the\nloop at the new anchor. Keeping the OS thread reuses the fast-forward channel and\ncore pinning.\n\n* fix(vdf): drain stale fast-forward steps on re-anchor\n\nStale FF steps queued from an orphaned fork could be accepted by the\nrestarted run_vdf loop and re-poison the freshly rebuilt canonical\nbuffer. Drain the FF receiver after the buffer swap during re-anchor.\n\n* fix(vdf): complete partition-recovery re-anchor heal\n\nCatch-up local-stepping now applies the reset seed pinned by the block ending\nat/below each crossed boundary (canonical_vdf_info_at_or_below_step), not the\ntip's next_seed which targeted a higher boundary and re-poisoned post-boundary\nsteps. Plus recompute-on-mismatch for VDF step batches, fork-local recall and\nprev-step validation views, efficient-sampling range reset on re-anchor\n(Reanchored broadcast), and requeue-not-fail on VdfStepRewound.\n\nNever mislabel a local/transient fault as peer invalidity: an unbuildable\nfork-local prev-step view requeues via the Cancelled lane\n(VdfPrevStepForkViewUnavailable) and an unbuildable fork-local recall view\nreclassifies to the soft-internal StepsUnavailable lane, rather than\nterminally rejecting the block. On a re-anchor rebuild failure the VDF\nsupervisor resumes from the live buffer's own tip instead of crashing the node.\n\n* test(vdf): harden pre-existing load-flaky tests\n\nThese flakes predate this branch; their tight wall-clock deadlines can be\nexceeded under parallel-CI CPU contention. None are caused by the re-anchor\nheal — split out from the logic commit so the consensus change stands alone.\n\n- hardfork epoch-boundary test: pre-activation window 10s -> 45s so txs submit\n  before activation despite slow node startup\n- validation_service JoinHandle select! watchdog 1s -> 30s (fail-fast guard,\n  not a timing assertion; spuriously fires on a starved current-thread runtime)\n- slow_ prefix on the perm_ledger expiry and sync-resume tests (180s budget)\n- cascade_reorg: poll for async reorg reinjection instead of a single check\n\n* fix(vdf): seed-buffer off-by-one + review follow-ups\n\nbuild_vdf_seed_buffer/create_state broke out of the walk-back's inner for-loop\nwhen the buffer filled but still fetched the parent and, when that parent was\ngenesis, appended one extra seed (capacity+1). The extra front seed inflates\nseeds.len(), dragging get_steps' first_global_step one step too low and\nmis-mapping the buffer's oldest step. Break the while-loop once the buffer is\nfull, before the parent fetch, in both functions; add a regression test.\n\nAlso two doc/comment accuracy fixes from the review pass (no behavior change):\n- design doc section 2: the at-or-below-step query returns Some(tip) when the\n  VDF runs ahead (a no-op), not the unwrap_or fallback; the fallback fires only\n  when the step is below the earliest cached block.\n- prev-step fork-local Err arm: document that it also covers the built-but-\n  uncoverable (near-genesis) view, and why both share the peer-innocent requeue.\n\n* test(vdf): give heavy4_network_partition_recovery the slow_ budget\n\nRename to heavy4_slow_network_partition_recovery so it matches nextest's\nslow_ override (90s x 2 = 180s) instead of the 60s default. A 10x flake pass of\nthe two partition_recovery e2e tests hit one 60s timeout here when it\nco-scheduled with its heavy4 sibling (each reserves 4 threads, oversubscribing\ncores under the opt-level=3 VDF crypto); it passes 8/8 in isolation (~22-30s),\nand the boundary-crossing heal test passed 10/10. Consistent with the existing\nheavy4_slow_ sibling and commit 6ce879875's pre-existing load-flaky hardening.\nThe body is unchanged (byte-identical to master); only the name/budget moves.\n\n* refactor(vdf-validation): unify prev-step check on the fork-local fallback\n\nThe previous-step continuity check carried two redundant recovery paths: a\nre-wait + VdfStepRewound sentinel when the live-buffer step was absent, and a\nfork-local view when it mismatched. Collapse them: read the live buffer as a\nfast path and treat absent and present-but-mismatched identically, both falling\nthrough to the authoritative fork-local cross-check — the same pattern the\nrecall-range seam already uses.\n\nDeletes the VdfStepRewound sentinel (struct + downcast arm) and the\nasync-window re-wait, with no steady-state cost: the live-buffer fast path is\nretained and the fork-local view is still built only in the rare absent/mismatch\nfallback. Net -43 code LOC.\n\nRecall-range validation is deliberately left as-is: its fork-local view is\nsourced only from the block tree, and the recall window (up to reset_frequency\nsteps) can exceed block_tree_depth on testnet config, so the live buffer must\nremain the primary source.\n\n* Fix fork-local VDF view DB fallback\n\n* Unify canonical VDF reads into one snapshot\n\n* Add deterministic canonical step lookup test\n\n* Fix FF boundary reset seed lookup\n\n* chore: fmt\n\n* Fold the reset boundary into VDF anchor hashes (finding #5)\n\nThe step buffer stores raw step outputs (process_reset folds the carried hash\nonly after a step is stored, and fast-forward stores values verbatim), so an\nanchor hash read back via get_last_step_and_seed is unfolded. When the\n(re-)anchor step is itself a reset boundary, run_vdf's first vdf_sha for step+1\nskipped that boundary's fold and diverged from the canonical lineage. Rare\n(only when an anchor lands exactly on a boundary) and non-safety (validation\nrecomputes from each block's own seed), but it mis-steps local mining until it\nheals.\n\nAdd irys_vdf::vdf::reset_applied_anchor_hash and apply it at both anchor sites:\nthe startup anchor (init_vdf_thread, with latest_block's seed) and the re-anchor\narm (with the rebuilt canonical tip's seed). The boundary seed is the anchoring\nblock's own vdf_limiter_info.seed -- when a block's step range contains a reset\nboundary, set_seeds pins that boundary's entropy in `seed` while `next_seed`\ntargets the next boundary above the block.\n\nAlso fixes finding #7: handle_reanchor now fully resets the recall-range\nrotation via Ranges::new instead of reinitialize() + last_step_num = 0. The\nlatter only pruned last_recall_ranges keys <= last_step_num - 20, leaving stale\nentries {N-19..N} that get_recall_range serves by absolute step (no seed check)\nfor the new lineage after a re-anchor. Adds a deterministic unit test (verified\nto fail under the old reinitialize path).\n\nAlso documents this session's reset-seed fixes in the re-anchor design doc: the\nunified single-lock canonical_vdf_snapshot (#2), the fast-forward path sourcing\nthe per-step seed at P-1 (#4), the fork-local views' block-tree->DB ancestor\nfallback (#1), and the anchor-hash fold (#5).\n\n* docs: clarify fork-local prev-step Err arm and re-anchor failure fallback\n\nTwo comment-only clarifications from the final branch review (no behavior\nchange):\n\n- validation_service.rs: the prev-step fork-local `Err` arm only fires on a\n  build failure (an ancestor absent from BOTH the block tree and the DB, post\n  DB-fallback). A successful `build_fork_local_step_view` is sized to include\n  `prev_output_step_number`, so the old comment's \"built but does not cover the\n  step\" subcase is unreachable -- drop it from the comment and the warn message.\n\n- chain.rs: document why the rebuild-FAILURE fallback intentionally skips\n  `reset_applied_anchor_hash`. It resumes from the live tip, which free-ran\n  above the canonical chain, so no confirmed canonical block pins that step's\n  boundary seed; folding a guessed seed would re-poison (the #4 hazard). Resume\n  raw and let store_step's forward-only rule + fork-local recompute realign.\n\n---------\n\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-06-29T11:25:31-07:00",
          "tree_id": "2fd452bfa3afe41da1cc90920c81402d610fc55c",
          "url": "https://github.com/Irys-xyz/irys/commit/3318846bab85e44ec015bfc4505d7ab4d9e0f050"
        },
        "date": 1782758680565,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012629,
            "range": "± 0.000141",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.131919,
            "range": "± 0.003549",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.280677,
            "range": "± 0.078643",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.484835,
            "range": "± 0.303714",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.074886,
            "range": "± 0.001804",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 759.083934,
            "range": "± 8.999217",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 978.394642,
            "range": "± 29.074653",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120143,
            "range": "± 0.001067",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1198.281564,
            "range": "± 5.594586",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1576.007184,
            "range": "± 10.467085",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034496,
            "range": "± 0.001531",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.948199,
            "range": "± 0.496573",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.529512,
            "range": "± 1.86484",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "6023b8475ecdc4dd7b85064329ccea658e7aca39",
          "message": "fix(consensus): prevent Submit-expiry vs promotion double-pay + ingress-proof cache delete (#1456)\n\n* fix: precise per-signer ingress-proof delete in maintenance loop\n\nNC-0042 §10 B1. `IngressProofs` is a DUPSORT table keyed by data_root with\nsigner-address as subkey. The maintenance loop in `prune_ingress_proofs`\npushed only the `data_root` into its `to_delete` vector and then called\n`delete_ingress_proof(data_root)` → `tx.delete::<IngressProofs>(data_root,\nNone)`. Per the upstream MDBX semantics (\"if data parameter is [None],\nany/all value(s) for specified key will be deleted\") this wiped *every*\nsigner's proof for the data_root, including the still-valid ones from other\nsigners.\n\nEffect: a node could never durably accumulate the 3 distinct-signer proofs\nrequired for promotion — any single expiry collapsed the set back to ~1. On\nNC-0042 this pinned node-4 at 2 proofs for 58 minutes and let the offending\ntx drift into its Submit-expiry epoch.\n\nFix: add `delete_ingress_proof_by_signer(tx, data_root, address)` mirroring\nthe precise-delete pattern in `store_ingress_proof_checked`, and a\n`remove_ingress_proof_by_signer` wrapper. `to_delete` now carries\n`(DataRoot, IrysAddress)` so each expired proof is removed by its exact\n(data_root, signer) pair; the `REGENERATE_PROOFS=false` fallbacks delete the\nlocal signer's proof only. The wholesale `delete_ingress_proof` /\n`remove_ingress_proof` helpers had no other callers and are removed.\n\nRegression test `prune_ingress_proofs_preserves_valid_distinct_signer_proofs`:\n3 distinct-signer proofs, expire one, run the pass, assert the other two\nsurvive.\n\n* feat: shared expired-Submit-tx set keyed on partition expiry\n\nNC-0042 §4b infrastructure. The producer filter and validator check both\nneed one question answered: \"has this Submit-ledger tx's storage expired as\nof block H?\" That is a per-*slot* property, NOT per-tx cycle math: a slot\nexpires at `slot.last_height + blocks_per_cycle`, except the last/newest slot\nis never expired, and slots are allocated at arbitrary epoch boundaries (not\ncycle boundaries). A per-tx `submit_expiry_height(included_height)`\napproximation diverges from this and would both reject valid promotions\n(false positive → fork) and miss the double-pay (false negative → the very\nbug). So expiry must be derived from the same expired-partition → block → tx\nwalk the refund pipeline uses.\n\n- `TermLedger::get_all_expired_slot_indexes` / `Ledgers` /\n  `EpochSnapshot::get_all_expired_term_slot_indexes`: slot indexes expired\n  as-of H, *inclusive* of slots that expired at an earlier epoch (the refund\n  pipeline's `get_expired_slot_indexes` returns only newly-expiring slots, to\n  act once; promotion must stay blocked for every later block too). Keyed on\n  `last_height` (set once at allocation), so producer and validator agree.\n- `ledger_expiry::expired_submit_tx_ids(...)`: the shared set. Reuses the\n  refund pipeline's positional `find_block_range` + boundary/middle walk\n  (extracted into `collect_tx_to_miners_from_range`, shared with\n  `calculate_expired_ledger_fees`) so \"is this tx refunded?\" and \"may this tx\n  be promoted?\" cannot diverge.\n\nUnit tests cover the single/last-slot (never expires) and already-expired\n(stays expired) cases. Replaces the misleading cycle-math lock-step test,\nwhich dodged the last-slot rule and gave false confidence.\n\n* fix(consensus): never promote a tx whose Submit storage expired\n\nNC-0042 §4b/§4c. Two independent pipelines disagreed on an expiring tx: the\ntx selector promoted it (enough ingress proofs) while the expiry pipeline\nscheduled a `user_perm_fee_refund` for it. Same-block, the defence-in-depth\nguard in `ShadowTxGenerator::new` turned this into an `Irrecoverable` producer\npanic; across blocks (refund at the expiry epoch, promotion a few blocks\nlater) the chain silently double-paid (refund + permanent storage).\n\nBoth sides now key off the shared `ledger_expiry::expired_submit_tx_ids` set\n(see prior commit), so promotion and refund decisions cannot diverge:\n\n- Producer (§4b): the block producer computes the expired set for the block\n  being produced and passes it into `TxSelectionContext`;\n  `get_publish_txs_and_proofs` drops any candidate in it. An honest producer\n  therefore never emits a both-promoted-and-refunded block.\n- Validator (§4c): `generate_expected_shadow_transactions` recomputes the set\n  and rejects any block promoting a tx in it with `ShadowTransactionInvalid`\n  (peer-attributable). This catches the cross-block case the old same-block\n  guard could not. Shipped unconditionally (no hardfork gate).\n\nThe `ShadowTxGenerator::new` guard is unchanged (defence-in-depth for\nnon-validation construction paths).\n\nHeavy chain-tests (real Submit flow, no DB seeding):\n- `heavy_producer_drops_publish_candidate_whose_submit_storage_expired`:\n  honest producer at the expiry epoch drops the expired candidates and refunds\n  them with no panic; the dropped set equals the refunded set exactly.\n- `heavy_block_promoting_already_expired_submit_tx_gets_rejected`: an evil\n  producer promotes an already-expired tx one block past expiry; both nodes\n  reject with `ShadowTransactionInvalid`.\n\n* perf(consensus): per-candidate Submit-expiry check, walk kept as oracle\n\nThe §4b/§4c expiry check previously computed the full expired-Submit-tx set\n(`expired_submit_tx_ids`) once per block by walking every block spanned by every\nhistorically-expired slot — O(Submit history) per block, on both the producer\nand every validator, growing unbounded over chain lifetime.\n\nReplace it on the production path with a per-candidate predicate. Because the\nSubmit ledger fills sequentially and slots expire oldest-first, expired txs are\na contiguous prefix in ledger-offset space, so a candidate is expired iff its\nSubmit start offset is below the expired range's end\n(`first-unexpired-slot × num_chunks_in_partition`, clamped to data written). The\nper-candidate cost is one `canonical_submit_height` read, with the intra-block\noffset reconstruction needed only for a candidate in the range's last block.\n\n- `is_submit_storage_expired` is the per-candidate check; `expired_submit_tx_ids`\n  is demoted to a **differential test oracle** — `promote_after_submit_expiry`\n  asserts the two agree for every posted tx, both directions, against real chain\n  state (incl. the boundary straddler), so the cheap path provably matches the\n  walk.\n- `expired_submit_range` hoists the block-level work (expired-slot scan + the two\n  block_index boundary lookups) out of the candidate loop; A2 and A3 compute it\n  once and reuse it via `submit_tx_expired`.\n- A2 threads the Submit-inclusion height it already resolved (DB-fallback path)\n  into the expiry check, avoiding a duplicate `canonical_submit_height` read.\n- `find_block_range` / `calculate_expired_ledger_fees` now take the parent header\n  so the walk oracle and the per-candidate check share the same `max_offset`\n  bound (unit-tested: `find_block_range_bounds_by_parent_not_index_tip`).\n\nNo behavior change — purely a cost reduction validated by the oracle-equivalence\ntest plus the existing T2/T3 heavy chain-tests.\n\n* fix(consensus): per-signer ingress-proof delete on cache maintenance\n\nThe IngressProofs DUPSORT delete removed ALL signers' rows for a data_root\n(delete(key, None)), so the maintenance loop could never durably hold the\ndistinct-signer proofs needed to promote a tx. Delete only the specific\n(data_root, signer) row via an O(1) seek, content-matched so a proof\nrefreshed between the read scan and the write isn't dropped (NC-0042 B1).\n\n* fix(consensus): branch-deterministic, exact Submit-ledger expiry (NC-0042)\n\nA tx promoted into Publish while its Submit slot has expired is both\nperm-fee-refunded and permanently stored (cross-block double-pay), or\npanics the producer when promotion and expiry land in the same epoch\nblock. Make the expiry verdict a pure function of the block's own parent\nancestry, shared by the promotion check (§4b producer filter / §4c\nvalidator rule) and the refund/fee walk so they cannot diverge:\n\n- resolve a tx's Submit inclusion and the expired chunk range from the\n  parent header plus a by-hash parent-chain walk (block tree for the\n  un-migrated tail, finalized block index below it, MigratedBlockHashes-\n  gated), never this node's migration-lagged index tip — closing a\n  cross-node epoch-block fork vector and a stranded-refund accounting gap.\n- attribute each expired tx exactly once to the partition its start\n  offset falls in: an exact two-sided boundary trim (no same-block\n  over-inclusion) and a corrected walk advance that no longer skips the\n  chunk straddling two blocks.\n\n* test(consensus): NC-0042 promotion-after-expiry chain-tests\n\nProducer must drop publish candidates whose Submit storage has expired\n(§4b) and the validator must reject blocks that promote them (§4c), both\nsame-block and cross-block. Adds a shared two-node setup, tightens the\nrejection assertions (NC-0042 message + evm-block-absent), pins the\nproducer to the validated block's parent, and asserts the per-candidate\npredicate agrees with the walk oracle.\n\n* test(consensus): correct term-expiry refund expectations + determinism\n\nExact refund attribution corrects two expectations that had encoded the\nold over-inclusion / index-truncation behavior (multiple_partitions 6->5,\nmultiple_txs 5->7), and makes perm_refund deterministic by mining the\nunpromoted tx alone so it owns the expiring slot (block tx order is set by\ncompare_tx, not post order; the prior over-inclusive walk had masked it).\n\n* ci: document conventional-pr workflow permissions\n\n* fix(consensus): branch-correct refund boundary base + per-slot miner attribution (NC-0042)\n\nTwo residuals in the epoch refund/fee walk that the §4b/§4c promotion fix\ndid not cover.\n\nR1 (boundary base): process_boundary_block re-derived the boundary block's\nstart offset from the migrated-only block index (get_previous_max_offset),\nwhich errored \"previous block must exist\" when the boundary block sat in the\nun-migrated tail, and could read a wrong-branch predecessor total on a side\nfork — re-introducing the R1 index-lag divergence on epoch blocks (node\nfault / fork / sync halt). resolve_ledger_offset_to_block now also returns\nthe branch-correct base; BoundaryBlock carries it and process_boundary_block\nuses it. get_previous_max_offset is removed.\n\nR4 (miner over-merge): find_block_range merged miners per block, so when two\nexpired slots owned by different miners shared a block, every tx in that\nblock was attributed to both miner sets — violating the module's start-offset\nownership rule and mis-distributing term fees and the reward rolling hash.\nMiners are now kept per slot (un-merged); each tx — in boundary and middle\nblocks alike — is attributed to the miners of the slot containing its start\noffset. The boundary trim still uses the union range.\n\nRegression tests: find_block_range_resolves_unmigrated_tail asserts the\nbranch-correct base (a tree-only value past the index tip) and runs\ncollect_tx_to_miners_from_range on an un-migrated boundary (errored pre-fix);\ncollect_attributes_each_tx_to_its_start_offset_slot_miners covers a\ntwo-slot/two-miner straddle block.\n\n* test(db): cap test database MDBX geometry; pass DatabaseArguments to DB opens\n\nMDBX maps the geometry's upper bound as virtual address space at open (its\nstable-pointer zero-copy design pre-reserves the whole range so the mapping\nbase never moves). The consensus DB inherits reth's 8 TiB default and storage\nsubmodules used 2 TiB — harmless as sparse reservations on a real disk, but\nmany concurrent test nodes (each mapping several such envs) exhaust the\nprocess's ~128 TiB user address space and ENOMEM at DB open under wide\n`cargo test` parallelism. The data is tiny; only the reservation is huge.\n\nAdd DatabaseConfig.geometry_max_size (None in production; Some(4 GiB) =\nTEST_DB_GEOMETRY_MAX_SIZE in NodeConfig::testing()) and cap irys_testing()\nargs. Production behaviour is unchanged; tests cap the reservation ~1000x.\nThe cap is a hard ceiling (exceeding it is MDBX_MAP_FULL), and 4 GiB is well\nabove any test DB.\n\nAlso stop threading DB tuning as piecemeal parameters: DB opens now take a\nfully-built DatabaseArguments. create_or_open_submodule_db and\nopen_or_create_irys_consensus_data_db_with_args take args, built by the new\nconsensus_db_args / submodule_db_args(&DatabaseConfig) helpers; a thin\n(path, sync_mode) convenience remains for existing test callers.\n\n* fix(consensus): fail loud when refund walk maps a tx to a non-expired slot (NC-0042)\n\n`filter_transactions_by_chunk_range` previously `warn!`-logged and silently\nskipped a tx whose Submit start offset mapped to a non-expired slot. That arm\nis unreachable (expired slots tile the range contiguously), but if it ever\nfired it would mean the refund walk diverged from the per-candidate promotion\nfilter (`submit_tx_expired`) — a silent under-refund that breaks Pipeline A ≡ B.\n\nMake it fail loud: return `Result` from `filter_transactions_by_chunk_range`\nand `bail!` on the `None` arm. The verdict is a pure function of canonical\nstate, so all nodes fail identically — no fork risk. Threaded `?` through the\ntwo callers (`process_boundary_block`, `process_middle_blocks`), both already\n`Result`-returning.\n\n* test(consensus): assert tx1 is the sole slot-0 Submit tx in perm-refund test\n\nThe assertion message claims \"tx1 should be alone in slot 0 at block 1\" but\nonly checked `.contains(&tx1.id)`. Tighten to `s.len() == 1 && s[0] == tx1.id`\nso the test actually enforces slot-0 exclusivity (only tx1 is posted before\nblock 1).\n\n* refactor(consensus): simplify ledger_expiry (no behavior change)\n\nBehavior-preserving cleanups from a simplification pass over the NC-0042 code:\n- gate test-only `is_submit_storage_expired` behind cfg(test / test-utils) and\n  trim its redundant doc; production hoists `expired_submit_range` +\n  `submit_tx_expired` per candidate and never called the wrapper\n- fix the stale block_producer comment that named `is_submit_storage_expired`\n- extract `is_canonical_at` for the C1 `MigratedBlockHashes` read that was\n  duplicated between the walk closure and `assert_canonical_via_mbh`\n- inline the single-use `submit_total_of_header` alias\n- drop the redundant `block_hash` param from `process_boundary_block`\n  (it always equalled `boundary.block_hash`)\n- collapse the two-step `all_tx_ids` build into a single `.collect()`\n\nVerified: 21 ledger_expiry unit tests + 3 heavy refund-pipeline chain-tests\npass; clippy + fmt clean.\n\n* fix(consensus): fail loud on non-prefix expired Submit slot set (NC-0042)\n\n`expired_submit_range` collapses the expired Submit slots to a single\n`[0, range_end)` prefix (range_end = (max_expired_slot+1)*P clamped to the\nparent header total). That is correct only if the expired set is a contiguous\nprefix `{0..=max}`. In normal operation it always is — chunk offsets are\ncumulative and slots fill sequentially, so `last_height` is non-decreasing in\nslot index and empty pre-allocated slots sit at the top where the\nnever-expire-last-slot rule trims them.\n\nBut the invariant was unguarded. A hole (an empty, non-last slot aged out by its\nallocation height beneath a still-live lower slot) would make the prefix\nover-approximate: it would deterministically reject valid promotions AND diverge\nfrom the slot-exact refund walk (`collect_expired_partitions` only settles slots\nthat actually hold partitions). Add a contiguity guard that `bail!`s on a hole.\nThe expired set is a pure function of canonical state, so all nodes fail\nidentically — no fork — mirroring the existing `filter_transactions_by_chunk_range`\nfail-loud guard rather than silently diverging.\n\nTests:\n- expired_submit_range_bails_on_non_prefix_expired_set: builds the real hole\n  topology (4 Submit slots, touch only slot 1 -> last_height [1,100,1,1]) and\n  asserts the guard fires.\n- filter_transactions_by_chunk_range_bails_on_non_expired_slot: direct coverage\n  of the c300d7ec2 fail-loud arm (previously untested).\n- get_all_expired_slot_indexes_can_be_non_prefix (irys-database): documents that\n  the holey input is constructible, kept in lockstep with the guard.\n\n* test(consensus): per-miner refund amount attribution across distinct slots (NC-0042 R4)\n\nThe R4 per-slot miner attribution (each tx credited to the miner of the slot\ncontaining its start offset, not the per-block miner union) is the highest-\nconsequence balance-affecting path. Its mapping is already unit-tested\n(collect_attributes_each_tx_to_its_start_offset_slot_miners), but no test\nconfirmed the resulting reward AMOUNTS per miner once distinct miners own\ndifferent slots.\n\nAdd aggregate_attributes_each_slots_fee_to_its_own_miner: two distinct miners\neach own two txs (mirroring the slot-0/slot-1 straddle split), run through\naggregate_balance_deltas, asserting each miner receives exactly the treasury\nslice of its own slot's txs with no cross-crediting and no third creditor.\n\nA full multi-node produced-block variant (decoding on-chain reward shadow-txs)\nis left as a follow-up: the term_ledger_expiry harness is single-miner and\ncannot place two miners' expired slots in one boundary block without\npartition-assignment control it does not expose.\n\n* test(db): cover per-signer + TOCTOU ingress-proof deletes\n\nThe per-signer fix (delete_ingress_proof_by_signer) and its content-aware CAS\nvariant (delete_ingress_proof_if_unchanged, commit b0b0b96c4) were only covered\ntransitively via the cache prune test. Add direct DB-layer unit tests:\n\n- delete_ingress_proof_by_signer_deletes_only_that_signer: two distinct-signer\n  rows on one data_root; deleting signer A removes only A's row and leaves B's,\n  returns true; re-deleting A and deleting an absent signer return false.\n- delete_ingress_proof_if_unchanged_preserves_refreshed_proof: the TOCTOU\n  Ok(false) branch (the reason the fn exists). Absent key -> false; after the\n  stored proof is overwritten with a different one, the stale expected value ->\n  false and the fresh proof survives; the current value -> true and the row is\n  removed.\n\n* docs(consensus): correct LedgerSlot.last_height \"never mutated\" comment\n\nThe doc claimed last_height is \"set once at allocation and never mutated\", but\nthe Cascade-gated touch_filled_slots refreshes it to the last write height (that\nis the whole point of the fill-anchor fix). The expiry-purity argument leans on\nthis comment, so correct it: last_height is set at allocation then refreshed by\nthe touch, and either way is a deterministic function of canonical state.\n\n* fix(consensus): guard chunk_size==0 in filter_transactions_by_chunk_range\n\n`data_size.div_ceil(chunk_size)` panics on a zero divisor. chunk_size is a\ngenesis-fixed consensus constant (never 0 in a real config), but guard it loudly\nand deterministically rather than risk a producer/validator panic — matching the\nexisting chunk_size==0 guard in submit_tx_start_offset.\n\n* perf(cache): release scan read-tx before the per-proof delete loop\n\nprune_ingress_proofs held the scan's read transaction (and its walker/cursor)\nopen across the delete loop, which opens up to MAX_PROOF_CHECKS_PER_RUN short\nwrite txns. A long-lived reader pins the MVCC snapshot and holds back MDBX\nfree-page reclamation. Drop the walker/cursor/tx after the scan; nothing past it\nreads from that tx (the delete/reanchor/regen loops open their own txns).\n\n* fix(cache): content-check ingress-proof delete in regen-disabled branches\n\nThe REGENERATE_PROOFS=false branches of prune_ingress_proofs deleted the local\nproof via remove_ingress_proof_by_signer (a plain per-signer delete), unlike the\nto_delete loop which uses the content-aware delete_ingress_proof_if_unchanged.\nThat left a TOCTOU gap if the const is ever flipped: a proof refreshed between\nthe scan and this delete would be wiped. Route both branches through the same\ncontent-checked delete (reconstructing the scanned CompactCachedIngressProof\nfrom local_addr + the proof), so a refreshed proof is preserved.\n\nThis orphaned the pub(crate) remove_ingress_proof_by_signer wrapper (its only\ncallers) and its IrysAddress/delete_ingress_proof_by_signer imports; remove them.\n(The branches are dead today — REGENERATE_PROOFS is a hardcoded true const — so\nthis is a consistency/hardening fix, not a behavior change in production.)\n\n* fix(cli): honor geometry_max_size when opening the consensus db\n\ncli_init_irys_db opened the consensus DB via\nopen_or_create_irys_consensus_data_db (sync mode only), silently ignoring\nconfig.database.geometry_max_size, while the node path in chain.rs honors it via\nconsensus_db_args. Use open_or_create_irys_consensus_data_db_with_args with\nconsensus_db_args so the CLI applies the same DB tuning (sync mode + geometry).\n\n* Gate ledger expiry fixes on Cascade\n\nTrack whether ledger slots have ever held canonical data and, after Cascade activation, exclude unwritten preallocated slots from expiry.\\n\\nThread the Cascade gate through epoch processing and the promotion/refund expiry checks so pre-Cascade replay keeps the old allocation-based behavior while post-Cascade blocks use the corrected written-slot semantics.\\n\\nAdd regression coverage for the canonical non-prefix topology, pre/post-Cascade unit expectations, and focused Cascade chain tests.\n\n* fix(consensus): keep expired-tx & ingress-anchor resolution branch-correct across deep reorgs\n\nDeep reorgs can rewrite already-migrated blocks up to block_tree_depth deep,\nso \"migrated\" no longer implies \"finalized / branch-invariant\". Two resolution\npaths inherited the old assumption and could fork between honest nodes for\nheights in the (tx_anchor_expiry_depth, block_tree_depth] band:\n\n- block_validation: a promotion's prior-Submit walk only covered\n  tx_anchor_expiry_depth, then fell back to a by-height index/MBH lookup (the\n  node's local canonical, not the candidate's own branch). The Submit->Publish\n  gap is bounded by ingress_proof_anchor_expiry_depth, not the tx anchor, so\n  legitimate promotions hit that fallback. Walk depth is now\n  max(tx_anchor_expiry_depth, block_tree_depth) via prior_inclusion_walk_depth,\n  so the by-hash walk covers the reorg window and the by-height fallback is\n  only reached for finalized inclusions. Corrected the flawed\n  \"20-is-sufficient\" docblock; added a regression test + a derivation unit test.\n\n- block_discovery: the ingress-anchor index-fill's one boundary height (above\n  the handoff) was taken from the reorg-mutable index; it is now supplied\n  by-hash from the candidate's own ancestry.\n\nConfig::validate now enforces (production only) the invariants the resolution\nrelies on:\n- submit_ledger_epoch_length * num_blocks_in_epoch > block_tree_depth (+ Cascade\n  thirty_day/one_year): an expired tx's inclusion is always below the reorg floor.\n- tx_anchor_expiry_depth <= block_tree_depth: tx anchors always resolve inside the tree.\n\nAlso corrected the now-stale \"migrated = immutable\" fork-awareness docstring above\ncanonical_metadata_height.\n\n* fix(consensus): harden ledger-expiry edge cases, restore bail coverage, simplify\n\n- Fail loud instead of silently defaulting to 0 when a migrated predecessor index\n  item is missing (migrated_predecessor_total + the resolve_submit_inclusion fast-path\n  base): a predecessor below the reorg floor must be indexed, and the sibling lookup\n  already bails.\n- Bail when num_chunks_in_partition == 0 in filter_transactions_by_chunk_range,\n  matching the existing chunk_size == 0 guard (previously skipped attribution silently).\n- Restore the contiguity-bail test (expired_submit_range_bails_on_non_prefix_expired_set)\n  and add a chunk_size == 0 bail test.\n- Simplify: delete the duplicate parent_ledger_total_chunks (use\n  IrysBlockHeader::ledger_total_chunks at all call sites); remove the same-block\n  empty-map no-op; dedup the C1 non-canonical-ancestor error into\n  c1_non_canonical_ancestor_err (shared by the walk and assert_canonical_via_mbh).\n- Correct the now-stale \"migrated = finalized/branch-invariant\" comments to note\n  branch-invariance holds only below the reorg floor.\n\n* fix(cache): scope per-signer ingress-proof delete to tests; cover at-capacity delete\n\n- delete_ingress_proof_by_signer is now #[cfg(test)] pub(crate): it performs no\n  content/TOCTOU check, so gating it on cfg(test) makes a production misuse impossible\n  at compile time (production paths needing TOCTOU-safety use\n  delete_ingress_proof_if_unchanged).\n- Add prune_ingress_proofs_at_capacity_deletes_local_proof covering the at-capacity\n  local-proof content-checked delete branch (the core of the per-signer delete fix),\n  asserting a distinct signer's proof survives.\n\n* test(consensus): two-miner refund attribution + Cascade-active promotion-filter coverage (NC-0042)\n\n- perm_refund_multi_miner: produced-block test that two distinct miners' expired\n  Submit slots settling in one boundary block each receive exactly their own slot's\n  TermFeeReward slice (no cross-crediting). Slot->miner assignment is RNG-seeded, so\n  the test observes the actual owners at runtime and fails loud if assignment collapses\n  to a single miner. Closes the only-in-memory-tested R4 amount-attribution gap.\n- cascade_submit_expiry_promotion: Cascade-active end-to-end §4b/§4c test — an\n  already-expired Submit tx is not promoted (§4b) and a block promoting it is rejected\n  by both nodes (§4c). Asserts the directional invariant (refund subset of expired, no\n  double-pay) rather than exact refunded==expired, which only holds pre-Cascade.\n- Pin cascade = None in submit_expiry_two_node_setup so the exact refunded==expired\n  assertion in publish_after_submit_expiry_filtered can't silently break on a config\n  default change (CodeRabbit CR3).\n\n* docs(types): note ledger_chunk_offset_ii! inclusive-inclusive vs exclusive consumption\n\nThe macro builds an inclusive-inclusive (ii) interval, but the ledger-expiry consumers\ntreat .end() as exclusive (loops use < end, trims use >= end). Self-consistent today,\nbut a future caller using nodit's .contains() on this interval would double-count the\nboundary chunk; note it and point to ledger_chunk_offset_ie! for half-open semantics.\n\n* fix(consensus): reject slot-lifetime overflow in config validation (NC-0042)\n\nThe Submit/Cascade slot-lifetime guard used saturating_mul, so an\noverflowing epoch_length * num_blocks_in_epoch clamped to u64::MAX and\nsilently passed the `> block_tree_depth` invariant — yet the runtime\nexpiry math (TermLedger::get_expired_slot_indexes' checked_mul.expect)\nwould then panic on the same overflow. Use checked_mul + bail, matching\nthe adjacent publish_ledger_epoch_length overflow check.\n\nExtend test_submit_slot_lifetime_vs_block_tree_depth_validation to cover\nthe Cascade thirty-day/one-year arms and the overflow case.\n\n* fix(consensus): reject term-ledger perm_fee Some(0) in the tx selector (NC-0042)\n\nThe producer's term-ledger filter skipped only `perm_fee > 0`, so a\n`perm_fee = Some(0)` tx was selectable — but the validator\n(block_validation term-ledger prevalidation) and mempool ingress\n(data_txs.rs) both reject any `perm_fee.is_some()`, making the producer\nbuild a block every peer rejects (producer-more-permissive-than-validator).\nMatch `is_some()` so the producer can never select such a tx.\n\nAdd a `perm_fee = Some(0)` rejection regression at the mempool ingress\nboundary so the rule is pinned where it is canonically enforced.\n\n* test(consensus): tighten NC-0042 Submit-expiry coverage (slow path, reorg, exact refunds)\n\n- slow_path_submit_expiry: exercise resolve_submit_inclusion's un-migrated\n  by-hash slow path under a live node (block_migration_depth = 12) and\n  assert the slow path is actually taken (canonical_submit_height == None),\n  so the test cannot silently pass via the fast path. The §4b/§4c chain\n  tests all use block_migration_depth = 1 (fast path only); production uses\n  larger depths where the slow path is reached.\n- reorg_submit_expiry: a reorg moves the canonical tip across the Submit\n  expiry epoch; the node adopts the winning branch and its expiry block\n  carries the correct perm-fee refunds (branch-correct settlement survives\n  the reorg).\n- cascade_submit_expiry_promotion: assert on-chain refunds EXACTLY equal\n  Pipeline B's refund set (catches an under-refund) instead of only the\n  directional `refund ⊆ expired` check.\n- Parameterize submit_expiry_two_node_setup with block_migration_depth;\n  existing §4b/§4c callers pass 1 (unchanged behavior).\n\n* fix(consensus): resolve term-ledger expiry offsets via tolerant block-index lookup\n\nThe epoch-block expiry-fee walk resolved expired chunk offsets with\n`BlockIndex::get_block_index_item`, whose binary search errors (\"Ledger\n{L} not found in block at height N\") on any probed block lacking an entry\nfor the ledger. On a mid-chain Cascade activation the pre-activation\nblocks carry only Publish+Submit, so once a OneYear/ThirtyDay slot ages\ninto the migrated index and expires, the search probes a pre-activation\nblock and errors out of BOTH the producer and the validator epoch-block\nexpiry path -> a deterministic chain stall.\n\nRoute resolution through the tolerant `get_block_bounds` (a probed block\nwith no entry for the ledger counts as total_chunks=0 and the search\nmoves on), via a new `resolve_offset_via_index` helper; drop the now\norphaned `migrated_predecessor_total`. The result block is still required\nto carry the ledger, so genuine index corruption is still surfaced.\nBehavior is unchanged for Submit/Publish and Cascade-from-genesis.\n\nCoverage (both verified to fail without the fix and pass with it):\n- unit: find_block_range_resolves_term_ledger_offset_across_preactivation_index\n- chain-test: slow_heavy_cascade_midchain_thirty_day_expiry_resolves_through_index\n  (the existing term-expiry tests all activate Cascade at genesis, so no\n  block ever lacked the ledger entry — which is why this slipped through)\n\n* fix(db,cli): honor consensus-db geometry on convenience + read-only open paths\n\nThe sync-mode-only `open_or_create_irys_consensus_data_db` built args via\n`irys_default` (no geometry cap), and the CLI's read-only consensus-db open used\nraw default `DatabaseArguments` — both bypassing `geometry_max_size`, so test /\nconstrained-geometry configs reserved reth's large default map. Cap the\nconvenience opener to `TEST_DB_GEOMETRY_MAX_SIZE` (all its callers are tests; the\nnode and the CLI RW path open via `consensus_db_args`) and route the RO CLI path\nthrough `consensus_db_args` with the RO flags layered on top.\n\n* fix(consensus): harden NC-0042 ledger-expiry guards and align the differential oracle\n\n- Config::validate now rejects `num_chunks_in_partition == 0` and\n  `num_chunks_in_recall_range == 0`: zero sizing collapses `expired_submit_range`'s\n  range to 0, silently disabling the §4b/§4c promotion filter while the refund walk\n  bails (an asymmetric silent bypass).\n- `expired_submit_range` bails loud on a zero partition size instead of returning\n  `Ok(None)`.\n- Overflow hardening: `SlotIndex::compute_chunk_range` (saturating_mul),\n  `filter_transactions_by_chunk_range` tx_end (saturating_add) and\n  `SubmitLedgerMetadata::new` (checked_mul) — no release-build wrap.\n- Differential oracle: `expired_submit_tx_ids` / `is_submit_storage_expired` now\n  take the block-under-test's own `cascade_active` (previously derived from the\n  parent epoch snapshot, wrong at the activation boundary); the four callers pass\n  it and the unit fixture asserts the walk oracle equals the per-candidate verdicts.\n- Tighten the affected expiry tests carried in those caller files: assert no\n  duplicate on-chain refunds (no over-refund) in the Cascade promotion test, and\n  fail fast on a missing payload in the slow-path test.\n- Document the perm-ledger `has_been_written` invariant in `get_perm_expiring_slots`.\n\n* test(consensus): strengthen NC-0042 expiry assertions and add multi-signer prune coverage\n\n- reorg_submit_expiry: assert the winning branch's on-chain refunds EXACTLY equal\n  the Pipeline-B recompute (was a non-empty + subset check that tolerated a partial\n  or missing refund).\n- perm_refund: assert tx2 actually lands in the second Submit block.\n- perm_refund_multi_miner: pin `num_partitions_per_term_ledger_slot = 1` so each\n  expired slot maps to exactly one miner.\n- cache_service: new `prune_ingress_proofs_deletes_two_expired_signers_preserves_one`\n  covering multi-signer per-key deletion in a single prune pass.\n- cascade_term_expiry: document the tolerated wall-clock activation dependency.\n\n* chore: review-driven comment and lint polish\n\n- shadow_tx_generator: update the NC-0042 comment to reference the per-candidate\n  `expired_submit_range` + `submit_tx_expired` path (the `expired_submit_tx_ids`\n  walk it named is now a test-only oracle).\n- block_validation: `prior_inclusion_walk_depth` uses `u64::from(..)` instead of a\n  lossy `as` cast.\n- conventional-pr workflow: correct the `contents: read` permission comment (it\n  grants read-only repo access; it does not check out the repo).\n\n* fix(consensus): branch-correct NC-0042 expiry refund-suppression\n\nThe perm-fee refund at Submit-ledger expiry was suppressed based on a tx\nheader's `promoted_height`, read from the node-local mempool (preferred by\n`get_data_tx_in_parallel`). That value is set at block confirmation within\nthe reorg window and is therefore branch-variant: two forks contesting a\nslot-expiry boundary could compute different refund sets and fork the chain\n(the mismatch classifies as a node fault, so a node would panic/restart on\nan otherwise-honest block).\n\nResolve promotion from the block's own parent ancestry instead, mirroring\nthe Submit-inclusion resolver: `resolve_promoted_on_branch` does a by-hash\nwalk over the reorg window checking Publish-ledger membership, with a\nfinalized `canonical_promoted_height` fallback capped below the reorg floor\n(where MigratedBlockHashes is branch-invariant). `aggregate_balance_deltas`\nnow takes the resolved set rather than reading the header.\n\nAlso consolidates the producer/validator expiry-settlement orchestration\ninto one shared `calculate_all_expired_ledger_fees` so parity is mechanical;\nextracts `block_header_from_tree_then_db` and `block_includes_ledger_tx`;\ndrops a redundant sort; and makes the chunk-total arithmetic saturating for\nparity with the surrounding code. Documents the deliberate clean-cutover\ndecision (the expired-promotion rejection and refund algorithm are not\nCascade-gated, so pre-Cascade replay applies them).\n\n* fix(config): reject degenerate zero-size configs in validation\n\n`chunk_size == 0` was not rejected, leaving an asymmetric silent bypass:\n`submit_tx_start_offset` treats it as \"not expired\" while the refund walk\nbails loud. Add a `chunk_size > 0` guard alongside the existing\nnum_chunks guards.\n\nAlso guard `SubmitLedgerMetadata::new` against a zero `blocks_per_cycle`\n(either factor zero) — without it `block_height % 0` panics opaquely; the\n`checked_mul` only caught overflow. And document why `publish_ledger_epoch_length`\nis intentionally exempt from the slot-lifetime > block_tree_depth guard\n(Publish expiry never resolves inclusions via the migrated-index walk).\n\n* chore(storage): gate test-only consensus-DB opener behind a feature\n\n`open_or_create_irys_consensus_data_db` applies the small\nTEST_DB_GEOMETRY_MAX_SIZE cap and is only meant for tests; a production\ncaller would silently inherit that cap. Gate it behind `cfg(test)` and a new\n`test-utils` feature so production misuse is a compile error, and enable the\nfeature in the dev-dependencies of the crates whose tests use it (actors,\np2p). Production opens the consensus DB via the uncapped `_with_args` path.\n\n* test(consensus): strengthen NC-0042 expiry/refund/promotion coverage\n\n- New regression for the branch-correct refund fix: a stale node-local\n  promoted_height must not suppress the refund (asserts on the resolver and\n  the settlement layer; fails on the pre-fix code).\n- New mixed-mode test: a Submit tx included pre-Cascade and expiring after a\n  mid-chain activation is refunded and the chain progresses (pins the\n  clean-cutover behavior across the activation boundary).\n- New validator-level test that a term-ledger block carrying perm_fee=Some(0)\n  is rejected (previously only API ingress was covered).\n- Refund parity assertions now detect over-refund (multiset) and compare\n  amounts, not just the id set; two-node setup waits for peer sync; vacuous\n  empty-payload / dropped-result paths fail loud.\n- data_ledger last-slot test now marks the slot written so it exercises the\n  last-slot rule instead of the unwritten-slot shortcut.\n\n* chore: drop comments of record from this branch's changes\n\nStrip historical/incident references a future reader can't resolve — devnet\nincident IDs, issue/PR numbers, the assigned-miners determinism branch name,\nand \"pre-fix\"/\"every validator accepted it\" war-stories — while keeping the\ndesign rationale each comment carries (reframed where a comment was framed as\nhistory). The NC-0042 / §-section spec anchors are kept as design labels.\n\nComment-only, plus two diagnostic-message strings in config validation that\ncarried a dangling issue ref. A handoff for applying the same pass to the\nrest of the repo (code already in origin/master) lives in .local/ (local-only).\n\n* fix(config): reject zero num_blocks_in_epoch and submit_ledger_epoch_length\n\nThey feed the slot-lifetime/expiry multiply on every path, including test mode where the slot-lifetime vs block_tree_depth check is skipped. A zero in either collapses every slot lifetime to zero blocks and can panic the runtime expiry math. Guard unconditionally, matching the existing publish_ledger_epoch_length check.\n\n* refactor(consensus): checked conversion for the expired-Submit slot-index cast\n\nReplace the unchecked as-u64 cast and bare +1 with u64::try_from and saturating_add so the slot-index conversion documents its invariant and the increment cannot wrap. Behavior-preserving on all supported targets.\n\n* docs(consensus): note the promotion-block set is a superset of the refund set\n\nThe NC-0042 promotion check keys off the inclusive all-expired Submit set, a strict superset of the set the refund pipeline settles (which also drops slots written this epoch). Refunded implies blocked, so no double-pay; the converse is conservative and self-healing. The prior comment overstated this as exact equality. Also correct the epoch-snapshot note: pre-Cascade, allocation-aged unwritten slots can still expire.\n\n* test(consensus): anchor mid-chain Cascade activation to the chain tip\n\nDerive the mid-chain Cascade activation timestamp from the pre-restart tip block's timestamp + 1 instead of wall-clock now() + 1. Activation stays strictly after every pre-restart block without depending on the realtime clock, so a backward CLOCK_REALTIME lurch can no longer mark a historical epoch cascade-active.\n\n* test(consensus): fail fast on a missing expiry-test fixture payload\n\nReplace unwrap_or_default() on the posted tx payload with expect() when building the ingress proof, so a fixture regression that drops the payload fails loudly instead of silently proving the test with an empty proof.\n\n* fix(block-index): reject cumulative chunk-total overflow instead of clamping\n\nA saturating add silently clamped the cumulative per-ledger total_chunks,\nwhich would corrupt the persisted ledger boundary. Use a checked add that\nfails the block-index write on overflow so callers reject the invalid total.\n\n* fix(config): reject reset-window confirmation-lag overflow instead of clamping\n\nThe 2x-confirmation-lag requirement used a saturating multiply, so an\noverflowing requirement was floored and could let an under-sized reset\nwindow pass validation. Compute it with a checked multiply and reject the\nconfig when it overflows; the ensure! guard and message are unchanged.\n\n* fix(cache): re-check data-root presence before deleting orphan ingress proofs\n\nOrphan-proof deletions were decided from scan-time state, so a proof could\nbe deleted even if its data root was re-cached before the delete tx ran.\nTag orphan deletions and re-check CachedDataRoots presence inside the write\ntx, keeping the proof when the root reappeared. Capacity/remote deletions\nare unaffected.\n\n* refactor(consensus): use explicit u128 conversion in PoA owning-tx scan\n\nWiden the cumulative byte cursor with u128::from rather than an `as` cast,\nmatching the repo's explicit-conversion style.\n\n* docs(shadow-tx): describe the expired-Submit rejection set as a superset of the refund set\n\nThe constructor note claimed the validator keys off the same tx set that\nschedules refunds. Under Cascade the refund pipeline additionally drops\nslots written this epoch, so the rejection set is a strict superset;\ncorrect the comment to state the containment that makes the no-double-pay\nproperty load-bearing.",
          "timestamp": "2026-06-30T22:37:10+01:00",
          "tree_id": "e2a56a2c43cd95aff6f9519cf7130bee3a9824f2",
          "url": "https://github.com/Irys-xyz/irys/commit/6023b8475ecdc4dd7b85064329ccea658e7aca39"
        },
        "date": 1782856618246,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012855,
            "range": "± 0.001075",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.153487,
            "range": "± 0.004031",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.557538,
            "range": "± 0.040905",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.47231,
            "range": "± 0.401538",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.076947,
            "range": "± 0.002404",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 775.606953,
            "range": "± 15.900286",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 991.156251,
            "range": "± 56.870664",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.118698,
            "range": "± 0.001928",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1198.76416,
            "range": "± 15.9156",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1560.12155,
            "range": "± 9.821201",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035349,
            "range": "± 0.004324",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 212.087892,
            "range": "± 0.899469",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 276.198295,
            "range": "± 2.695409",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000112,
            "range": "± 0.000003",
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
          "id": "8a49f5fa49cd0a27c79da79fef2e5c7fa877b5bf",
          "message": "feat: add handshake-peer-info to the handshake process",
          "timestamp": "2026-07-01T16:47:34Z",
          "tree_id": "bbd15d12e1b373acb10ab020846e362f83fcefc6",
          "url": "https://github.com/Irys-xyz/irys/commit/8a49f5fa49cd0a27c79da79fef2e5c7fa877b5bf"
        },
        "date": 1782925380718,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015641,
            "range": "± 0.000616",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.153928,
            "range": "± 0.003324",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.555501,
            "range": "± 0.024454",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.445687,
            "range": "± 0.205208",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083177,
            "range": "± 0.000569",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 783.066213,
            "range": "± 22.666083",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 977.703975,
            "range": "± 12.555494",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120331,
            "range": "± 0.002854",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1219.309997,
            "range": "± 70.165547",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1564.766008,
            "range": "± 6.271146",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034632,
            "range": "± 0.003266",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.047293,
            "range": "± 0.901086",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 272.713319,
            "range": "± 1.341057",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000114,
            "range": "± 0.000003",
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
          "id": "4c4e2ace5a0cccae976c2acb9e03b5a815649b67",
          "message": "chore(consensus): set testnet aurora activation timestamp\n\nAurora hard-fork activation time (2026-02-03) for the testnet reset.",
          "timestamp": "2026-07-01T17:29:57Z",
          "tree_id": "cc6d8bd184583d8d1876a1081b89ccfa419b840d",
          "url": "https://github.com/Irys-xyz/irys/commit/4c4e2ace5a0cccae976c2acb9e03b5a815649b67"
        },
        "date": 1782928218640,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012581,
            "range": "± 0.000341",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.126085,
            "range": "± 0.002991",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.264975,
            "range": "± 0.020267",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.098171,
            "range": "± 0.395121",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.077809,
            "range": "± 0.001386",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 780.922511,
            "range": "± 17.488956",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1012.553295,
            "range": "± 43.192817",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120183,
            "range": "± 0.001515",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1207.082805,
            "range": "± 7.289197",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1577.828051,
            "range": "± 13.319444",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034134,
            "range": "± 0.001288",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 218.024346,
            "range": "± 1.593837",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 275.622997,
            "range": "± 2.234997",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000114,
            "range": "± 0.000004",
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
          "id": "ce26231b1ceba103294d5c81014c897c72e49527",
          "message": "feat(api): add canonical ledger offset -> tx attribution endpoints (#1467)\n\n* feat(api): add canonical ledger offset -> tx attribution endpoints\n\nGET /v1/ledger/{ledger_id}/offset/{ledger_offset}/tx\nGET /v1/ledger/{ledger_id}/slot/{slot_index}/offset/{slot_offset}/tx\n\nResolves a ledger chunk offset to the owning data transaction via the\nblock index and canonical block headers. Works for all data ledgers,\nincluding the Cascade term ledgers (OneYear/ThirtyDay).\n\n* refactor(api): single-snapshot ledger attribution + typed BlockBoundsError\n\n- Resolve the whole offset->tx lookup (bounds search, block header,\n  tx-header walk) inside one MDBX view transaction so a concurrent\n  deep-reorg truncation can't mix index states mid-request, and the\n  tx walk costs one transaction instead of one per tx\n- Add typed BlockBoundsError (IndexEmpty / LedgerInactive /\n  OffsetBeyondFrontier / Internal) on the BlockIndex bounds API and\n  migrate all callers: the API maps not-allocated variants to 404,\n  PoA prevalidation to its typed consensus errors, and cache/storage\n  services drop their frontier-precheck + expect() patterns\n- BlockBounds now carries block_hash from the resolved index item\n  (same snapshot); cross-check header total_chunks against the index\n  and reject tx walks that overshoot the block's appended range\n- Wire contract: txOffset -> txIndex (u32; it is a list index, not a\n  chunk offset), all u64 response fields serialize as strings, slot\n  endpoint 404s restate the client's slot coordinates\n- Share parse_ledger_id across ledger + chunk routes\n\n* fix poa bounds owning hash snapshot\n\n* use ledger accessor for poa inactive error",
          "timestamp": "2026-07-04T09:38:03-07:00",
          "tree_id": "e33caad2f3f8bbfff27b70cc7efce67b2828e363",
          "url": "https://github.com/Irys-xyz/irys/commit/ce26231b1ceba103294d5c81014c897c72e49527"
        },
        "date": 1783183997152,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.01522,
            "range": "± 0.000312",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.152804,
            "range": "± 0.002101",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.558842,
            "range": "± 0.0423",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.452425,
            "range": "± 0.243781",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083206,
            "range": "± 0.000551",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 782.437634,
            "range": "± 27.159884",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 978.582946,
            "range": "± 9.751211",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.117518,
            "range": "± 0.001519",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1183.750351,
            "range": "± 5.26791",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1578.28051,
            "range": "± 105.804734",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.033928,
            "range": "± 0.000978",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.418731,
            "range": "± 1.141344",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.491195,
            "range": "± 2.335947",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "76ba7bdcfb32f589b6e4a180bc529040ba9827a4",
          "message": "feat(consensus): longer anchor-expiry window for commitment txs (+ durable replay protection) (#1463)\n\n* docs: spec for longer commitment-tx anchor expiry depth\n\nSeparate, longer anchor-expiry window for commitment transactions\n(custody/multisig workflows, ~24h). Decouples the anchor-validity\nwindow from replay protection: extending the window requires durable,\nconsensus-level commitment dedup (the current epoch-reset snapshot only\nbounds replay because anchors expire within an epoch).\n\n* docs: implementation plan for commitment anchor expiry depth\n\nTwo phases: (A) durable consensus-level commitment replay protection\n(by-hash ancestry walk within the reorg window + MBH-verified finalized\nlookup below it, reusing IrysCommitmentTxMetadata.included_height), then\n(B) the longer commitment_anchor_expiry_depth window wired through\ningress, pruning, production, and validation. Records the resolved\nfinalized-lookup primitive in the spec.\n\n* feat(db): MBH-verified canonical commitment inclusion lookup\n\n* feat(actors): by-hash ancestor commitment tx_id collector\n\n* feat(consensus): reject replayed commitments in prevalidation\n\n* feat(consensus): reject replayed commitments in full validation\n\n* fix(consensus): park (not discard) on commitment-dedup DB read fault\n\n* test(consensus): reorged-out commitment remains includable\n\n* feat(config): add commitment_anchor_expiry_depth\n\n* chore(config): add commitment_anchor_expiry_depth to docker TOMLs\n\n* feat(mempool): validate commitment anchors against commitment window\n\n* feat(mempool): prune commitments against the commitment window\n\n* feat(block-producer): select commitments over the commitment window\n\n* feat(consensus): validate commitment anchors over the commitment window\n\n* test(consensus): end-to-end commitment anchor window\n\nAdd heavy_e2e_commitment_in_window_anchor_produces_and_validates: mines\na real block via mine_block() containing a commitment anchored beyond\ntx_anchor_expiry_depth but within commitment_anchor_expiry_depth, and\nasserts both that the canonical height advances (the node validated its\nown produced block) and that the block contains the commitment. Proves\nselector (B5) and block-level anchor validation (B6) agree end-to-end.\n\nAlso rename the strat local (ForceCommitmentStrategy/ReplayStrategy\ninstances) to strategy across commitment_anchor_window.rs,\ncommitment_replay.rs, and the design doc, to satisfy the typos gate\nrequired by cargo xtask local-checks.\n\n* test(consensus): pin commitment anchor window in anchor double-check test\n\nThe block-validation anchor double-check test crafted a genesis-anchored\ncommitment expected to be rejected as too-old. With commitments now on a\nseparate, longer anchor window, that commitment inherited the default\n(100) and stayed valid — so pin commitment_anchor_expiry_depth to the same\ntight bound the test already uses for tx anchors, keeping the\ncommitment-anchor double-check meaningful.\n\n* feat(config): require ingress_proof_anchor_expiry_depth >= tx_anchor_expiry_depth\n\nDocuments the anchor-set-depth assumption in block_discovery: the ingress\nanchor set is built from the by-hash tx-anchor walk extended by the\nfinalized index, so the ingress window must be at least the tx window.\nProduction-gated, matching the sibling tx_anchor_expiry_depth <=\nblock_tree_depth invariant; no shipped config violates it.\n\n* perf(consensus): batch commitment dedup reads into one db txn\n\nBoth replay-dedup paths opened one MDBX read txn per commitment for the\nfinalized-inclusion lookup; wrap all lookups in a single read txn instead.\nDedup decisions (which blocks are rejected) are unchanged.\n\n* chore: clarify commitment-selection log field and stale anchor-set comment\n\n* refactor(mempool): select anchor-expiry depth on the tx type\n\nvalidate_tx_anchor and should_prune_tx took the anchor-expiry depth as a\nparameter, so every call site had to pick the right window for the tx kind\n(and could pass the wrong one — the compiler wouldn't catch a data-tx depth\nhanded to a commitment). Move the choice onto IrysTransactionCommon via\nanchor_expiry_depth(&consensus): data txs report tx_anchor_expiry_depth,\ncommitments report commitment_anchor_expiry_depth. The two functions now\nderive the depth from the tx, so no caller can mismatch it.\n\n* perf(consensus): bound anchor-validation walk to the reorg window\n\nBlock validation built a map of every canonical anchor block down to\nmin(ingress_floor, commitment_floor) — on mainnet the commitment window is\n~7200 blocks, so every block (even commitment-free ones) walked ~7200\nblock-index entries, up from ~200 pre-commitment-window.\n\nThe eager map only needs branch-correct coverage of the reorg window\n(block_tree_depth): below the reorg floor the chain is finalized, so an\nanchor there is branch-invariant and can be resolved on demand via an\nMBH-verified canonical_block_height_by_hash lookup. Cap the index walk at\nthe reorg floor and add a finalized fallback in anchor_valid_for (a\nnot-in-map anchor is valid only if it resolves strictly below map_floor and\nwithin the item's window; at/above the floor it is a sibling-branch anchor\nand is rejected). Anchor validation is now O(block_tree_depth + items)\ninstead of O(anchor_expiry_depth). NC-0042 by-hash/index handoff unchanged.\n\n* refactor(consensus): clarify block_discovery anchor validation\n\nBehavior-preserving cleanup after commitments gained a divergent (longer)\nanchor window:\n- group the three per-item anchor floors + the two map bounds (reorg_floor,\n  map_floor) with a single explanatory comment\n- make the ancestry walk unconditional (the anchor map is needed by\n  commitment and ingress anchors, not just submit txs) with submit-ledger\n  duplicate detection as an inner check; the Submit ledger is always present\n  so this is behavior-preserving\n- reuse reorg_floor in the commitment replay dedup instead of recomputing an\n  identical finalized_floor\n\n* fix(consensus): content-verify finalized commitment-inclusion lookups\n\ncanonical_commitment_included_height trusted the metadata included_height\nhint once MigratedBlockHashes recorded any canonical block at that height.\nA row is written at tip-confirmation (depth 0), so an orphaned local tip\ncan strand a hint that no ReorgEvent ever clears across a restart+reorg.\nOnce a different winning block migrates at that height, the MBH check alone\nreads the stranded hint as canonical truth -> false DuplicateCommitmentTransaction.\n\nLoad the canonical header at MBH[h] and require it to actually carry the\ntxid in its commitment ledger before returning Some(h). A missing header\nfor the attested hash is a cross-table inconsistency and fails loud (Err);\nboth dedup call sites already map Err to the safe SoftInternal-park path.\n\n* fix(consensus): fail closed on unknown ancestor in commitment dedup walk\n\nThe by-hash ancestor walk returned a partial set (None => break) when an\nin-window ancestor resolved in neither the block tree nor the DB. A partial\nset can miss a replay included in an ancestor below the break point but still\ninside the reorg window, letting it slip past both the by-hash set and the\nfinalized lookup.\n\nEvery ancestor of a validatable block was itself validated-and-stored, so an\nin-window gap is a local inconsistency. bail! loud (naming the missing hash,\nthe block under validation, and heights) like resolve_promoted_on_branch.\nBoth callers already map the error to a SoftInternal-park path (retry, don't\naccept or reject). The clean stops (below min_height, genesis) still return.\n\n* perf(consensus): skip commitment replay dedup when the block has no commitments\n\nEvery non-epoch block ran the by-hash ancestor walk (up to block_tree_depth\nheaders) and a finalized-lookup read txn even carrying zero commitment txs,\nthe overwhelmingly common case.\n\nGate both on !commitment_txs.is_empty(). In block_discovery the commitment\nanchor loop is unchanged (a no-op over an empty slice) and only the dedup\nwalk + read are gated. In commitment_txs_are_valid an early return after the\nepoch branch short-circuits the walk, read, and simulated-snapshot loop; the\nsnapshot loop was already a no-op on empty and the tail already returned Ok\nthere, so this is behavior-preserving (the now-redundant tail empty-check is\nremoved).\n\n* perf(consensus): short-circuit the in-memory dedup check before the DB read\n\nThe dedup loops computed canonical_commitment_included_height (a DB read)\nbefore testing the in-memory prior_commitment_ids set, so the read ran even\nwhen the reorg-window set already matched. Test the in-memory set first and\nonly do the read on a miss, letting || short-circuit. The reported tx_id is\nunchanged.\n\n* fix(consensus): epoch blocks apply commitments — only inclusion blocks write dedup metadata\n\nNon-genesis epoch blocks re-list (apply) every commitment of the epoch but\nnever first-include one, yet the ledger-driven metadata path treated their\nconfirmation/migration like any other block: it overwrote each commitment's\n`included_height` with the epoch height and deleted the rows on orphan. A\nreorg orphaning an epoch block then stranded the dedup rows of commitments\nwhose true inclusions remained canonical below the fork point, opening a\nreplay hole the by-hash walk (bounded to block_tree_depth) cannot cover\nacross a full epoch.\n\n`included_height` must name the block that INCLUDED a commitment, never the\nepoch block that APPLIED it. Skip commitment-metadata create/overwrite (Phase\n2 / persist_block) and delete (Phase 1) for non-genesis epoch blocks; data-tx\nmetadata and IrysCommitments inserts are untouched. Genesis (height 0)\ngenuinely first-includes, so it retains normal behavior. Thread\nnum_blocks_in_epoch into BlockMigrationService for the epoch check.\n\n* fix(consensus): backfill genesis commitment inclusion metadata at startup\n\nGenesis first-includes its commitment txs but, as the block-index head,\nnever flows through the confirm/migration metadata writers, so no\nIrysCommitmentTxMetadata.included_height row is written for it. Once\ngenesis falls below the reorg floor the commitment replay-dedup's\nfinalized lookup (canonical_commitment_included_height) is the only path\nthat can catch a replayed genesis commitment — the by-hash ancestry walk\nonly covers the reorg window — so a producer bypassing the mempool could\nreplay a genesis Stake and double-debit the signer.\n\nAdd backfill_genesis_commitment_included_height and call it at the single\nstartup seam that runs on both fresh init and restart (after genesis is\nguaranteed persisted). The write is unconditional and trivially\nidempotent; running it every boot lets already-initialized data dirs\nconverge on upgrade restart rather than diverging from fresh nodes. A\nmissing MigratedBlockHashes[0] or genesis header fails loud.\n\n* refactor(consensus): consolidate commitment replay dedup into one shared helper\n\n* perf(consensus): reuse one read transaction for on-demand anchor resolution\n\n* refactor(db): extract the canonical-header-at-height primitive\n\n* refactor(consensus)+test: single owner for commitment-inclusion writes; shared test scaffolding\n\n* refactor actors ancestry walks\n\n* fix(consensus): skip unresolvable-anchor txs in selection; reject same-block duplicate commitments\n\nAn orphan-anchored tx sitting in the mempool must not abort block\nproduction. `validate_anchor_for_inclusion` now returns Ok(false) (skip)\nwhen an anchor can't resolve to a canonical block, mirroring the ingress\npath; both selector call sites already skip on Ok(false) and still\npropagate genuine DB errors. Otherwise a commitment whose anchor was\nreorged out could poison every production attempt for the whole\ncommitment-anchor-expiry window.\n\n`find_replayed_commitment` also now rejects a commitment listed twice\nwithin the same block. `CommitmentSnapshot::add_commitment` is idempotent\nfor Stake/Pledge, so the snapshot alone cannot reject a same-block\nduplicate; this is the layer that catches it (defense-in-depth behind the\nheader/body reconciliation backstop).\n\nCovered by unit tests plus end-to-end regression tests for both paths.\n\n* style(consensus): use widening conversions for anchor expiry depths\n\nReplace lossless as-casts with .into()/u64::from() for u8/u16 anchor\nexpiry depths, and centralize the tx-selector min-anchor margin math in\na shared closure so the data-tx and commitment windows can't drift.\n\nRemove superseded superpowers plan/spec docs.\n\n* chore: fmt\n\n* harden(consensus): reject inflated block height before anchor walk; review followups\n\nblock_discovery: reject a block whose height != parent.height + 1 before the\nanchor index-walk. An inflated height (parent = the real tip) drove the\nreorg_floor..bt_finished_height loop into not-yet-migrated heights and panicked\nthe node; prevalidation's height_is_valid runs after the walk, too late to guard\nit. Convert the walk's two internal block-index assertions from panic! to\nBlockDiscoveryInternalError so index inconsistency parks the block instead of\naborting the process.\n\nblock_ancestry: stop the ancestry walk on the floor height instead of fetching\none below-floor parent and then discarding it (drops an extra header read and\nits fail-closed bail! path).\n\ndatabase: canonical_header_at_height cross-checks the resolved header's height\nagainst the requested height, guarding against cross-table corruption.\n\ntests: extract a shared InjectCommitmentsStrategy and shared node/setup\nscaffolding across the commitment replay and fork-recovery tests; strengthen\nheavy_commitment_reorged_out_stays_includable to assert the stake+pledge tx ids\nactually land in the orphaned block, so the dedup false-positive path is\nexercised rather than passing vacuously.\n\nMinor polish: u64::from over an as-cast in tx_selector, soften an\nunenforced-ordering doc comment, drop a stray blank line.\n\n* harden(consensus): dedup finalized commitments in block production selection\n\n`select_best_txs` deduped commitment candidates only against the block-tree\ncanonical cache (~block_tree_depth blocks). A commitment finalized below\n`tip - block_tree_depth` is pruned from that cache but stays in the mempool\n(confirmation stamps `included_height` without evicting it) while still inside\nits longer `commitment_anchor_expiry_depth` window, so it remained a selection\ncandidate and could be re-included — building a block the producer's own\nvalidation rejects as a replay via `find_replayed_commitment`.\n\nExtend selection with the same durable finalized-inclusion index the validator\nuses (`canonical_commitment_included_height`), in a single read txn over the\ncandidate set. The floor is anchored at `tip - block_tree_depth` so the\nin-memory reorg-window set and the finalized index meet with no coverage gap\neven when building on a non-tip parent, and never over-skip: the parent is\ncanonical and within the cache, so every height at or below the floor is both\nfinalized and on the parent's ancestry.",
          "timestamp": "2026-07-07T18:34:51+01:00",
          "tree_id": "7fb7a5a3ebd0d757fa04df922a11405584062b42",
          "url": "https://github.com/Irys-xyz/irys/commit/76ba7bdcfb32f589b6e4a180bc529040ba9827a4"
        },
        "date": 1783446879623,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.011989,
            "range": "± 0.00028",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.152845,
            "range": "± 0.00234",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.262565,
            "range": "± 0.029641",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.355532,
            "range": "± 0.279815",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.07763,
            "range": "± 0.001706",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 765.724676,
            "range": "± 12.518602",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1015.444597,
            "range": "± 48.405696",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.119936,
            "range": "± 0.000485",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1213.76496,
            "range": "± 13.647638",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1599.944824,
            "range": "± 22.053164",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034946,
            "range": "± 0.000734",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 208.685878,
            "range": "± 0.253137",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 271.247753,
            "range": "± 0.231917",
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
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "c126b5aed936a8c734f71090407d6b0fa753eab6",
          "message": "fix(consensus): branch-correct data-tx publish selection + content-verified canonical lookups (#1468)\n\n* fix(consensus): branch-correct data-tx publish selection + content-verified canonical lookups\n\nProducer publish-candidate selection gated on the node-local\n`promoted_height` hint, which is branch-variant under reorg: it can read\n`None` for a tx already promoted on the parent's branch, so the producer\nre-selects it and mints a block validators reject as a double-promotion.\nResolve promotion from the parent's own ancestry (`resolve_promoted_on_branch`,\nby-hash over the reorg window, MBH-verified below the floor) and filter the\ncommitment/Submit dedup folds to `Onchain` ancestors.\n\n`canonical_submit_height`/`canonical_promoted_height` trusted a metadata hint\non `MigratedBlockHashes` existence alone. The row is written at tip\nconfirmation, so a stranded hint from an orphaned tip that survives a\nrestart-interrupted reorg can collide with a different block migrated at the\nsame height, yielding a false PublishTxAlreadyIncluded / PublishTxMissingPriorSubmit.\nContent-verify the canonical block actually carries the tx in the relevant\nledger, matching `canonical_commitment_included_height`.\n\n* fix(consensus): write canonical tx/commitment metadata only at migration\n\n`persist_metadata` wrote `IrysDataTxMetadata`/`IrysCommitmentTxMetadata` at\nblock confirmation (depth 0), before the block is durably persisted. An\norphaned confirmed-but-unmigrated tip could strand such a row; once a different\nblock later migrated at the same height, the `MigratedBlockHashes`-existence\ncheck read the stale row as canonical — the root of the branch-safety hazard.\n\nWrite these rows only at migration (`persist_block`), atomically with\n`MigratedBlockHashes` and the block header, so a metadata row cannot exist\nwithout its MBH entry naming the same canonical block. Unmigrated blocks are\nalready served branch-correctly from the in-memory block tree, so consensus\nnever needed the confirmation-time rows. The non-consensus\n`CachedDataRoots.block_set` hint stays at confirmation — the ingress-proof\npipeline depends on it pre-migration.\n\nQuery paths that reported pre-migration state from the DB now read the live\nmempool for the unmigrated window: the promotion-status endpoint and the\n`get_is_promoted` test helper fall back to the mempool when the DB lacks\n`promoted_height`.\n\nThis intentionally drops the prior \"status survives a node restart before\nmigration\" behavior: an unmigrated block is not durably persisted, so after a\nrestart the mempool restores its txs as pending. Reporting them CONFIRMED named\na block the node no longer had. Tests updated accordingly.\n\nThe content-verified `canonical_*_height` lookups are kept as defense-in-depth\nfor the deep-reorg-past-migration case.\n\n* test(consensus): share plant_canonical_block helper across tests\n\nThe synthetic-canonical-block setup (insert a mock header carrying given\nledger tx_ids, repoint MigratedBlockHashes) was duplicated between the\nblock-validation and mempool-ingress-dedup tests. Extract one helper in\nutils.rs taking flexible (DataLedger, Vec<H256>) ledger pairs; both tests\ncall it. Behavior-preserving.\n\n* docs(consensus): correct stale confirmation-time metadata-write comments\n\nCanonical tx/commitment metadata (included_height / promoted_height) is now\nwritten only at migration (persist_block), not at confirmation. Update the\ncomments that still described the old depth-0 confirmation write path:\n\n- reframe stranded-row rationale in terms of legacy/stale rows not backed by\n  canonical block content (block_validation, database canonical_* helpers)\n- persist_metadata no longer has a Phase 2 nor writes canonical metadata;\n  fix references in cache_service, db_index, mempool lifecycle\n- a present IrysDataTxMetadata row means migrated (finalized), not merely\n  confirmed (cache_service scrub recheck)\n- genesis backfill / test docs: 'confirm/migration' -> 'migration' writer\n\nComment-only; no behavior change.\n\n* perf(consensus): skip ancestry walk for empty promotion-resolver input\n\nresolve_promoted_on_branch unconditionally ran walk_ancestors_tree_then_db,\nwhich takes the block-tree read lock and fetches at least the parent header\nbefore the visit closure can break. tx_selector reaches it with an empty\ncandidate list (publish candidates filtered to none), so guard on an empty\ninput and return the empty set directly. Behavior-identical.",
          "timestamp": "2026-07-07T22:10:37+01:00",
          "tree_id": "c3f03dff54ad600b3d5da784482575c324571447",
          "url": "https://github.com/Irys-xyz/irys/commit/c126b5aed936a8c734f71090407d6b0fa753eab6"
        },
        "date": 1783459740196,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012151,
            "range": "± 0.000263",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.118789,
            "range": "± 0.00732",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.192267,
            "range": "± 0.008998",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.917343,
            "range": "± 0.429124",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.074966,
            "range": "± 0.000682",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 751.393062,
            "range": "± 6.218182",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1016.793149,
            "range": "± 33.688787",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.119849,
            "range": "± 0.0004",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1202.47823,
            "range": "± 11.17467",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1572.384379,
            "range": "± 5.546212",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034483,
            "range": "± 0.00154",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.687372,
            "range": "± 0.954542",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 275.101621,
            "range": "± 1.944819",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0.000005",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "603958f6004ca6e562d3433cd007ac9317e7c013",
          "message": "fix(consensus): fork-safe canonical metadata lookups + reorg-floor prevalidation cap (#1464)\n\n* fix(consensus): fail loud on a non-advancing block-index probe\n\nThe find_block_range probe loop advances by setting the next offset to\nthe resolved block's cumulative total. A well-formed index guarantees\nthat total exceeds the probed offset (the offset lies within the block),\nbut a corrupted index could return a total at or below it — leaving the\nloop spinning forever instead of surfacing the fault. Guard the\ninvariant with ensure! naming the ledger, the resolved height, and both\noffsets so corruption errors out load-bearing paths instead of hanging\nblock production.\n\n* fix(consensus): cap the promotion-prevalidation DB fallback at the reorg floor\n\nThe Searching{Publish} fallback in data_txs_are_valid capped\ncanonical_submit_height / canonical_promoted_height at parent_height.\nMetadata+MBH rows exist down to migration depth, so rows in the\nreorg-mutable band (tip - block_tree_depth, tip - block_migration_depth]\ncould decide validity — but those rows describe the node's LOCAL chain,\nnot the candidate's branch. On a fork deeper than the migration depth, a\nsibling-branch-only Submit inclusion in the band satisfies the\nprior-Submit requirement on nodes that saw it while others reject with\nPublishTxMissingPriorSubmit (false-accept), and a local-branch-only\npromotion makes an honest fork block fail PublishTxAlreadyIncluded\n(false-reject) — either way the network diverges.\n\nCap both lookups at block.height - block_tree_depth. The by-hash walk\n(prior_inclusion_walk_depth >= block_tree_depth) already resolves every\nin-window inclusion on the candidate's own branch, so walk + capped\nfallback cover [0, block.height) with no gap: finalized heights\nby-height, the reorg window by-hash. The old comment claimed any\ninclusion reaching the fallback was below the reorg floor — true only\nfor the candidate's branch; the cap is what enforces it for node-local\nrows.\n\nAdd a band-cap regression pinning the invariant the fallback relies on:\na canonical, content-valid row inside the band is visible at the parent\ncap but excluded at the reorg-floor cap. A unit test of the full\ndata_txs_are_valid fallback arm would require a bespoke harness (no\nexisting one) and a multi-node band-fork integration test is out of\nscope here; the cap semantics are covered at unit level.\n\n* test(chain): seed content-verified canonical priors in the prevalidation fallback fixtures\n\nThe content check in the canonical metadata lookups made hint-only\nfixtures resolve to None: a hint is only canonical if the block MBH\nnames actually carries the tx. A content-verified prior must also sit\nbelow the inclusion-history walk window, or the walk itself resolves it\nand the fallback arm under test never fires. Rebuild the fixtures on a\nheight-3 chain with a fabricated finalized prior at height 1.\n\n* fix(consensus): cap the promotion-prevalidation fallback strictly below the walk window\n\nThe inclusive floor cap trusted a boundary height where a node-local\nsibling row could survive a missed scrub; since Searching state means\nthe walk found nothing on the candidate's branch in-window, a fallback\nanswer there is never a true positive, so the strictly-below cap removes\nthe exposure with zero coverage loss and matches the expiry-side\nfallback.\n\n* test(chain): pin the promotion-fallback cap's exclusion side end-to-end\n\nThe existing fixtures pass under a loosened cap, so they cannot catch a\nregression toward trusting in-window node-local rows; this fixture seeds a\ncontent-verified row inside the walk window and asserts it never decides\nvalidity.\n\n* style(chain-tests): regroup a hex literal into equal-size digit groups for clippy\n\n* fix(consensus): dedup producer tx selection against the full parent ancestry\n\nThe publish/commitment dedup folds filtered the parent's canonical\nancestry to `Onchain` entries only. The validator dedups against the\nparent ancestry by hash regardless of `ChainState`\n(`commitment_dedup::ancestor_commitment_tx_ids`, `data_txs_are_valid`'s\ninclusion walk), so during reorg replay a transiently-`Validated`\nin-window ancestor was dropped from the producer's sets while the\nvalidator still saw it — letting the producer re-select a tx and mint a\nblock peers reject as a duplicate commitment / double-publish.\n\n`get_canonical_chain` builds its chain by walking `previous_block_hash`\nfrom the max-difficulty tip, so every entry in the truncated `canonical`\nslice is a genuine ancestor of the block being produced. Deduping against\nall of them, regardless of chain state, is what keeps the producer\nsymmetric with the validator; an ancestor that later rolls back takes the\nparent with it, so the attempt is on a doomed branch that self-corrects.\nSkipping non-`Onchain` entries is only safe in `lookup_via_block_tree`,\nwhich has a by-hash slow-path fallback the dedup folds lack.\n\nAlso correct the `canonical_*_height` docstring: the content check proves\ninclusion on this node's canonical chain, and `Some(h)` is only\nbranch-agnostic below the reorg floor, since `MigratedBlockHashes` is\nnode-local within the reorg window.",
          "timestamp": "2026-07-08T09:39:36+01:00",
          "tree_id": "af52de41a0b5c90d532ede0b94973e8248674d0d",
          "url": "https://github.com/Irys-xyz/irys/commit/603958f6004ca6e562d3433cd007ac9317e7c013"
        },
        "date": 1783501069593,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012654,
            "range": "± 0.000161",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.129765,
            "range": "± 0.008831",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.29923,
            "range": "± 0.138772",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.336879,
            "range": "± 0.200358",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.076092,
            "range": "± 0.001452",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 751.040532,
            "range": "± 7.193767",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1014.966571,
            "range": "± 36.889525",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.137896,
            "range": "± 0.009158",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1374.505597,
            "range": "± 7.903091",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1801.677542,
            "range": "± 15.832204",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034505,
            "range": "± 0.001447",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.13668,
            "range": "± 1.958125",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.862314,
            "range": "± 1.196494",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000121,
            "range": "± 0.000003",
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
          "id": "2692ccb8151ce011e4046e9d322da8179bd384a6",
          "message": "feat: canonicalize testnet consensus config",
          "timestamp": "2026-07-08T10:43:12Z",
          "tree_id": "f03c79f6f3f79d868c8bbef164f37e5f9c684296",
          "url": "https://github.com/Irys-xyz/irys/commit/2692ccb8151ce011e4046e9d322da8179bd384a6"
        },
        "date": 1783509114588,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012066,
            "range": "± 0.000236",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.118884,
            "range": "± 0.001576",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.196004,
            "range": "± 0.049276",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.267865,
            "range": "± 0.187574",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075729,
            "range": "± 0.00152",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 751.018548,
            "range": "± 8.59704",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1019.20566,
            "range": "± 40.700945",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120187,
            "range": "± 0.000461",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1205.965555,
            "range": "± 15.649254",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1583.170623,
            "range": "± 12.635222",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034193,
            "range": "± 0.00126",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 211.243634,
            "range": "± 2.694067",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.729202,
            "range": "± 2.409255",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000117,
            "range": "± 0.000004",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "18c3ed1e8fa530a657d8d766e2452140a516881c",
          "message": "feat: flaky nextest detection (#1472)\n\n* feat(xtask): nextest-based three-phase flaky-test detection\n\nReplace the cargo-flake wrapper with a nextest-native `cargo xtask flaky`\nthat reuses the existing nextest-wrapper stats infrastructure.\n\nPhases:\n1. Full suite xN under normal parallelism -> suspects (any test that\n   fails at least once), pass/fail read from per-iteration wrapper stats.\n2. Stress the suspect set together at high concurrency xK to reproduce\n   peer contention cheaply.\n3. Run each suspect in isolation (single-threaded, one process) xY,\n   capturing full logs; pass/fail from the process exit code.\n\nSuspects are classified as GENUINE FLAKY / BROKEN / TIMEOUT-BOUND (gate\nCI) vs CONTENTION (peers / full-suite) (warn only). Emits report.md +\nreport.json under target/nextest-monitor/flaky/<ts>/, feeds genuine\nflakes into failures.json, and exits non-zero when genuine flakes exceed\n-f/--tolerable-failures.\n\nEnforces retries=0, builds tests once up front, and never fail-fasts so\nevery iteration observes the full suite. Shared xtask helpers moved to a\nnew xtask::util module.\n\nUpdates flaky.yml, CLAUDE.md, and the debugging-flaky-tests skill.\n\n* feat(xtask): richer per-test isolated failure logs\n\nIsolation phase now runs with --no-capture and RUST_BACKTRACE=full so each\nrun's full stdout/stderr (panics, tracing) is captured. Log files are\nrenamed by outcome (run-N.FAIL.log / run-N.TIMEOUT.log / run-N.pass.log)\nand a per-test summary.txt records each iteration's result, making a\nflaky test's failure history readable at a glance.\n\n* feat(xtask): flag-gated JSON report on stdout for CI/tooling\n\nAdd `--json` to `cargo xtask flaky`: on completion it emits the report as\na single compact JSON line on stdout, bracketed by\n<<<FLAKY_REPORT_JSON_BEGIN>>> / <<<FLAKY_REPORT_JSON_END>>> sentinels so\ndownstream tooling can extract it amid the streamed nextest output without\ndiscovering the timestamped report.json path.\n\nflaky.yml now runs with --json and extracts the payload via sed, using it\nas the authoritative source for genuine_flakes/total_suspects metrics\n(falling back to the report.json file), and uploads run-output.log.\n\n* fix: tests using testnet() consensus config\n\n* ci(flaky): fail the job when genuine flakes exceed tolerance\n\nThe run step keeps continue-on-error so reporting/upload still happen, but\na final step now re-raises the captured exit code (non-zero only when\ngenuine flakes exceed -f, or on a run error). A red job triggers GitHub's\nnative scheduled-failure email and any Slack GitHub-app workflow\nsubscription — no webhook secret required.\n\n* feat(xtask): targeted mode, failure signatures, verify fast-path\n\nOptimize the flaky tool for the agent-driven \"diagnose these known flaky\ntests and fix them\" loop:\n\n- Targeted mode: --tests 'a,b' and --tests-from <report.json|failures.json>\n  skip 40-min full-suite discovery; phase 1 is scoped to the given tests\n  (still gets the contention signal) before stress + isolation.\n- Failure signatures: parse each isolated run's captured output for the\n  panic/assertion message + file:line, dedupe distinct signatures per test,\n  and surface them in report.json / report.md / stdout — the \"why\" without\n  opening logs.\n- Verify fast-path: --verify 'a,b' runs isolation only (post-fix check);\n  a test that passes every isolated run is classified CLEAN (exit 0).\n- Report iteration counts now reflect what actually ran (0 for skipped\n  phases in verify mode).\n- debugging-flaky-tests skill routes agents to the targeted/--verify/--json\n  path with the report schema.\n\n* fix(xtask/flaky): address review findings\n\n- flaky.yml: pass workflow_dispatch inputs and github/runner context via\n  quoted shell env expansion instead of ${{ }} interpolation inside run\n  scripts, closing a script-injection vector.\n- classify: with isolation skipped, a stress failure no longer returns\n  PeerContention (which asserts the test passes alone — unprovable without\n  isolation); it stays Unverified. Add regression test.\n- clean: --clean now runs `cargo clean` (previously only rebuilt), and the\n  --no-run prebuild fails fast instead of swallowing compile errors.\n- isolation: IsoResult::NoMatch is now a hard error as documented, rather\n  than breaking out and being silently classified as contention/clean on\n  zero recorded runs.\n- failures.json: always rewritten for the current run — cleared when there\n  are no genuine flakes so `--rerun-failures` can't re-run stale entries.\n\n* refactor(xtask): share NEXTEST_VERSION between test and flaky\n\nHoist the pinned cargo-nextest version into a shared util constant so\n`xtask flaky` installs the same version as `xtask test` whenever the\npin changes, instead of drifting via a hardcoded copy.\n\n* feat: address feedback",
          "timestamp": "2026-07-08T18:43:31+01:00",
          "tree_id": "222e447f219e71060073291091f503d0c4015958",
          "url": "https://github.com/Irys-xyz/irys/commit/18c3ed1e8fa530a657d8d766e2452140a516881c"
        },
        "date": 1783533577850,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.01613,
            "range": "± 0.002235",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.155383,
            "range": "± 0.011278",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.584417,
            "range": "± 0.105902",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.498468,
            "range": "± 0.309044",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083896,
            "range": "± 0.002157",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 836.973302,
            "range": "± 10.878018",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1030.098606,
            "range": "± 43.223691",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.159635,
            "range": "± 0.013848",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1212.190352,
            "range": "± 15.634588",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1566.905929,
            "range": "± 127.472961",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.038035,
            "range": "± 0.002939",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 220.980093,
            "range": "± 9.825404",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 281.003496,
            "range": "± 4.014158",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0.000005",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "bcb7a333bae7b751672ce9e4199d27d1573fd4e1",
          "message": "fix(consensus): strictly-below dedup fallback caps + CDR scrub confirmed-window recheck (#1471)\n\n* fix(consensus): cap dedup finalized fallbacks strictly below their branch-resolved windows\n\ncommitment_dedup: the finalized replay lookup was capped AT the walk floor\n(height - block_tree_depth), overlapping the by-hash walk's inclusive\ncoverage of that height. A node-local MigratedBlockHashes row at the\nboundary is reorg-mutable, and since the walk already resolves the floor on\nthe candidate's own branch, a lookup answer there is never a true positive\n— only a potential false DuplicateCommitmentTransaction (a hard reject) on\na maximal-depth fork whose boundary sibling carries the same commitment.\nCap the lookup strictly below the floor; the walk covers [floor, height),\nso coverage is unchanged. This matches the data-tx fallbacks in\ndata_txs_are_valid and resolve_promoted_on_branch, which already cap\nstrictly below their walk windows.\n\ntx_selector: the publish prior-Submit gate capped canonical_submit_height\nat the parent height, admitting node-local reorg-band rows into the answer.\nThe canonical fold already resolves the parent's ancestry down to the cache\nfloor, so cap the DB lookup at tip - block_tree_depth — finalized,\nbranch-invariant heights only — mirroring the commitment selection dedup's\nfinalized floor. Anchoring at the tip rather than the parent keeps the band\n(parent - depth, tip - depth] covered when building on a non-tip parent.\n\n* test(consensus): pin the dedup finalized-fallback caps on both sides\n\ncommitment_dedup unit tests: a node-local canonical inclusion at exactly\nthe walk floor must never decide replay-validity (the walk owns that\nheight on the candidate's branch), while a finalized inclusion strictly\nbelow the floor must still be flagged — the stricter cap loses no\ncoverage.\n\nchain-tests: producer-side sibling of the validator pin\ntest_prevalidation_ignores_content_verified_row_inside_walk_window. A\ncontent-verified canonical Submit row inside the reorg-mutable band must\nnot satisfy the publish prior-Submit gate (in-band inclusions are owned\nby the canonical fold, never the DB), and a finalized row at or below\ntip - block_tree_depth must still promote — the cap does not over-skip.\n\nBoth regression arms were verified to fail against the pre-fix caps.\n\n* fix(cache): recheck the canonical tree window before scrubbing CachedDataRoots txids\n\nThe txid scrub's re-confirmation recheck keyed solely on IrysDataTxMetadata\nexistence. Canonical metadata is written only at migration, so a tx\nre-confirmed in a still-unmigrated block during the scrub window had no row\nyet and was wrongly scrubbed from txid_set — letting the prune loop evict\nchunks that chunk migration still needs.\n\nConfirmed-but-unmigrated inclusions live only in the in-memory block tree,\nso snapshot the canonical tree window's data-ledger txids before the write\ntxn (guard released before DB I/O) and keep a queued txid when either\nsignal holds: metadata row present (migrated) or in the canonical window\n(confirmed, not yet migrated). Only the canonical chain counts —\n\"(re-)confirmed\" means on the branch this node currently follows; a kept\nentry is re-scrubbed by the next TTL/reorg pass, so over-keeping is safe.\n\nThe scrub body moves out of the spawned thread into\nprune_txids_from_cached_data_roots so the recheck is directly testable;\nthread wrapper, panic handling, and completion signaling are unchanged.\nThe residual race (gossip re-adds a tx that has confirmed nowhere) remains\nas documented.\n\nThe regression test was verified to fail with the tree-window recheck\ndisabled.\n\n* test(consensus): pin the saturated dedup floor and tighten the gate-test geometry guard\n\ncommitment_dedup: near genesis (block_tree_depth >= height) the walk floor\nand the finalized cap both saturate to 0, leaving the walk owning the whole\n[0, height) range. Pin that coverage does not break down there: an\nin-window inclusion is still flagged (via the walk — the metadata row sits\nabove the saturated cap), a never-included commitment is not, and the\ndouble saturating_sub cannot underflow. The walk's DB phase descends to\nheight 0, so genesis is stored in the DB as in production.\n\npublish_gate_reorg_band: the geometry assert only guarded\ncontrol_height = floor - 1, not the later parent lookup at floor - 2,\nwhich underflows at floor == 1. Require floor >= 2 so the guard covers\nevery height subtraction it exists to protect.",
          "timestamp": "2026-07-08T18:55:15+01:00",
          "tree_id": "c1baf6d26d1d5ac313f0a0673a1084b8ae6b06e0",
          "url": "https://github.com/Irys-xyz/irys/commit/bcb7a333bae7b751672ce9e4199d27d1573fd4e1"
        },
        "date": 1783534478283,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012434,
            "range": "± 0.00051",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.120525,
            "range": "± 0.003517",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.2147,
            "range": "± 0.064507",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.099755,
            "range": "± 0.237626",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.077438,
            "range": "± 0.001925",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 753.307951,
            "range": "± 4.901297",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 979.904204,
            "range": "± 5.225859",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.117632,
            "range": "± 0.001834",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1203.874905,
            "range": "± 124.128148",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1592.292681,
            "range": "± 114.873231",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.033552,
            "range": "± 0.000857",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 218.790726,
            "range": "± 10.343125",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 308.985077,
            "range": "± 16.718627",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000148,
            "range": "± 0.000014",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "94fc7fc24f11f49ad65e9f06b7531af17c36e6b8",
          "message": "fix(ci): remove empty GitHub-expression from flaky workflow run block (#1473)\n\nGitHub Actions evaluates ${{ }} across the entire run: block string,\nincluding shell comments. A literal empty expression written as prose in\na comment made the workflow invalid (\"An expression was expected\"),\nanchored to the block-scalar start. Reword the comment to drop the token.",
          "timestamp": "2026-07-08T19:10:55+01:00",
          "tree_id": "a0b44dbd8b57a72e793765a62845ca5d87fbde23",
          "url": "https://github.com/Irys-xyz/irys/commit/94fc7fc24f11f49ad65e9f06b7531af17c36e6b8"
        },
        "date": 1783535395251,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012504,
            "range": "± 0.000744",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.12548,
            "range": "± 0.005427",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.226388,
            "range": "± 0.034895",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 7.925571,
            "range": "± 0.300638",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.074598,
            "range": "± 0.000314",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 749.203945,
            "range": "± 4.313461",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 976.153649,
            "range": "± 7.032608",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120528,
            "range": "± 0.002213",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1235.086308,
            "range": "± 32.350844",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1589.147948,
            "range": "± 21.298636",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034548,
            "range": "± 0.001931",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 228.674143,
            "range": "± 14.625423",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.223946,
            "range": "± 2.860343",
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
          "id": "f5b7794757bb04a94b4202b20641a6df01a82b57",
          "message": "fix(consensus): validate data ledger total_chunks in prevalidation (#1475)\n\nRecompute each ledger's cumulative total_chunks as parent total +\nchunks_added(included txs) and reject TotalChunksMismatch when the\nsigned header value diverges. Mirrors the existing tx_root integrity\ncheck so a peer cannot inflate/deflate capacity, expiry write-windows,\nor PoA bounds while keeping a valid tx_root.",
          "timestamp": "2026-07-10T13:21:04-07:00",
          "tree_id": "5532691390f4881b33ecafa8cd88269c76d80a66",
          "url": "https://github.com/Irys-xyz/irys/commit/f5b7794757bb04a94b4202b20641a6df01a82b57"
        },
        "date": 1783715817187,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015162,
            "range": "± 0.000351",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.153106,
            "range": "± 0.007555",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.55778,
            "range": "± 0.025052",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 9.924174,
            "range": "± 0.985775",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078804,
            "range": "± 0.000411",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 768.99628,
            "range": "± 25.036984",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 974.647252,
            "range": "± 6.691705",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120425,
            "range": "± 0.001185",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1221.511502,
            "range": "± 81.896759",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1563.641657,
            "range": "± 9.323097",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034184,
            "range": "± 0.000879",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.223417,
            "range": "± 0.976013",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.206265,
            "range": "± 1.577454",
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
          "id": "81128607c548c5de8195909b9ecf948ca759916c",
          "message": "feat(api): add GET /v1/mempool/txs pending transaction list (#1480)\n\n* feat(api): add GET /v1/mempool/txs pending transaction list\n\nExpose light metadata for unconfirmed mempool data and commitment txs so clients can list pending ids that match block ledgers and GET /v1/tx/{id}, without changing /v1/mempool/status.\n\n* style: cargo fmt\n\n* fix(api): harden mempool txs list and tighten docs\n\nInclude pending pledges, add after_id cursor paging with 400 on bad ids, pass chunk_size directly, drop flaky timing asserts, and keep comments/docs concise.\n\n* fix(api): page mempool data and commitment lists independently\n\nReplace the shared after_id bound with an opaque dual-list cursor so truncated pages do not skip or duplicate txs across lists. Document next_cursor semantics and add mixed-type paging coverage.",
          "timestamp": "2026-07-11T12:18:51-07:00",
          "tree_id": "76c9c7b4ef7e8786d323e2129589b108f25093fa",
          "url": "https://github.com/Irys-xyz/irys/commit/81128607c548c5de8195909b9ecf948ca759916c"
        },
        "date": 1783798423489,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015389,
            "range": "± 0.000629",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.156146,
            "range": "± 0.007071",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.57459,
            "range": "± 0.0695",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.470509,
            "range": "± 0.490856",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083053,
            "range": "± 0.000966",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 817.743266,
            "range": "± 28.051606",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 980.608857,
            "range": "± 15.691822",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.120805,
            "range": "± 0.0016",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1221.561576,
            "range": "± 76.262878",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1569.459047,
            "range": "± 10.943496",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.03425,
            "range": "± 0.001746",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.920506,
            "range": "± 1.182325",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.800883,
            "range": "± 0.928796",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000119,
            "range": "± 0.000003",
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
          "id": "ab0d9f541e022e6a7279c1d2f4cfc0354f666067",
          "message": "fix(consensus): pin shadow-tx priority fees to generator-expected tips (#1478)\n\n* fix(consensus): pin shadow-tx priority fees to generator-expected tips\n\nValidate each shadow transaction's EIP-1559 max_priority_fee_per_gas\nagainst ShadowMetadata.transaction_fee during shadow_transactions_are_valid.\n\nEL skims that raw-wei tip from the packet fee-payer to the miner before\npacket execution; CL previously only compared borsh packets, so a producer\ncould keep a valid packet and inflate the envelope tip to drain users.\nExpected metadata now retains the fee; match rejects tip inflation\n(including nonzero tips where zero is expected). Soft-fork-style check —\nwire format unchanged.\n\n* test(consensus): cover legacy shadow-tx envelope tip normalization\n\nAdd a unit test that builds a real TxLegacy shadow envelope (where\nmax_priority_fee_per_gas is None), asserts the same unwrap_or(0)\nnormalization used in shadow extraction, and checks validation accepts\nexpected fee 0 while rejecting a nonzero expected tip.",
          "timestamp": "2026-07-11T16:13:48-07:00",
          "tree_id": "f60d0d37cb7b29def72b695ef28fe4fe83989260",
          "url": "https://github.com/Irys-xyz/irys/commit/ab0d9f541e022e6a7279c1d2f4cfc0354f666067"
        },
        "date": 1783812523858,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015453,
            "range": "± 0.001509",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.151455,
            "range": "± 0.005395",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.562144,
            "range": "± 0.12409",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.830469,
            "range": "± 2.092108",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083689,
            "range": "± 0.009459",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 833.82827,
            "range": "± 22.581332",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1023.194062,
            "range": "± 13.626421",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.126745,
            "range": "± 0.00766",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1235.181203,
            "range": "± 55.540643",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1567.396774,
            "range": "± 150.026712",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035084,
            "range": "± 0.000601",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 208.97287,
            "range": "± 1.158647",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.371711,
            "range": "± 1.401776",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000114,
            "range": "± 0.000002",
            "unit": "ms/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "20095347+JesseTheRobot@users.noreply.github.com",
            "name": "Jesse",
            "username": "JesseTheRobot"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "37e3299a440abb28f769bc57c558f68622699d1a",
          "message": "fix(consensus): only fully-written ledger slots may expire post-Cascade (#1477)\n\n* fix(consensus): only fully-written ledger slots may expire post-Cascade\n\nLedger chunk offsets are strictly cumulative and headroom slots are\npreallocated above the write frontier, so a partially-written frontier\nslot could expire during a full-term ingress stall (it is written, aged,\nand no longer the protected last slot once headroom exists). Any tx\nappended into the expired slot's remainder afterwards was charged but\npermanently non-promotable (the inclusive all-expired promotion filter\nnever re-admits the slot), never settled or refunded (the newly-expiring\nsettlement set fires exactly once per slot, before the tx existed), and\nits chunks never received partition assignments (slot needs skip expired\nslots) — stranding its term and perm fees and leaving the data unstored.\n\nPost-Cascade, slot-expiry eligibility now additionally requires the slot\nto be fully written: its entire chunk range at or below the ledger's\ncumulative total at the epoch block. This extends the frontier\nprotection the never-expire-the-last-slot rule was a proxy for to the\nslot the write head actually sits in, making \"no write ever lands in an\nexpired slot\" structural, and strengthens the contiguous-prefix\ninvariant the promotion filter's [0, range_end) assumption relies on.\nThe gate shares the cascade_active flag with the has_been_written\nfilter, so pre-Cascade replay stays bit-identical; chunks_per_slot == 0\nfails safe (nothing counts as fully written).\n\nThe recycle set, the fee-settlement set, and the promotion-blocked set\nall receive the same write-frontier inputs (per-ledger cumulative totals\nplus chunks-per-slot), so they cannot diverge. The trade-off is bounded\nand mirrors the last slot's existing semantics: a slot that never fills\nnever expires — its data stays live, promotable, and stored, and\nsettlement waits for the slot to fill.\n\nTwo chain-tests that pinned partial-slot recycling are re-pinned to the\nnew semantics: the partially-written frontier slot survives its stale\nwindow, a refill refreshes its expiry clock, and it recycles (settling\nits deferred refunds) a full term after the write that fills it.\n\n* fix: multiversion test flake\n\n* fix(consensus): keep pre-Cascade-expired slots in the inclusive expiry set\n\nAn already-recycled slot must stay in the inclusive non-promotability set\nregardless of the post-Cascade fully-written gate. Without this, a slot that\nexpired and was perm-fee refunded pre-Cascade while only partially written\nwould drop out of the set the moment Cascade activates (its write frontier\nstill sits inside it, so it is not fully written), letting its already-refunded\ntxs be promoted again — a refund plus permanent-storage double-pay. is_expired\nis only ever set by the expiry path and pre-Cascade expiry is prefix-ordered,\nso the set stays a contiguous prefix; a hypothetical non-prefix state trips the\nfail-loud contiguity guard rather than silently under-approximating.\n\nReview-driven cleanups (no behavior change):\n- extract the shared post-Cascade expiry gate into cascade_expiry_gate,\n  applied across all three expiry scans\n- correct the promotion-delay comment: a slot completed mid-epoch can be held\n  non-promotable until the next epoch, not just one block\n- hoist duplicated test scaffolding to module scope in the ledger_expiry tests\n- trim three restated comments",
          "timestamp": "2026-07-13T19:26:37+01:00",
          "tree_id": "94aaae204c3b3c3a6eb61c52a23c214a4c076042",
          "url": "https://github.com/Irys-xyz/irys/commit/37e3299a440abb28f769bc57c558f68622699d1a"
        },
        "date": 1783968560799,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012591,
            "range": "± 0.000469",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.119512,
            "range": "± 0.001376",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.247424,
            "range": "± 0.044636",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.338925,
            "range": "± 0.410935",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.077833,
            "range": "± 0.001605",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 767.632658,
            "range": "± 27.847261",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 969.508818,
            "range": "± 3.364004",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.125963,
            "range": "± 0.002754",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1223.703878,
            "range": "± 5.246177",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1636.118916,
            "range": "± 98.977588",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.036201,
            "range": "± 0.003244",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 216.347708,
            "range": "± 3.512588",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 277.140028,
            "range": "± 2.572495",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000117,
            "range": "± 0.000002",
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
          "id": "e121de8b4b7fa1707d128806076d05f578201948",
          "message": "fix(api): fix storage Path extraction (drop rename_all on path params) (#1482)\n\n* fix(api): match storage route placeholders to camelCase path params\n\n* fix(api): drop rename_all from storage path params, fix external test parsing\n\n* test(api): lock storage response camelCase JSON keys\n\nAssert slotIndex/chunkType/dataChunks/packedChunks wire names so the\npath-param snake_case fix cannot silently break the response contract.",
          "timestamp": "2026-07-13T20:47:01-07:00",
          "tree_id": "90477dd80e15a7af38290329014b2fee1fdbcd04",
          "url": "https://github.com/Irys-xyz/irys/commit/e121de8b4b7fa1707d128806076d05f578201948"
        },
        "date": 1784001595693,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015836,
            "range": "± 0.000548",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.162886,
            "range": "± 0.024869",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.640736,
            "range": "± 0.042976",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.995507,
            "range": "± 1.299183",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.083528,
            "range": "± 0.002241",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 830.614455,
            "range": "± 22.740414",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 1021.472396,
            "range": "± 24.283116",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.148716,
            "range": "± 0.002371",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1260.615691,
            "range": "± 68.64174",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1942.701034,
            "range": "± 20.642939",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.144773,
            "range": "± 0.04022",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 223.263883,
            "range": "± 17.148306",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 283.368957,
            "range": "± 9.113794",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.00015,
            "range": "± 0.000001",
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
          "id": "ec44983b05ad78ce98d4b90aec45d1ef402058a9",
          "message": "refactor(vdf): centralise VDF ownership and heal deep reorgs in place (#1474)\n\n* revert(vdf): remove supervisor-loop re-anchor superseded by in-place heal\n\n* refactor(vdf): centralise ownership and re-anchor seed buffer on reorg\n\n* test(chain-tests): clamp mining helper to the latest available VDF step\n\n* refactor: address review comments\n\n* fix(vdf): restore per-step canonical reset seed lookup\n\nReintroduce BlockProvider::canonical_vdf_snapshot(step) and\nCanonicalVdfSnapshot.reset_seed_for_step so the VDF loop folds the\nreset seed pinned by the block ending at or before the current step,\nnot always the tip's next_seed.\n\nWithout this, a node catching up while behind the tip across a reset\nwindow can apply the wrong boundary seed and diverge the local lineage.\nRestores BlockTree::canonical_entry_at_or_below_step and the regression\ntests that guard tip-vs-step seed selection.\n\n* fix(vdf): pause free-run while buffer suspect; harden re-anchor queue\n\nKeep mark-before-queue for buffer_suspect (required: heal clears the\nflag, so a post-send mark can stick forever). Document that ordering,\nreturn whether send_reanchor_request published, and warn when a queue\nfails after marking so tip-advance retries remain the recovery path.\n\nAlso pause free-running VDF steps while the buffer is suspect so the\nnode does not mine on minority seeds or advance live_step further past\napply_reanchor's second-boundary skip. Validated peer FF still applies.\n\n* fix(vdf): use Acquire/Release on re-anchor generation and suspect\n\nPublish heals and suspect marks with Release/AcqRel, and load them with\nAcquire, so validators that observe a new generation also observe the\nbuffer rewrite that preceded record_heal_applied (and cannot re-poison\nvia generation-stamped FF of pre-heal seeds on weak memory models).\n\n* fix(vdf): metricize re-anchor heal/skip and buffer-suspect state\n\nExport irys.vdf.buffer_suspect, reanchor_healed_total, and\nreanchor_skipped_total so operators can alert on long-lived poison\nwindows and skipped heals (e.g. second-boundary skip while tip lags).\nWarn when a re-anchor is skipped so the stuck-suspect path is visible\nin logs without solely relying on metrics.\n\n* fix(vdf): keep validation waits alive across a re-anchor heal\n\napply_reanchor freezes the step counter while it recomputes the seed\nbuffer tail (up to ~2*reset_frequency steps at ~1s/step). The plain\nwait_for_step treated that as a dead VDF thread and returned Stalled,\nwhich the validation task escalates to the never-mislabel panic — so a\nnode could self-abort during the exact deep-reorg recovery the re-anchor\nexists to perform.\n\nMake the step wait heal-aware: while the buffer is suspect, hold the\nprogress deadline open instead of firing Stalled. The dead-thread guard\nstill fires whenever the buffer is not suspect. Stage A now uses the new\nwait_for_step_or_suspect (liveness only; its invariant is proven\nblock-rootedly in prevalidate_block) and the generation-aware Stage F\nwait gains the same suppression.\n\n* fix(consensus): reject VDF step count that would underflow first_step_number\n\nfirst_step_number() computes global_step_number - steps.len() + 1 with\nraw subtraction, and the release profile enables overflow-checks, so a\npeer can self-sign a gossip block declaring more steps than its global\nstep number and panic the block-discovery task (remote DoS). Guard the\nstep count in prevalidate_block before the subtraction runs; honest\nblocks never exceed global_step_number + 1 steps.\n\n* test(chain-tests): repair the cross-boundary re-anchor e2e spec\n\nThe ignored heavy_vdf_reanchor_after_boundary_crossing_reorg deadlocked\nin setup: the confirmation gate operates in VDF steps, but the test set\nreset_frequency=6 and treated fork_len as a block count, so the loop\nparked before the first block could confirm.\n\nDrive the fork toward a VDF-step target with reset_frequency=240\n(headroom over steps-per-block), crossing just past the first divergent\nboundary so the confirmation lag stays under the reset window. Add an\nassertion that the re-anchor generation advanced, so a green run proves\nthe heal actually fired rather than passing trivially.\n\nStill #[ignore]d: the full-stack timing is fragile under load (the\nmechanism is covered by the deterministic apply_reanchor / gate /\ngeneration / wait tests), so this stays an opt-in executable spec.\n\n* fix: address review findings\n\n* fix: address review findings on the VDF reset-seed gate\n\n* fix: address review findings\n\n* fix: rephrase AB-BA comments to satisfy typos check\n\nCI's typos-cli flags \"BA\" in lock-order jargon; use \"lock-order inversion\" instead.\n\n* docs: collapse VDF re-anchor comment essays\n\nTrim multi-paragraph safety prose to short why-notes and design-doc links.\nNo behavior change (~700 LOC net reduction).\n\n* fix(workflow): add heavy-nightly\n\n* fix(workflow): remove target caching from workflow\n\n* test(vdf): un-ignore the re-anchor e2e and de-flake its fork phases\n\nPromote spiky_slow_heavy_vdf_reanchor_after_boundary_crossing_reorg into\nthe PR lane (spiky_ caps concurrent CPU-spiking tests, slow_ keeps the\n180s kill budget) by removing the wedge/flake sources:\n\n- Freeze the peer's VDF between the shared base and its own fork phase:\n  free-running ahead of its frozen chain makes the first fork block span\n  a full reset window, permanently wedging the confirmation gate. A\n  fail-fast assert pins the freeze against harness waits auto-restarting\n  the VDF.\n- Free-run genesis's VDF during its fork phase instead of crawling a\n  step or two per mine_block call.\n- Extend the peer fork until its penultimate block passes the second\n  divergent boundary's rotation point, so the post-reorg confirmed step\n  un-gates genesis's mining for the closing block.\n- Pin genesis difficulty to zero (0.5 packed partitions) so mining at a\n  gate-parked step always succeeds; fork choice is unaffected since\n  per-block work stays uniform.\n- Poll the asynchronous heal-generation bump instead of asserting the\n  instant heights converge.\n- Start only the peer's VDF (not partition mining) for its fork loop, so\n  zero-difficulty autonomous solutions can't slip in unrecorded blocks.\n\n* docs(actors): drop stale review-label cross-references from recall-range tests\n\n---------\n\nCo-authored-by: dmac <samuraidan@gmail.com>\nCo-authored-by: JesseTheRobot <jesse.cruz.wright@gmail.com>",
          "timestamp": "2026-07-14T09:42:13+01:00",
          "tree_id": "6dfec2c216cfa454069eb2a0b62799c7f5303896",
          "url": "https://github.com/Irys-xyz/irys/commit/ec44983b05ad78ce98d4b90aec45d1ef402058a9"
        },
        "date": 1784019704162,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.012473,
            "range": "± 0.000255",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.125912,
            "range": "± 0.00308",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.277263,
            "range": "± 0.041652",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.336602,
            "range": "± 0.245542",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078119,
            "range": "± 0.001436",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 766.454845,
            "range": "± 28.051798",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 971.177078,
            "range": "± 12.662709",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.117285,
            "range": "± 0.000072",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1190.988184,
            "range": "± 10.754694",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1549.262265,
            "range": "± 12.952971",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034361,
            "range": "± 0.00168",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.225105,
            "range": "± 1.082372",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 271.139076,
            "range": "± 1.837752",
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
          "id": "aefe6f2179fb4deb89383a0f0f371bafbafa02c4",
          "message": "fix: clip orphaned chunk ranges to storage module bounds in deep-reorg recovery (#1476)\n\n* fix(consensus): clip orphaned chunk ranges to storage module bounds\n\n* style(consensus): trim review-flagged comments in recovery fix\n\n* fix: address review findings\n\n* docs: clarify method comment\n\n---------\n\nCo-authored-by: DMac <samuraidan@gmail.com>",
          "timestamp": "2026-07-14T10:42:47+01:00",
          "tree_id": "251a84c81ab939523f675ae9140ce1a003812e0a",
          "url": "https://github.com/Irys-xyz/irys/commit/aefe6f2179fb4deb89383a0f0f371bafbafa02c4"
        },
        "date": 1784023106773,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.016115,
            "range": "± 0.001819",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.157842,
            "range": "± 0.003699",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.62116,
            "range": "± 0.088373",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 10.794596,
            "range": "± 0.696782",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.078541,
            "range": "± 0.001488",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 775.84745,
            "range": "± 16.353676",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 980.169141,
            "range": "± 7.124949",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.123659,
            "range": "± 0.002056",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1203.804938,
            "range": "± 91.44792",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1533.086408,
            "range": "± 10.170632",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.034426,
            "range": "± 0.001171",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 210.41312,
            "range": "± 1.316681",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 273.506897,
            "range": "± 0.833385",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000112,
            "range": "± 0.000003",
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
          "id": "d45c6721b3fbe4473faeac8a9365ed1c5845c3d5",
          "message": "[P0] Ingress-proof-count panic — steady-state remote node-crash DoS (0c / FC-core-B)",
          "timestamp": "2026-07-14T09:42:52Z",
          "url": "https://github.com/Irys-xyz/irys/pull/1514/commits/d45c6721b3fbe4473faeac8a9365ed1c5845c3d5"
        },
        "date": 1784033542597,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "get_recall_range/100",
            "value": 0.015228,
            "range": "± 0.000414",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/1000",
            "value": 0.153496,
            "range": "± 0.003386",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/10000",
            "value": 1.696381,
            "range": "± 0.069224",
            "unit": "ms/iter"
          },
          {
            "name": "get_recall_range/64840",
            "value": 8.337778,
            "range": "± 0.774598",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testing",
            "value": 0.075661,
            "range": "± 0.001588",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/testnet",
            "value": 757.12974,
            "range": "± 6.840645",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha/mainnet",
            "value": 969.37113,
            "range": "± 2.557986",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testing",
            "value": 0.119917,
            "range": "± 0.000341",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/testnet",
            "value": 1196.721052,
            "range": "± 2.855425",
            "unit": "ms/iter"
          },
          {
            "name": "vdf_sha_verification/mainnet",
            "value": 1556.664633,
            "range": "± 2.80462",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testing",
            "value": 0.035092,
            "range": "± 0.00088",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/testnet",
            "value": 209.271602,
            "range": "± 2.578192",
            "unit": "ms/iter"
          },
          {
            "name": "parallel_verification/mainnet",
            "value": 272.708252,
            "range": "± 1.291413",
            "unit": "ms/iter"
          },
          {
            "name": "apply_reset_seed",
            "value": 0.000113,
            "range": "± 0",
            "unit": "ms/iter"
          }
        ]
      }
    ]
  }
}