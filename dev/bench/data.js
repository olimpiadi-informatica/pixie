window.BENCHMARK_DATA = {
  "lastUpdate": 1795272628947,
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
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "55024474+Virv12@users.noreply.github.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "0f5307699987537b82a730723b953ce120a6510a",
          "message": "Fix crash on out-of-order GPT partitions and unaligned NTFS partitions\n\n- Sort GPT partitions by starting byte offset in parse_gpt so\n  partitions listed out of order in the GPT array are processed in\n  disk order, preventing an assertion crash in parse_disk.\n- Use integer division for NTFS cluster count calculation so cluster\n  ranges do not exceed partition boundaries when a partition's size is\n  not an exact multiple of the cluster size.",
          "timestamp": "2026-09-23T12:16:21+02:00",
          "tree_id": "5e96e7d1552a4c856f45c373646ae15f3c1a684c",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/0f5307699987537b82a730723b953ce120a6510a"
        },
        "date": 1790158652630,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 341615.68,
            "range": "± 3020.78",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2345.36,
            "range": "± 30.5",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 3522755.65,
            "range": "± 222539.44",
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
          "id": "364c6114ffd7fb2be1945e1026ebdb65795570a2",
          "message": "fmt & clippy",
          "timestamp": "2026-09-23T12:32:24+02:00",
          "tree_id": "c087f9c36da22fe4dd602da311c8823274af721d",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/364c6114ffd7fb2be1945e1026ebdb65795570a2"
        },
        "date": 1790159645707,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 344228.26,
            "range": "± 3449.87",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2390.89,
            "range": "± 15.17",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 3508314.55,
            "range": "± 197206.91",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "distinct": true,
          "id": "fd97685af8de717c2cb7e4c7579e3c111e96a94a",
          "message": "Run tests without requiring root",
          "timestamp": "2026-11-07T11:02:41+01:00",
          "tree_id": "7326c1f185032cd71ccdc1d986b35ff898ab9641",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/fd97685af8de717c2cb7e4c7579e3c111e96a94a"
        },
        "date": 1794045838446,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 230055.52,
            "range": "± 4060.45",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 1851.3,
            "range": "± 37.24",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 5393430.5,
            "range": "± 100993.8",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "distinct": true,
          "id": "169b65f416a54103d737f4695a303612e778192a",
          "message": "Overhaul pixie-web with Leptos 0.8, DaisyUI, and fine-grained reactivity",
          "timestamp": "2026-11-08T00:08:55+01:00",
          "tree_id": "1ba342567bb211b6e2a8a774302e01e05321d528",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/169b65f416a54103d737f4695a303612e778192a"
        },
        "date": 1794093016512,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_200kib",
            "value": 272766.08,
            "range": "± 5575.6",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_20b",
            "value": 2276.2,
            "range": "± 49.69",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec::benches::bench_flash_roundtrip_4mib",
            "value": 6546876.65,
            "range": "± 690672.98",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "distinct": true,
          "id": "4359ca6867cf1618caf8d1daac2154b8db1cbdfc",
          "message": "Move chunk_codec benchmarks to bench_codec using criterion and rand",
          "timestamp": "2026-11-10T18:17:06+01:00",
          "tree_id": "37ef6ae0db26f94794c3dcb874422caf225b6a65",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/4359ca6867cf1618caf8d1daac2154b8db1cbdfc"
        },
        "date": 1794331153577,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec/roundtrip_20b",
            "value": 52,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/roundtrip_200kib",
            "value": 45805,
            "range": "± 1562",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/roundtrip_4mib",
            "value": 3107017,
            "range": "± 70163",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/encode_4mib",
            "value": 489484,
            "range": "± 8585",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_0_drops",
            "value": 112729,
            "range": "± 22872",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_1_drop",
            "value": 180246,
            "range": "± 3397",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_4_drops_stride32",
            "value": 490073,
            "range": "± 21643",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_16_drops_stride32",
            "value": 493320,
            "range": "± 17602",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_16_drops_burst",
            "value": 493246,
            "range": "± 7623",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_32_drops_burst",
            "value": 506111,
            "range": "± 19435",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_32_drops_stride32",
            "value": 491830,
            "range": "± 33518",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "55024474+Virv12@users.noreply.github.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "57965f663e7d92f9ec89b9f47f3b2c58beac64b2",
          "message": "Fix and test ProxyDHCP on the second interface",
          "timestamp": "2026-11-20T14:18:42+01:00",
          "tree_id": "0f29df596369c9f281ab097d7b8214b897c02412",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/57965f663e7d92f9ec89b9f47f3b2c58beac64b2"
        },
        "date": 1795180896686,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec/roundtrip_20b",
            "value": 69,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/roundtrip_200kib",
            "value": 64284,
            "range": "± 2345",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/roundtrip_4mib",
            "value": 2800484,
            "range": "± 5682",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/encode_4mib",
            "value": 922369,
            "range": "± 4759",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_0_drops",
            "value": 431079,
            "range": "± 1680",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_1_drop",
            "value": 520797,
            "range": "± 1040",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_4_drops_stride32",
            "value": 928656,
            "range": "± 3190",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_16_drops_stride32",
            "value": 934080,
            "range": "± 20431",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_16_drops_burst",
            "value": 934546,
            "range": "± 1890",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_32_drops_burst",
            "value": 948489,
            "range": "± 4933",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_32_drops_stride32",
            "value": 948911,
            "range": "± 3008",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "49699333+dependabot[bot]@users.noreply.github.com",
            "name": "dependabot[bot]",
            "username": "dependabot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "ec93eba7c31538cfc04a4c824cce313f40a032ac",
          "message": "Bump axum from 0.7.9 to 0.8.9 in /pixie-server (#115)\n\n* Bump axum from 0.7.9 to 0.8.9 in /pixie-server\n\nBumps [axum](https://github.com/tokio-rs/axum) from 0.7.9 to 0.8.9.\n- [Release notes](https://github.com/tokio-rs/axum/releases)\n- [Changelog](https://github.com/tokio-rs/axum/blob/main/CHANGELOG.md)\n- [Commits](https://github.com/tokio-rs/axum/compare/axum-v0.7.9...axum-v0.8.9)\n\n---\nupdated-dependencies:\n- dependency-name: axum\n  dependency-version: 0.8.8\n  dependency-type: direct:production\n  update-type: version-update:semver-minor\n...\n\nSigned-off-by: dependabot[bot] <support@github.com>\n\n* Update axum route syntax to {param} for axum 0.8\n\n* Use fallback_service instead of nest_service(\"/\") for axum 0.8\n\n---------\n\nSigned-off-by: dependabot[bot] <support@github.com>\nCo-authored-by: dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>\nCo-authored-by: Filippo Casarin <casarin.filippo17@gmail.com>",
          "timestamp": "2026-11-21T15:48:35+01:00",
          "tree_id": "46f667f9d8b5ee4669cc9ab53b4cb59c0ca9306d",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/ec93eba7c31538cfc04a4c824cce313f40a032ac"
        },
        "date": 1795272628641,
        "tool": "cargo",
        "benches": [
          {
            "name": "chunk_codec/roundtrip_20b",
            "value": 79,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/roundtrip_200kib",
            "value": 68353,
            "range": "± 459",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/roundtrip_4mib",
            "value": 4060267,
            "range": "± 100330",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/encode_4mib",
            "value": 685761,
            "range": "± 14962",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_0_drops",
            "value": 156850,
            "range": "± 4649",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_1_drop",
            "value": 247304,
            "range": "± 6124",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_4_drops_stride32",
            "value": 700497,
            "range": "± 27349",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_16_drops_stride32",
            "value": 696548,
            "range": "± 9563",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_16_drops_burst",
            "value": 693217,
            "range": "± 6475",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_32_drops_burst",
            "value": 711999,
            "range": "± 10186",
            "unit": "ns/iter"
          },
          {
            "name": "chunk_codec/decode_32_drops_stride32",
            "value": 712356,
            "range": "± 9457",
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
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "55024474+Virv12@users.noreply.github.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "0f5307699987537b82a730723b953ce120a6510a",
          "message": "Fix crash on out-of-order GPT partitions and unaligned NTFS partitions\n\n- Sort GPT partitions by starting byte offset in parse_gpt so\n  partitions listed out of order in the GPT array are processed in\n  disk order, preventing an assertion crash in parse_disk.\n- Use integer division for NTFS cluster count calculation so cluster\n  ranges do not exceed partition boundaries when a partition's size is\n  not an exact multiple of the cluster size.",
          "timestamp": "2026-09-23T12:16:21+02:00",
          "tree_id": "5e96e7d1552a4c856f45c373646ae15f3c1a684c",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/0f5307699987537b82a730723b953ce120a6510a"
        },
        "date": 1790158747316,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 21.174,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 16.737,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.252,
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
          "id": "364c6114ffd7fb2be1945e1026ebdb65795570a2",
          "message": "fmt & clippy",
          "timestamp": "2026-09-23T12:32:24+02:00",
          "tree_id": "c087f9c36da22fe4dd602da311c8823274af721d",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/364c6114ffd7fb2be1945e1026ebdb65795570a2"
        },
        "date": 1790159747371,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 21.383,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 17.435,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.208,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "distinct": true,
          "id": "fd97685af8de717c2cb7e4c7579e3c111e96a94a",
          "message": "Run tests without requiring root",
          "timestamp": "2026-11-07T11:02:41+01:00",
          "tree_id": "7326c1f185032cd71ccdc1d986b35ff898ab9641",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/fd97685af8de717c2cb7e4c7579e3c111e96a94a"
        },
        "date": 1794046031369,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 19.431,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 15.955,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 12.478,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "distinct": true,
          "id": "169b65f416a54103d737f4695a303612e778192a",
          "message": "Overhaul pixie-web with Leptos 0.8, DaisyUI, and fine-grained reactivity",
          "timestamp": "2026-11-08T00:08:55+01:00",
          "tree_id": "1ba342567bb211b6e2a8a774302e01e05321d528",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/169b65f416a54103d737f4695a303612e778192a"
        },
        "date": 1794093175624,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 21.162,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 15.766,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.249,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "distinct": true,
          "id": "4359ca6867cf1618caf8d1daac2154b8db1cbdfc",
          "message": "Move chunk_codec benchmarks to bench_codec using criterion and rand",
          "timestamp": "2026-11-10T18:17:06+01:00",
          "tree_id": "37ef6ae0db26f94794c3dcb874422caf225b6a65",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/4359ca6867cf1618caf8d1daac2154b8db1cbdfc"
        },
        "date": 1794331268815,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 18.304,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 14.905,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 10.413,
            "unit": "s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "veluca93@gmail.com",
            "name": "Luca Versari",
            "username": "veluca93"
          },
          "committer": {
            "email": "55024474+Virv12@users.noreply.github.com",
            "name": "Filippo Casarin",
            "username": "Virv12"
          },
          "distinct": true,
          "id": "57965f663e7d92f9ec89b9f47f3b2c58beac64b2",
          "message": "Fix and test ProxyDHCP on the second interface",
          "timestamp": "2026-11-20T14:18:42+01:00",
          "tree_id": "0f29df596369c9f281ab097d7b8214b897c02412",
          "url": "https://github.com/olimpiadi-informatica/pixie/commit/57965f663e7d92f9ec89b9f47f3b2c58beac64b2"
        },
        "date": 1795181098918,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "store (qemu e2e)",
            "value": 24.204,
            "unit": "s"
          },
          {
            "name": "flash, cold (qemu e2e)",
            "value": 17.892,
            "unit": "s"
          },
          {
            "name": "flash, cached (qemu e2e)",
            "value": 11.908,
            "unit": "s"
          }
        ]
      }
    ]
  }
}