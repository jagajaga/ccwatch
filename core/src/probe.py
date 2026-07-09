#!/usr/bin/env python3
"""ccwatch zero-install remote probe.

Piped over `ssh <host> python3 -` by ccwatchd, so nothing has to be installed
on the remote machine. Reads the remote's ~/.claude (or argv[1] /
$CLAUDE_CONFIG_DIR) and prints a Snapshot JSON matching ccwatch-core's serde
model exactly. Stdlib only; must stay compatible with python3.6+.
"""
import glob
import json
import os
import subprocess
import sys
import time
from datetime import datetime

ROOT = (
    sys.argv[1]
    if len(sys.argv) > 1
    else os.environ.get("CLAUDE_CONFIG_DIR", os.path.expanduser("~/.claude"))
)
NOW_MS = int(time.time() * 1000)
WINDOW_MS = 5 * 60 * 1000   # rate window, matches core default
IDLE_MS = 120 * 1000        # idle threshold, matches core default
MAX_TAIL = 16 * 1024 * 1024  # cap transcript read for huge histories
BUCKET_MS = 5 * 60 * 1000    # governor usage buckets
HORIZON_MS = 9 * 24 * 3600 * 1000  # ~9 days: covers the weekly window
WORKFLOW_FRESH_MS = 30 * 60 * 1000  # only recent workflow fleets, matches retention

# bucket_ts -> Opus-equivalent (weighted) billable tokens, fed by every scan.
# Kept mix-invariant so the governor's learned ceiling survives Fable↔Opus
# swaps. Weights match ccwatch-core's config defaults (Opus-normalised price
# ratios); a local config override is NOT applied here — remote buckets always
# use the defaults.
USAGE_BUCKETS = {}
# observed 429 timestamps (epoch ms) within the horizon
RATE_LIMITS = []
# parsed limit-hit markers
LIMIT_HITS = []
# raw billable tokens per model tier over the horizon — the account's mix
MODEL_MIX = {}

WEIGHTS = {"opus": 1.0, "sonnet": 0.6, "haiku": 0.2, "fable": 2.0, "other": 1.0}


def tier_of(model):
    """Weight/cost tier for a model id (substring match), else 'other'."""
    if not model:
        return "other"
    m = model.lower()
    if "opus" in m:
        return "opus"
    if "sonnet" in m:
        return "sonnet"
    if "haiku" in m:
        return "haiku"
    if "fable" in m or "mythos" in m:
        return "fable"
    return "other"


def account_buckets(ts, billable, model):
    """Fold one message into the account-wide governor buckets + mix."""
    if ts < NOW_MS - HORIZON_MS:
        return
    bucket = ts - (ts % BUCKET_MS)
    tier = tier_of(model)
    USAGE_BUCKETS[bucket] = USAGE_BUCKETS.get(bucket, 0) + int(
        round(billable * WEIGHTS.get(tier, 1.0))
    )
    MODEL_MIX[tier] = MODEL_MIX.get(tier, 0) + billable


def parse_reset(text, hit_ms):
    """Best-effort '8pm (Europe/Paris)' / 'Jul 1 at 8pm (UTC)' -> epoch ms."""
    try:
        from zoneinfo import ZoneInfo
    except Exception:
        return None
    import re as _re
    m = _re.search(r"\(([^)]+)\)", text)
    if not m:
        return None
    tzname = {"GMT": "UTC", "PST": "America/Los_Angeles", "PDT": "America/Los_Angeles",
              "EST": "America/New_York", "EDT": "America/New_York",
              "CET": "Europe/Paris", "CEST": "Europe/Paris"}.get(m.group(1).strip(), m.group(1).strip())
    try:
        tz = ZoneInfo(tzname)
    except Exception:
        return None
    tm = _re.search(r"(\d{1,2})(?::(\d{2}))?\s*(am|pm)", text.lower())
    if not tm:
        return None
    h = int(tm.group(1)); mi = int(tm.group(2) or 0)
    if tm.group(3) == "pm" and h != 12: h += 12
    if tm.group(3) == "am" and h == 12: h = 0
    from datetime import datetime, timezone
    local = datetime.fromtimestamp(hit_ms / 1000, tz)
    y, mo, d = local.year, local.month, local.day
    months = ["jan","feb","mar","apr","may","jun","jul","aug","sep","oct","nov","dec"]
    dm = _re.search(r"(" + "|".join(months) + r")\w*\s+(\d{1,2})", text.lower())
    if dm:
        mo = months.index(dm.group(1)) + 1; d = int(dm.group(2))
    try:
        return int(datetime(y, mo, d, h, mi, tzinfo=tz).timestamp() * 1000)
    except Exception:
        return None


