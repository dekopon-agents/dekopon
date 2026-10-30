Same command at both trees; only the checkout differs (base 95d9e2e2b679adf9d5f4bc615d7ca744d528c8a9 for `base-full/`, PR head 0d500f78014754e1c5049cb48c10ab6c7951cadf for `head-full/`). Fixtures first: `ci/fetch-external-provider-components.sh examples/providers`.

    RE='<alternation of line:col below>'
    CARGO_PROFILE_DEV_DEBUG=0 cargo mutants -p dekopon-broker \
      --file crates/dekopon-broker/src/lib.rs --re "lib\.rs:($RE):" \
      --in-place --timeout 120 --cargo-arg=-p --cargo-arg=dekopon-broker-host \
      -- --lib --tests

`--re` is matched against `file:line:col: description`; the 147 mutants are every mutation at the line:col pairs of the triage's `kill` items (minus the three hang items 140:48, 142:38, 168:36). Run alone: a concurrent second job made the base baseline time out.
Spot-check one mutant: `--re 'lib\.rs:3059:9:'` (canonical_authority_surface, 3 variants) or `--re 'lib\.rs:4367:5:'` (effect_tag).

line:col pairs: `940:9|1102:9|1106:9|1107:13|1107:31|1108:13|1108:32|1109:13|1124:9|1128:9|1134:9|1146:39|1147:17|1151:17|1187:54|1201:58|1270:9|1361:16|1452:9|1463:26|1469:31|1850:9|2039:17|2040:17|2041:17|2071:9|2073:66|2104:17|2163:33|2172:17|2231:33|2232:33|2309:16|2309:37|2430:21|2572:9|2586:9|2591:9|2599:9|2950:32|3023:39|3042:9|3059:9|3086:13|3094:41|3095:21|3096:21|3125:21|3139:12|3243:37|3262:9|3271:42|3271:46|3284:51|3288:25|3288:43|3288:65|3503:36|3587:51|3802:30|4130:51|4199:34|4367:5|4375:5|4740:9|4770:9|4826:33|4827:78|4936:9|5144:5|5156:5|5173:5|5181:5|5188:5|5189:11|5189:19|5189:27|5407:5|5509:13|5510:13|5511:13|5512:13`
