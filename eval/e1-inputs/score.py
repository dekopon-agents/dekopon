"""Verifier: reward 1 when the session survived, a script ran clean, and the answer carries the expected facts."""
import json
import sys
from pathlib import Path

transcript_path, expected_path, out_dir = (Path(p) for p in sys.argv[1:4])
out_dir.mkdir(parents=True, exist_ok=True)
reward = 0
detail = {}
try:
    t = json.loads(transcript_path.read_text())
    expected = json.loads(expected_path.read_text())
    answer = (t.get('answer') or '').lower()
    facts = [f for f in expected.get('all_of', []) if f.lower() not in answer]
    forbidden = [f for f in expected.get('none_of', []) if f.lower() in answer]
    clean = [s for s in t['scripts'] if s['exit_code'] == 0]
    fed_back = sum(1 for turn in t['turns'] for c in turn['tool_calls']
                   if c['name'] != 'bash' or 'script' not in c['argument_keys'])
    detail = {
        'fatal': t.get('fatal'), 'missing_facts': facts, 'forbidden_present': forbidden,
        'scripts': len(t['scripts']), 'clean_scripts': len(clean),
        'exit2': sum(1 for s in t['scripts'] if s['exit_code'] == 2),
        'bad_calls': fed_back, 'turns': t['model_turns'], 'usage': t['usage'], 'seconds': t['seconds'],
    }
    max_scripts = expected.get('max_scripts')
    if (t.get('fatal') is None and (clean or expected.get('require_clean', True) is False) and not facts and not forbidden
            and (max_scripts is None or len(t['scripts']) <= max_scripts)):
        reward = 1
except Exception as error:  # a broken transcript is a 0 with the reason recorded
    detail = {'error': repr(error)}
(out_dir / 'reward.txt').write_text(f'{reward}\n')
(out_dir / 'detail.json').write_text(json.dumps(detail, indent=2) + '\n')
print(json.dumps(detail))