def note_limit_marker(d, ts):
    """Record 'hit your {weekly|session} limit · resets …' markers."""
    m = d.get("message") or {}
    c = m.get("content")
    text = ""
    if isinstance(c, str):
        text = c
    elif isinstance(c, list):
        text = " ".join(b.get("text", "") for b in c if isinstance(b, dict) and b.get("type") == "text")
    if "hit your " not in text or "not your" in text:
        return
    rest = text.split("hit your ", 1)[1]
    weekly = rest.startswith("weekly")
    if not (weekly or rest.startswith("session")) or not ts:
        return
    reset_text = rest.split("resets", 1)[1].strip() if "resets" in rest else ""
    LIMIT_HITS.append({"weekly": weekly, "at_ms": ts, "reset_ms": parse_reset(reset_text, ts)})
AGENT_TOOLS = ("Agent", "Task", "Workflow")


def zero_ledger():
    return dict(input=0, output=0, cache_write=0, cache_read=0,
                web_search=0, web_fetch=0, messages=0)


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except Exception:
        return False


def parse_ts(ts):
    """ISO-8601 'Z' timestamp -> epoch ms, or None."""
    try:
        return int(
            datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp() * 1000
        )
    except Exception:
        return None


def parse_etime(e):
    """[[dd-]hh:]mm:ss -> seconds."""
    try:
        days = 0
        if "-" in e:
            d, e = e.split("-", 1)
            days = int(d)
        parts = [int(x) for x in e.split(":")]
        while len(parts) < 3:
            parts.insert(0, 0)
        return days * 86400 + parts[0] * 3600 + parts[1] * 60 + parts[2]
    except Exception:
        return 0


def process_table():
    """pid -> (ppid, cpu, rss_mb, run_secs, comm) for the whole system."""
    table = {}
    try:
        out = subprocess.run(
            ["ps", "-axo", "pid=,ppid=,pcpu=,rss=,etime=,comm="],
            capture_output=True, text=True, timeout=5,
        ).stdout
        for line in out.splitlines():
            f = line.split(None, 5)
            if len(f) < 6:
                continue
            table[int(f[0])] = (int(f[1]), float(f[2]), int(f[3]) // 1024,
                                parse_etime(f[4]), f[5])
    except Exception:
        pass
    return table


PROC_TABLE = process_table()


def children_of(root, cap=12):
    kids = {}
    for pid, (ppid, *_rest) in PROC_TABLE.items():
        kids.setdefault(ppid, []).append(pid)
    out, queue = [], [root]
    while queue:
        p = queue.pop()
        for c in kids.get(p, []):
            queue.append(c)
            ppid, cpu, rss, secs, comm = PROC_TABLE[c]
            out.append({
                "pid": c,
                "name": os.path.basename(comm),
                "cmd": comm,
                "cpu_pct": cpu,
                "rss_mb": rss,
                "run_secs": secs,
            })
    out.sort(key=lambda x: (-x["cpu_pct"], -x["rss_mb"]))
    return out[:cap]


def proc_stat(pid):
    """(cpu_pct, rss_mb) via ps — portable across macOS and Linux."""
    try:
        out = subprocess.run(
            ["ps", "-o", "pcpu=,rss=", "-p", str(pid)],
            capture_output=True, text=True, timeout=3,
        ).stdout.split()
        return float(out[0]), int(out[1]) // 1024
    except Exception:
        return 0.0, 0


