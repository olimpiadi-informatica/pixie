window.BENCHMARK_DATA = {
  "lastUpdate": 1788910343539,
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
      }
    ]
  }
}