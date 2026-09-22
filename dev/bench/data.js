window.BENCHMARK_DATA = {
  "lastUpdate": 1790093269957,
  "repoUrl": "https://github.com/olimpiadi-informatica/pixie",
  "entries": {
    "Flash codec micro-benchmark (chunk_codec)": [
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "55024474+Virv12@users.noreply.github.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "041ffac60f7ef3f9802da34302b6e9d1965b0f18",
          "message": "Add e2e store/flash benchmarks and track benchmarks in CI\n\nIntroduces a QEMU-driven e2e benchmark for the store/flash path and a\nmicro-benchmark for the chunk_codec flash codec (pixie-shared), both run\nin CI via benchmark-action/github-action-benchmark.\n\nBenchmark history is stored on a 'benchmark-data' branch (created here,\nsince no such branch previously existed): pushes to master commit and\npush results there so future runs have a baseline to compare against\nand can flag regressions; PR runs only compare, they don't write\nhistory. GitHub Pages is explicitly pointed at 'benchmark-data' so the\naction's chart page keeps working (its default 'gh-pages' branch name\nis reserved for auto-activating Pages, which is misleading for a branch\nwhose real purpose is storing benchmark history). Auto-push is gated on\n`github.ref == 'refs/heads/master'` rather than the push event name, so\nit can't silently start firing for other branches if the workflow's\n`on:` trigger is ever broadened. The second benchmark step in the job\nskips re-fetching the data branch, since the first step already leaves\nit fetched and advanced locally and re-fetching would conflict with it.\n\nCo-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>\nClaude-Session: https://claude.ai/code/session_011WEfV39dSLVtDccUcYmZCt",
          "timestamp": "2026-09-09T01:30:49+02:00",
          "tree_id": "a6110dd145d687209b27c86b2dbe7395a0b8f326",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/041ffac60f7ef3f9802da34302b6e9d1965b0f18"
        },
        "date": 1788910342234,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 341502.05,
            "range": "± 4990.91",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2357.17,
            "range": "± 15.14",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 3539645.95,
            "range": "± 368312.55",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "e5815b52e647375feded1e99a09f914db0df02b7",
          "message": "Add UnitStats",
          "timestamp": "2026-09-21T15:35:40+02:00",
          "tree_id": "878855aa90c3b30600613cf97aabcd4662ffc38d",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/e5815b52e647375feded1e99a09f914db0df02b7"
        },
        "date": 1789997912514,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 342139.3,
            "range": "± 9279.01",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2364.8,
            "range": "± 22.34",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 3515813.3,
            "range": "± 199452.5",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "b6e25980c7f8392dbec4990a46f84eb2f0fb99d8",
          "message": "Open disk with safe api",
          "timestamp": "2026-09-21T16:56:30+02:00",
          "tree_id": "0d5c2821d166df2f911d894ae45c06ccee97bbc0",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/b6e25980c7f8392dbec4990a46f84eb2f0fb99d8"
        },
        "date": 1790002678705,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 341844.29,
            "range": "± 3822.14",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2347.48,
            "range": "± 48.62",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 3615633.65,
            "range": "± 239043.84",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "b7e5ab6e496b1693cdbece1b77128a7d7da29a27",
          "message": "cargo fmt",
          "timestamp": "2026-09-21T17:09:05+02:00",
          "tree_id": "65994f66c47f7b64d8b8e09aed8599d35cc867d0",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/b7e5ab6e496b1693cdbece1b77128a7d7da29a27"
        },
        "date": 1790003436617,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 341643.09,
            "range": "± 4670.52",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2343.46,
            "range": "± 21.27",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 3654435.35,
            "range": "± 295584",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "84cbe4bc37cd71db2c551e6e2e83891b1d6b0d6b",
          "message": "Improve gpt parser to be more granular",
          "timestamp": "2026-09-22T18:02:03+02:00",
          "tree_id": "45eb603308108132947a2d207ef2c66e0c2ca7b6",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/84cbe4bc37cd71db2c551e6e2e83891b1d6b0d6b"
        },
        "date": 1790093168036,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 140351.3,
            "range": "± 1637.14",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2607.94,
            "range": "± 408.18",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 1992657.1,
            "range": "± 28785.33",
            "unit": "ns/iter"
          }
        ]
      }
    ],
    "Store/Flash E2E (qemu)": [
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "55024474+Virv12@users.noreply.github.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "041ffac60f7ef3f9802da34302b6e9d1965b0f18",
          "message": "Add e2e store/flash benchmarks and track benchmarks in CI\n\nIntroduces a QEMU-driven e2e benchmark for the store/flash path and a\nmicro-benchmark for the chunk_codec flash codec (pixie-shared), both run\nin CI via benchmark-action/github-action-benchmark.\n\nBenchmark history is stored on a 'benchmark-data' branch (created here,\nsince no such branch previously existed): pushes to master commit and\npush results there so future runs have a baseline to compare against\nand can flag regressions; PR runs only compare, they don't write\nhistory. GitHub Pages is explicitly pointed at 'benchmark-data' so the\naction's chart page keeps working (its default 'gh-pages' branch name\nis reserved for auto-activating Pages, which is misleading for a branch\nwhose real purpose is storing benchmark history). Auto-push is gated on\n`github.ref == 'refs/heads/master'` rather than the push event name, so\nit can't silently start firing for other branches if the workflow's\n`on:` trigger is ever broadened. The second benchmark step in the job\nskips re-fetching the data branch, since the first step already leaves\nit fetched and advanced locally and re-fetching would conflict with it.\n\nCo-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>\nClaude-Session: https://claude.ai/code/session_011WEfV39dSLVtDccUcYmZCt",
          "timestamp": "2026-09-09T01:30:49+02:00",
          "tree_id": "a6110dd145d687209b27c86b2dbe7395a0b8f326",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/041ffac60f7ef3f9802da34302b6e9d1965b0f18"
        },
        "date": 1788910525796,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 21.164,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 16.47,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.309,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "e5815b52e647375feded1e99a09f914db0df02b7",
          "message": "Add UnitStats",
          "timestamp": "2026-09-21T15:35:40+02:00",
          "tree_id": "878855aa90c3b30600613cf97aabcd4662ffc38d",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/e5815b52e647375feded1e99a09f914db0df02b7"
        },
        "date": 1789998097745,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 21.246,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 16.491,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.312,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "b6e25980c7f8392dbec4990a46f84eb2f0fb99d8",
          "message": "Open disk with safe api",
          "timestamp": "2026-09-21T16:56:30+02:00",
          "tree_id": "0d5c2821d166df2f911d894ae45c06ccee97bbc0",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/b6e25980c7f8392dbec4990a46f84eb2f0fb99d8"
        },
        "date": 1790002775561,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 21.432,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 16.166,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.363,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "b7e5ab6e496b1693cdbece1b77128a7d7da29a27",
          "message": "cargo fmt",
          "timestamp": "2026-09-21T17:09:05+02:00",
          "tree_id": "65994f66c47f7b64d8b8e09aed8599d35cc867d0",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/b7e5ab6e496b1693cdbece1b77128a7d7da29a27"
        },
        "date": 1790003541014,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 21.191,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 16.265,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.302,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "committer": {
            "email": "casarin.filippo17@gmail.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "84cbe4bc37cd71db2c551e6e2e83891b1d6b0d6b",
          "message": "Improve gpt parser to be more granular",
          "timestamp": "2026-09-22T18:02:03+02:00",
          "tree_id": "45eb603308108132947a2d207ef2c66e0c2ca7b6",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/84cbe4bc37cd71db2c551e6e2e83891b1d6b0d6b"
        },
        "date": 1790093269947,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 20.609,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 15.657,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.008,
            "unit": "s"
          }
        ]
      }
    ]
  }
}