# session id -> transcript path
transcripts = {}
for p in glob.glob(os.path.join(ROOT, "projects", "*", "*.jsonl")):
    transcripts[os.path.basename(p)[:-6]] = p


def scan_transcript(path):
    """Fold assistant usage; returns
    (ledger, window_billable, last_activity, model, agents)."""
    led = zero_ledger()
    window_billable = 0
    last_act = None
    model = None
    titles = {"ai": None, "custom": None}
    agents = {}  # tool_use id -> agent dict, insertion-ordered
    bg_ids = set()  # background launches: plain tool_results don't finish them
    pending = {}  # tool_use id -> (tool, detail, ts) awaiting a result
    try:
        size = os.path.getsize(path)
        fh = open(path, "rb")
        if size > MAX_TAIL:
            fh.seek(size - MAX_TAIL)
            fh.readline()  # drop the partial line
        for raw in fh:
            try:
                d = json.loads(raw)
            except Exception:
                continue
            ts = parse_ts(d.get("timestamp") or "")
            if d.get("type") == "ai-title" and d.get("aiTitle"):
                titles["ai"] = d["aiTitle"]
            elif d.get("type") == "custom-title" and d.get("customTitle"):
                titles["custom"] = d["customTitle"]
            if d.get("apiErrorStatus") == 429 and ts and ts >= NOW_MS - HORIZON_MS:
                RATE_LIMITS.append(ts)
            note_limit_marker(d, ts)
            m = d.get("message") or {}
            content = m.get("content")
            # Agent lifecycle: launches in assistant turns, completions via
            # tool_result carriers.
            if isinstance(content, list):
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    if block.get("type") == "tool_use" and block.get("name") in AGENT_TOOLS:
                        inp = block.get("input") or {}
                        if inp.get("run_in_background") is True:
                            bg_ids.add(block.get("id") or "")
                        agents[block.get("id") or ""] = {
                            "id": block.get("id") or "",
                            "subagent_type": inp.get("subagent_type") or block.get("name"),
                            "description": inp.get("description") or "",
                            "model": inp.get("model"),
                            "state": "running",
                            "started_at": ts,
                            "tokens": zero_ledger(),
                            "tokens_per_min": 0.0,
                            "children": [],
                        }
                    elif block.get("type") == "tool_use" and block.get("name") not in AGENT_TOOLS:
                        inp = block.get("input") or {}
                        detail = ""
                        for key in ("file_path", "command", "pattern", "query", "url", "description"):
                            if isinstance(inp.get(key), str):
                                detail = inp[key][:100]
                                break
                        if ts:
                            pending[block.get("id") or ""] = (block.get("name") or "", detail, ts)
                    elif block.get("type") == "tool_result":
                        tid = block.get("tool_use_id")
                        pending.pop(tid, None)
                        # Background launches ack immediately; that's not done.
                        if tid in agents and tid not in bg_ids:
                            agents[tid]["state"] = "finished"
            tid = d.get("sourceToolUseID")
            pending.pop(tid, None)
            if tid in agents and tid not in bg_ids:
                agents[tid]["state"] = "finished"
            # Task-notifications are the authoritative background completion.
            if b"<task-notification>" in raw:
                text = raw.decode("utf-8", "replace")
                rest = text
                while "<tool-use-id>" in rest:
                    rest = rest.split("<tool-use-id>", 1)[1]
                    tid, _, rest = rest.partition("</tool-use-id>")
                    if tid in agents:
                        agents[tid]["state"] = "finished"

            if d.get("type") != "assistant":
                continue
            u = m.get("usage") or {}
            if not u:
                continue
            g = lambda k: u.get(k) or 0
            led["input"] += g("input_tokens")
            led["output"] += g("output_tokens")
            led["cache_write"] += g("cache_creation_input_tokens")
            led["cache_read"] += g("cache_read_input_tokens")
            srv = u.get("server_tool_use") or {}
            led["web_search"] += srv.get("web_search_requests") or 0
            led["web_fetch"] += srv.get("web_fetch_requests") or 0
            led["messages"] += 1
            # Skip internal markers like "<synthetic>".
            if m.get("model") and not m["model"].startswith("<"):
                model = m["model"]
            if ts:
                last_act = max(last_act or 0, ts)
                billable = (
                    g("input_tokens")
                    + g("output_tokens")
                    + g("cache_creation_input_tokens")
                )
                if ts >= NOW_MS - WINDOW_MS:
                    window_billable += billable
                account_buckets(ts, billable, model)
        fh.close()
    except Exception:
        pass
    activity = [
        {"tool": t, "detail": det, "since_ms": ts}
        for t, det, ts in sorted(pending.values(), key=lambda x: x[2])
        if NOW_MS - ts < 30 * 60 * 1000
    ][:6]
    title = titles["custom"] or titles["ai"]
    return led, window_billable, last_act, model, list(agents.values()), activity, title


