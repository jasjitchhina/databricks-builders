#!/usr/bin/env python3
"""Engine/writer-side mutants (h4), driven by h1's oracle/mutate.py tiers.
Usage: mutate_engine.py BASE_REV [MUTANT_ID ...]   (needs oracle/mutate.py from wt/h1 ef71392+)
Own work dir ~/data/scratch/mut-h4 and log ~/data/scratch/mutants-h4.tsv."""
import os, runpy, sys

HERE = os.path.dirname(os.path.abspath(__file__))
src = os.path.join(HERE, "mutate.py")
g = runpy.run_path(src, run_name="mutate_h1")
HOME = os.path.expanduser("~")
g["main"].__globals__["W"] = os.path.join(HOME, "data/scratch/mut-h4")
g["main"].__globals__["LOG"] = os.environ.get("MUT_LOG", os.path.join(HOME, "data/scratch/mutants-h4.tsv"))
g["main"].__globals__["EQUIVALENT"] = {}
g["main"].__globals__["M"] = [
    # assists: every campaign with a qualifying click assists each session once, on that session's day
    ("asst_credit_lastday", "engine.rs", "            let lo = g0.max(a);", "            let lo = a;", 0),
    ("asst_credit_end+1", "engine.rs", "unsafe { *asst.get_unchecked_mut(row as usize + k) += end - lo };", "unsafe { *asst.get_unchecked_mut(row as usize + k) += end - lo + 1 };", 0),
    ("asst_since_next", "engine.rs", "*since.get_unchecked_mut(k) = g; // assists this and later sessions", "*since.get_unchecked_mut(k) = g + 1;", 0),
    ("asst_reactivate_resets", "engine.rs", "                if *c == 0 {\n                    *since.get_unchecked_mut(k) = g; // assists this and later sessions\n                }", "                *since.get_unchecked_mut(k) = g;", 0),
    ("asst_close_incl_current", "engine.rs", "                    // assisted sessions [since, g)\n                    credit(days, *since.get_unchecked(k), g, k, asst);", "                    credit(days, *since.get_unchecked(k), g + 1, k, asst);", 0),
    ("asst_no_end_close", "engine.rs", "                *c = 0;\n                credit(days, *since.get_unchecked(k), g, k, asst);", "                *c = 0;", 0),
    ("asst_close_at_one", "engine.rs", "                *c -= 1;\n                if *c == 0 {", "                *c -= 1;\n                if *c <= 1 {", 0),
    # day assignment: UTC date of the session START (base may not be midnight-aligned)
    ("day_ignore_base_rem", "engine.rs", "((self.base_rem + rel as u64) / DAY_MS as u64)", "((rel as u64) / DAY_MS as u64)", 0),
    # per-day aggregation
    ("ud_events_as_sessions", "engine.rs", "        d_ev += n;", "        d_ev += 1;", 0),
    ("cd_revenue_cumulative", "engine.rs", "        ca.revenue += rev;", "        ca.revenue += rev + d_rev;", 0),
    # writer: rows exist for credited OR assisted (assist-only rows have 0 credited), organic channel, DATE, null top
    ("cd_drop_assist_only", "writer.rs", "if a.sessions == 0 && a.assisted == 0 {", "if a.sessions == 0 {", 0),
    ("cd_organic_channel", "writer.rs", "                cols[1].push_str(b\"organic\");", "                cols[1].push_str(b\"none\");", 0),
    ("cd_day_plus1", "writer.rs", "cols[2].push_i32(o.day0 + di as i32);", "cols[2].push_i32(o.day0 + di as i32 + 1);", 0),
    ("ud_null_as_empty", "engine.rs", "        None => u[5].push_null(),", "        None => u[5].push_str(b\"\"),", 0),
    ("cd_organic_assisted", "writer.rs", "            cols[6].push_i64(a.assisted as i64);", "            cols[6].push_i64(if k == ncamp { a.sessions as i64 } else { a.assisted as i64 });", 0),
]
sys.argv = [src] + sys.argv[1:]
g["main"]()
