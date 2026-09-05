# Example: Dekopon's verified narrow memory pilot

Repository: `dekopon-agents/dekopon`, study `studies/memory/`.
[Proof report](https://github.com/dekopon-agents/dekopon/blob/research/memory-study/studies/memory/E2E_PROOF.md)
and [findings](https://github.com/dekopon-agents/dekopon/blob/research/memory-study/studies/memory/FINDINGS.md).

On 2026-09-05, **after** independently accepted infrastructure gates, the project first ran
one real release broker with echo through startup, 60-second idle, shutdown, parsing, analysis,
sanitized export/analysis-only restore and exact owned-container/volume cleanup:

- Campaign `approved-v4-pilot-01-proof`, experiment `RT-X02`, replicate1.
- Attempt `6233f5550d8643e796e3575a84d89040`.
- Parse `c43bac2278a844398e2c395ea5631b20`.
- Raw artifact `2a429c42810b4efa8e6eb35125c9d784`, SHA-256
  `8d9cf67ac90eea8ac703255d50cf7772a84b183c283174750809abbeb66956ac`.
- 61 idle RSS observations: mean26,340,368.786885247B, sampled maximum26,361,856B.
- Exit0, PID0, identity-checked deletion and both slots free; no repair/retry.

Only after that proof did the approved nine-trial screen run, sequentially: three P4, three
echo and three real source-History-kernel trials. P4's larger observed RSS did not identify
allocator costs. History logical text reached zero after drop while RSS stayed high; that
was not a leak diagnosis or a whole-gateway result. Linux4KiB pages were not Pi16KiB parity.

The transferable lesson is the **ordering of gates and bounded evidence chain**. This project
used a private canonical SQLite ledger, compatible retained binaries and closed Docker recipes.
Its commands, schema and containment assumptions are not a universal backend. No production
access, new framework, nested agent or full Cargo build was needed for the proof/screen phase.