def scan_sidechain(path):
    """One subagent's own transcript → (ledger, window_billable, last_act,
    model, activity). Tokens also feed the global usage buckets (they drain
    the same account)."""
    led = zero_ledger()
    window_billable = 0
    last_act = None
    model = None
    pending = {}
    try:
        fh = open(path, "rb")
        for raw in fh:
            try:
                d = json.loads(raw)
            except Exception:
                continue
            ts = parse_ts(d.get("timestamp") or "")
            if d.get("apiErrorStatus") == 429 and ts and ts >= NOW_MS - HORIZON_MS:
                RATE_LIMITS.append(ts)
            m = d.get("message") or {}
            content = m.get("content")
            if isinstance(content, list):
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    if block.get("type") == "tool_use":
                        inp = block.get("input") or {}
                        detail = ""
                        for key in ("file_path", "command", "pattern", "query", "url", "description"):
                            if isinstance(inp.get(key), str):
                                detail = inp[key][:100]
                                break
                        if ts:
                            pending[block.get("id") or ""] = (block.get("name") or "", detail, ts)
                    elif block.get("type") == "tool_result":
                        pending.pop(block.get("tool_use_id"), None)
            tid = d.get("sourceToolUseID")
            pending.pop(tid, None)
            if d.get("type") != "assistant":
                continue
            u = m.get("usage") or {}
            if not u:
                continue
            g = lambda k: u.get(k) or 0
            led["input"] += g("input_tokens")
            led["output"] += g("output_tokens")
            led["cache_write"] += g("cache_creation_input_tokens")
            led["cache_read"] += g("cache_read_input_tokens")
            led["messages"] += 1
            if m.get("model") and not m["model"].startswith("<"):
                model = m["model"]
            if ts:
                last_act = max(last_act or 0, ts)
                billable = (g("input_tokens") + g("output_tokens")
                            + g("cache_creation_input_tokens"))
                if ts >= NOW_MS - WINDOW_MS:
                    window_billable += billable
                account_buckets(ts, billable, model)
        fh.close()
    except Exception:
        pass
    activity = [
        {"tool": t, "detail": det, "since_ms": ts}
        for t, det, ts in sorted(pending.values(), key=lambda x: x[2])
        if NOW_MS - ts < 30 * 60 * 1000
    ][:4]
    return led, window_billable, last_act, model, activity


def enrich_agents_from_sidechains(sid, transcript_path, agents_list):
    """Match `<project>/<sid>/subagents/agent-*.jsonl` back to agent entries
    via meta.json's toolUseId. Returns extra (ledger, window_billable) to roll
    up into the parent session."""
    extra_led = zero_ledger()
    extra_window = 0
    by_tool_id = {a["id"]: a for a in agents_list}
    subdir = os.path.join(os.path.dirname(transcript_path), sid, "subagents")
    for f in glob.glob(os.path.join(subdir, "agent-*.jsonl")):
        try:
            meta = json.load(open(f[:-6] + ".meta.json"))
        except Exception:
            continue
        agent = by_tool_id.get(meta.get("toolUseId"))
        if not agent:
            continue
        led, window_billable, last_act, model, activity = scan_sidechain(f)
        agent["tokens"] = led
        agent["tokens_per_min"] = window_billable / (WINDOW_MS / 60000.0)
        agent["activity"] = activity
        agent["last_activity"] = last_act
        if model:
            agent["model"] = model
        for k in extra_led:
            extra_led[k] += led[k]
        extra_window += window_billable
    return extra_led, extra_window


