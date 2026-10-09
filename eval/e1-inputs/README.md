# E1 eval task inputs (staging)

Thirteen task definitions for the eval at the tag, staged here until E1 ports them into
`crates/dekopon-agent/examples/eval/`. Each `tasks/<task>/` holds the user turn (`instruction.md`) and
what the verifier looks for (`expected.json`). `score.py` judges the basic-world tasks; `x2-score.py`
judges the `sci-*` tasks, which also check scripts, output counts and call order. Both take
`<transcript.json> <expected.json> <out_dir>` and write `reward.txt` and `detail.json`.

Every org, repo and person name is synthetic (`orchard-hq/ledger`, `tangelo-oss/tangelo`, `tess-orchard`);
the fixture world the agent runs against must use the same names. The private instruction texts the
`sci-*` tasks were measured with are not here: they reach `--system` at run time from a pinned private
repository.
