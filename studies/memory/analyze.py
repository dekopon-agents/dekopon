#!/usr/bin/env python3
"""Read-only bounded report. Infra, failed attempts and missing metrics are not benchmark wins."""
import argparse
import json
from pathlib import Path
import sqlite3

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--campaign')
args=parser.parse_args()
path=Path(__file__).resolve().parent/'private/ledger.sqlite3'
c=sqlite3.connect(path.as_uri()+'?mode=ro',uri=True)
c.row_factory=sqlite3.Row
try:
    params=(args.campaign,) if args.campaign else ()
    where=' AND t.campaign_id=?' if args.campaign else ''
    coverage=[dict(r) for r in c.execute('SELECT * FROM coverage ORDER BY lane,stage,id LIMIT 64')]
    results=[dict(r) for r in c.execute('''SELECT t.campaign_id,t.experiment_id,r.classification,
        s.phase,s.metric_id,m.unit,env.architecture,env.page_bytes,a.driver_sha256,
        count(DISTINCT t.replicate) AS independent_replicates,
        min(s.maximum) AS minimum_trial_peak,max(s.maximum) AS maximum_trial_peak,
        avg(s.mean) AS mean_of_trial_means,
        'Sampled peaks are lower bounds; kernel high-water metrics are separate' AS caveat
        FROM phase_summary s JOIN latest_parse p ON p.id=s.parse_id
        JOIN attempt a ON a.id=p.attempt_id JOIN trial t ON t.id=a.trial_id
        JOIN experiment e ON e.id=t.experiment_id JOIN recipe r ON r.id=e.recipe_id
        JOIN metric m ON m.id=s.metric_id LEFT JOIN environment env ON env.id=a.environment_id
        WHERE a.status='succeeded' AND r.classification!='infra' '''+where+'''
        GROUP BY t.campaign_id,t.experiment_id,s.phase,s.metric_id,env.architecture,env.page_bytes,a.driver_sha256
        ORDER BY t.campaign_id,t.experiment_id,s.phase,s.metric_id LIMIT 500''',params)]
    failures=[dict(r) for r in c.execute('''SELECT a.id,t.campaign_id,t.experiment_id,a.number,a.status,
        er.category,er.retryable FROM attempt a JOIN trial t ON t.id=a.trial_id
        LEFT JOIN error er ON er.id=(SELECT max(id) FROM error WHERE attempt_id=a.id)
        WHERE a.status!='succeeded' '''+where+' ORDER BY a.started_at LIMIT 100',params)]
    print(json.dumps({'coverage':coverage,'successful_benchmark_results':results,'failures':failures,
        'limits':'64 cells,500 metric groups,100 failures; missing is not zero; >=3 replicates required for comparative claims'},sort_keys=True))
finally:c.close()