def workflow_first_prompt(path):
    """The opening line of a fleet agent's task prompt → its short label."""
    try:
        with open(path, "rb") as fh:
            first = fh.readline()
        c = (json.loads(first).get("message") or {}).get("content")
        if isinstance(c, str):
            for ln in c.splitlines():
                ln = ln.strip()
                if ln:
                    return ln[:80] + ("…" if len(ln) > 80 else "")
    except Exception:
        pass
    return "agent"


def workflow_label(session_dir, sid, wf_id):
    """Workflow's human name from its script file `<name>-<wf_id>.js`."""
    scripts = os.path.join(session_dir, sid, "workflows", "scripts")
    suffix = "-%s.js" % wf_id
    try:
        for name in os.listdir(scripts):
            if name.endswith(suffix):
                return name[: -len(suffix)]
    except Exception:
        pass
    return "workflow"


def collect_workflow_fleets(sid, transcript_path):
    """Workflow fleets under `<sid>/subagents/workflows/wf_*/agent-*.jsonl`. The
    Workflow tool spawns these with no launching tool call and a meta.json that
    lacks a toolUseId, so the normal enrichment misses them. Group each run under
    one named Workflow node and return (nodes, extra_ledger, extra_window,
    last_activity) — the burn rolls up so a workflow-only session stays active."""
    session_dir = os.path.dirname(transcript_path)
    wf_root = os.path.join(session_dir, sid, "subagents", "workflows")
    nodes, extra_led, extra_window, fleet_last = [], zero_ledger(), 0.0, None
    try:
        wf_ids = [d for d in os.listdir(wf_root)
                  if d.startswith("wf_") and os.path.isdir(os.path.join(wf_root, d))]
    except Exception:
        return nodes, extra_led, extra_window, fleet_last
    for wf_id in wf_ids:
        wf_dir = os.path.join(wf_root, wf_id)
        try:
            if NOW_MS - int(os.path.getmtime(wf_dir) * 1000) > WORKFLOW_FRESH_MS:
                continue
        except Exception:
            continue
        children, p_led, p_window, p_last = [], zero_ledger(), 0.0, None
        for f in sorted(glob.glob(os.path.join(wf_dir, "agent-*.jsonl"))):
            led, window_billable, last_act, model, activity = scan_sidechain(f)
            running = last_act is not None and NOW_MS - last_act <= IDLE_MS
            children.append({
                "id": "%s/%s" % (wf_id, os.path.basename(f)[:-6]),
                "subagent_type": "workflow-subagent",
                "description": workflow_first_prompt(f),
                "model": model,
                "state": "running" if running else "finished",
                "started_at": None,
                "tokens": led,
                "tokens_per_min": window_billable / (WINDOW_MS / 60000.0),
                "activity": activity,
                "last_activity": last_act,
                "children": [],
            })
            for k in p_led:
                p_led[k] += led[k]
            p_window += window_billable
            if last_act:
                p_last = max(p_last or 0, last_act)
        if not children:
            continue
        nodes.append({
            "id": wf_id,
            "subagent_type": "Workflow",
            "description": workflow_label(session_dir, sid, wf_id),
            "model": None,
            "state": "running" if any(c["state"] == "running" for c in children) else "finished",
            "started_at": None,
            "tokens": p_led,
            "tokens_per_min": p_window / (WINDOW_MS / 60000.0),
            "activity": [],
            "last_activity": p_last,
            "children": children,
        })
        for k in extra_led:
            extra_led[k] += p_led[k]
        extra_window += p_window
        if p_last:
            fleet_last = max(fleet_last or 0, p_last)
    return nodes, extra_led, extra_window, fleet_last


