"""X2 verifier: reward 1 when the session survived, the answer carries the expected facts, and the scripts
show the expected behaviour (calls made, calls avoided, order, counts)."""
import json
import sys
from pathlib import Path


def judge(t, expected):
    answer = (t.get('answer') or '').lower()
    scripts = [s['script'] for s in t['scripts']]
    joined = '\n'.join(scripts)
    lowered = joined.lower()
    problems = []
    if t.get('fatal') is not None:
        problems.append(f"fatal:{t['fatal']}")
    problems += [f'answer-missing:{f}' for f in expected.get('all_of', []) if f.lower() not in answer]
    problems += [f'answer-any-of-missing:{"|".join(group)}' for group in expected.get('any_of', [])
                 if not any(f.lower() in answer for f in group)]
    problems += [f'script-missing:{f}' for f in expected.get('script_all_of', []) if f.lower() not in lowered]
    problems += [f'script-forbidden:{f}' for f in expected.get('script_none_of', []) if f.lower() in lowered]
    for needle, most in expected.get('script_count_max', {}).items():
        if lowered.count(needle.lower()) > most:
            problems.append(f'script-count:{needle}>{most}')
    outputs = [s['output_head'] for s in t['scripts']]
    for needle, (least, most) in expected.get('output_count', {}).items():
        seen = sum(o.count(needle) for o in outputs)
        if not least <= seen <= most:
            problems.append(f'output-count:{needle}={seen}')
    for script_needle, output_needle in expected.get('read_before', []):
        read = next((i for i, s in enumerate(scripts) if script_needle.lower() in s.lower()), None)
        wrote = next((i for i, o in enumerate(outputs) if output_needle in o), None)
        if wrote is not None and (read is None or read > wrote):
            problems.append(f'order:{script_needle}<{output_needle}')
    if expected.get('require_clean', False) and not any(s['exit_code'] == 0 for s in t['scripts']):
        problems.append('no-clean-script')
    if expected.get('max_scripts') is not None and len(scripts) > expected['max_scripts']:
        problems.append(f'scripts>{expected["max_scripts"]}')
    return problems


if __name__ == '__main__':
    transcript_path, expected_path, out_dir = (Path(p) for p in sys.argv[1:4])
    out_dir.mkdir(parents=True, exist_ok=True)
    reward, detail = 0, {}
    try:
        t = json.loads(transcript_path.read_text())
        problems = judge(t, json.loads(expected_path.read_text()))
        reward = 0 if problems else 1
        detail = {'fatal': t.get('fatal'), 'problems': problems, 'missing_facts': problems,
                  'scripts': len(t['scripts']), 'clean_scripts': sum(1 for s in t['scripts'] if s['exit_code'] == 0),
                  'exit2': sum(1 for s in t['scripts'] if s['exit_code'] == 2), 'bad_calls': None,
                  'turns': t['model_turns'], 'usage': t['usage'], 'seconds': t['seconds']}
    except Exception as error:  # a broken transcript is a 0 with the reason recorded
        detail = {'error': repr(error)}
    (out_dir / 'reward.txt').write_text(f'{reward}\n')
    (out_dir / 'detail.json').write_text(json.dumps(detail, indent=2) + '\n')
    print(json.dumps(detail))
