import json, pathlib, shutil, uuid, datetime, os, sys
home = pathlib.Path(sys.argv[1] if len(sys.argv)>1 else '/tmp/desktop')
if home.exists(): shutil.rmtree(home)
home.mkdir(parents=True)
sessions = home / '.omp/agent/sessions'
base = datetime.datetime(2026, 11, 8, 9, 0, tzinfo=datetime.timezone.utc)
convs = [
 ('webshop', 'Checkout totals off by one cent', [
   ('The cart total is one cent off when a coupon applies. Find out why.', 'Rounding happens per line before the coupon. I moved it to the order total in `cart/total.ts` and added a test with three 0.333 items.'),
   ('Run the tests again.', 'All 214 tests pass. The new case fails on the old code and passes now.')], 25),
 ('webshop', 'Add Stripe webhook retries', [
   ('Webhook deliveries fail when the DB is busy. Make them retry safely.', 'Handlers are now idempotent on the event id, and a failed delivery returns 503 so Stripe retries with backoff.')], 70),
 ('api-gateway', 'Rate limiter per API key', [
   ('Add a token-bucket rate limit per API key, 100 req/min.', 'Done in `middleware/ratelimit.go`, with buckets in Redis and a 429 carrying Retry-After.'),
   ('Can bursts of 20 pass?', 'Yes: capacity is 20 and the refill rate is 100 per minute. I added a burst test.')], 140),
 ('api-gateway', 'Migrate config to TOML', [
   ('Move the YAML config to TOML and keep the old keys working.', 'The loader reads TOML first and falls back to YAML with a deprecation warning. Both are covered by tests.')], 300),
 ('ml-notes', 'Summarise the attention paper', [
   ('Summarise the main idea of the attention paper in five lines.', 'Self-attention lets every token weigh every other token. Multi-head attention runs several of these in parallel. No recurrence is needed, so training parallelises well.')], 900),
 ('dotfiles', 'Fix tmux clipboard on Wayland', [
   ('Copying in tmux does not reach the Wayland clipboard.', 'Set `copy-command` to `wl-copy` and bind `y` in copy mode. Restart the tmux server to apply it.')], 2000),
 ('webshop', 'Refactor product search', [
   ('Product search is slow for long queries. Profile it.', 'Most of the time goes into building trigrams per request. I cached them per product and search went from 420 ms to 35 ms.')], 3500),
]
tmp = os.environ.get('TMPDIR', '/tmp')
for i, (proj, title, turns, ago) in enumerate(convs):
    cwd = home / 'projects' / proj
    cwd.mkdir(parents=True, exist_ok=True)
    sid = str(uuid.UUID(int=0x1000 + i, version=4))
    start = base - datetime.timedelta(minutes=ago)
    ts = lambda m: (start + datetime.timedelta(minutes=m)).strftime('%Y-%m-%dT%H:%M:%S.000Z')
    lines = [{"type": "session", "version": 3, "id": sid, "timestamp": ts(0), "cwd": str(cwd), "title": title}]
    parent = None
    for t, (q, a) in enumerate(turns):
        u, r = f"u{t}-{i}", f"a{t}-{i}"
        lines.append({"type": "message", "id": u, "parentId": parent, "timestamp": ts(2*t+1), "message": {"role": "user", "content": [{"type": "text", "text": q}]}})
        lines.append({"type": "message", "id": r, "parentId": u, "timestamp": ts(2*t+2), "message": {"role": "assistant", "content": [{"type": "text", "text": a}], "stopReason": "stop", "provider": "fake", "model": "fake-32k"}})
        parent = r
    d = sessions / f"-projects-{proj}"
    d.mkdir(parents=True, exist_ok=True)
    f = d / (start.strftime('%Y-%m-%dT%H-%M-%S-000Z') + f"_{sid}.jsonl")
    f.write_text('\n'.join(json.dumps(l) for l in lines) + '\n')
    mt = (start + datetime.timedelta(minutes=2*len(turns))).timestamp()
    os.utime(f, (mt, mt))
for d in ['.pi/agent/sessions', '.claude/projects', '.config/sesync']:
    (home / d).mkdir(parents=True, exist_ok=True)
print(home)