def read_tasks(sid):
    out = []
    files = glob.glob(os.path.join(ROOT, "tasks", sid, "*.json"))

    def order(p):
        stem = os.path.basename(p)[:-5]
        return int(stem) if stem.isdigit() else 1 << 60

    for tf in sorted(files, key=order):
        try:
            td = json.load(open(tf))
        except Exception:
            continue
        if not td.get("subject"):
            continue
        out.append({
            "subject": td["subject"],
            "status": td.get("status", ""),
            "blocked": bool(td.get("blockedBy")),
            "active_form": td.get("activeForm"),
        })
    return out


sessions = []
for f in sorted(glob.glob(os.path.join(ROOT, "sessions", "*.json"))):
    try:
        meta = json.load(open(f))
    except Exception:
        continue
    sid = meta.get("sessionId")
    pid = meta.get("pid")
    if not sid or not pid or not alive(pid):
        continue

    led, window_billable, last_act, model, agents, activity, title = (
        scan_transcript(transcripts[sid])
        if sid in transcripts
        else (zero_ledger(), 0, None, None, [], [], None)
    )
    # Subagent sidechains: enrich agent entries and roll their burn up into
    # the parent session — it drains the same account.
    if sid in transcripts and agents:
        extra_led, extra_window = enrich_agents_from_sidechains(
            sid, transcripts[sid], agents
        )
        for k in led:
            led[k] += extra_led[k]
        window_billable += extra_window
    # Workflow fleets: grouped nodes + rollup. Drop bare Workflow launch nodes
    # (their fleet shows under the wf_ node), and let fleet burn keep the session
    # active even when the main conversation is parked waiting on it.
    if sid in transcripts:
        wf_nodes, wf_led, wf_window, wf_last = collect_workflow_fleets(sid, transcripts[sid])
        agents = [a for a in agents
                  if not (a.get("subagent_type") == "Workflow" and not a.get("children"))]
        agents.extend(wf_nodes)
        for k in led:
            led[k] += wf_led[k]
        window_billable += wf_window
        if wf_last:
            last_act = max(last_act or 0, wf_last)
    tpm = window_billable / (WINDOW_MS / 60000.0)
    last = last_act or meta.get("startedAt")
    if last is None or NOW_MS - last <= IDLE_MS:
        state = "running"
    elif activity:
        state = "waiting"  # tool/permission pending: time, not tokens
    else:
        state = "idle"
    cpu, rss = proc_stat(pid)

    sessions.append({
        "id": sid,
        "name": meta.get("name") or sid,
        "title": title,
        "cwd": meta.get("cwd", ""),
        "pid": pid,
        "kind": meta.get("kind") or "interactive",
        "entrypoint": meta.get("entrypoint", ""),
        "version": meta.get("version", ""),
        "model": model,
        "state": state,
        "started_at": meta.get("startedAt"),
        "last_activity": last,
        "tokens": led,
        "tokens_per_min": tpm,
        "cpu_pct": cpu,
        "rss_mb": rss,
        "agents": agents,
        "tasks": read_tasks(sid),
        "watchers": [],
        "activity": activity,
        "processes": children_of(pid),
        "host": {"kind": "local"},
        "remote_name": None,
    })

agg = dict(input=0, cache_write=0, cache_read=0)
total_tokens = 0
for s in sessions:
    t = s["tokens"]
    total_tokens += t["input"] + t["output"] + t["cache_write"] + t["cache_read"]
    for k in agg:
        agg[k] += t[k]
denom = agg["input"] + agg["cache_write"] + agg["cache_read"]
print(json.dumps({
    "generated_at": NOW_MS,
    "sessions": sessions,
    "alerts": [],
    "usage_buckets": sorted(USAGE_BUCKETS.items()),
    "rate_limits": sorted(set(RATE_LIMITS)),
    "limit_hits": LIMIT_HITS,
    "model_mix": sorted((k, v) for k, v in MODEL_MIX.items() if v > 0),
    "totals": {
        "active_sessions": len(sessions),
        "tokens_per_min": sum(s["tokens_per_min"] for s in sessions),
        "total_tokens": total_tokens,
        "cache_hit_pct": (agg["cache_read"] / denom * 100.0) if denom else 0.0,
    },
}))
