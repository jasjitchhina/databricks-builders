#!/usr/bin/env python3
"""pqcheck.py OUT_DIR — independent-reader check of both output tables (runs in harness-py image):
pyarrow full validation per file, exact column set, day is DATE, ints are integers, and DuckDB
row count equals pyarrow row count. Exit 1 on any problem."""
import glob, os, sys
import duckdb, pyarrow as pa, pyarrow.parquet as pq
OUT = sys.argv[1]
COLS = {"user_daily": {"user_id": "s", "day": "d", "sessions": "i", "events": "i", "revenue_cents": "i", "top_category": "s"},
        "campaign_daily": {"campaign_id": "s", "channel": "s", "day": "d", "sessions": "i", "purchasing_sessions": "i",
                           "revenue_cents": "i", "assisted_sessions": "i"}}
bad = 0
for t, cols in COLS.items():
    fs = [f for f in sorted(glob.glob(os.path.join(OUT, t, "*.parquet")))
          if not os.path.basename(f).startswith((".", "_")) and os.path.isfile(f) and not os.path.islink(f)]
    if not fs:
        print(f"{t}: no parquet files"); bad += 1; continue
    n = 0
    for f in fs:
        tb = pq.read_table(f); tb.validate(full=True); n += tb.num_rows
        names = set(tb.schema.names)
        if names != set(cols):
            print(f"{f}: columns {sorted(names)}"); bad += 1
        for c, k in cols.items():
            if c not in names: continue
            ty = tb.schema.field(c).type
            ok = {"s": pa.types.is_string(ty) or pa.types.is_large_string(ty) or pa.types.is_dictionary(ty) or pa.types.is_string_view(ty),
                  "d": ty == pa.date32(), "i": pa.types.is_integer(ty)}[k]
            if not ok:
                print(f"{f}: {c} has type {ty}"); bad += 1
    dn = duckdb.sql(f"select count(*) from read_parquet({fs!r})").fetchone()[0]
    dt = duckdb.sql(f"select typeof(day) from read_parquet({fs!r}) limit 1").fetchall()
    if dn != n or (dt and dt[0][0] != "DATE"):
        print(f"{t}: duckdb rows {dn} vs pyarrow {n}, day type {dt}"); bad += 1
    print(f"{t}: {len(fs)} files {n} rows ok" if not bad else f"{t}: problems")
sys.exit(1 if bad else 0)
