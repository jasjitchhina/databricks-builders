#!/usr/bin/env python3
"""
Independent second implementation (polars, vectorized) used ONLY to cross-check oracle.py.
Deliberately written with different techniques: regex-on-lines dialect parsing, chrono date
validation, label-propagation linking, join_asof crediting, join+filter assists.

Usage: xcheck.py DATA_DIR OUT_DIR   (writes OUT_DIR/user_daily.csv, OUT_DIR/campaign_daily.csv
in the dev-answer format, so scripts/compare.py can diff it against oracle parquet output)
"""
import json
import os
import sys

import polars as pl

DAY = 86_400_000
WEEK = 7 * DAY


def load_lines(folder_or_file, ncols, names):
    paths = []
    if os.path.isdir(folder_or_file):
        paths = [os.path.join(folder_or_file, n) for n in sorted(os.listdir(folder_or_file)) if n.endswith(".csv")]
    elif os.path.isfile(folder_or_file):
        paths = [folder_or_file]
    lines = []
    for p in paths:
        txt = open(p, "rb").read().decode("utf-8")
        ls = txt.split("\n")
        if ls and ls[-1] == "":
            ls = ls[:-1]  # trailing terminator
        lines.extend(ls[1:])
    df = pl.DataFrame({"line": pl.Series(lines, dtype=pl.String)})
    df = df.filter((pl.col("line").str.count_matches(",", literal=True) == ncols - 1) & (pl.col("line") != ""))
    df = df.with_columns(pl.col("line").str.split(",").alias("f"))
    df = df.with_columns([pl.col("f").list.get(i).alias(names[i]) for i in range(ncols)])
    ok = pl.lit(True)
    for n in names:
        has_q = pl.col(n).str.contains('"', literal=True)
        ok = ok & (~has_q | pl.col(n).str.contains(r'^"[^"]*"$'))
    df = df.filter(ok)
    df = df.select([
        pl.when(pl.col(n).str.starts_with('"')).then(pl.col(n).str.slice(1, pl.col(n).str.len_chars() - 2))
        .otherwise(pl.col(n)).alias(n) for n in names
    ])
    df = df.with_columns([pl.when(pl.col(n) == "").then(None).otherwise(pl.col(n)).alias(n) for n in names])
    return df


ISO = r"^([0-9]{4}-[0-9]{2}-[0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})(Z|[+-][0-9]{2}:[0-9]{2})$"


def ts_expr(c):
    col = pl.col(c)
    date = col.str.extract(ISO, 1).str.to_date("%Y-%m-%d", strict=False)
    h = col.str.extract(ISO, 2).cast(pl.Int64)
    mi = col.str.extract(ISO, 3).cast(pl.Int64)
    s = col.str.extract(ISO, 4).cast(pl.Int64)
    z = col.str.extract(ISO, 5)
    sign = pl.when(z.str.slice(0, 1) == "-").then(-1).otherwise(1)
    oh = z.str.slice(1, 2).cast(pl.Int64, strict=False)
    om = z.str.slice(4, 2).cast(pl.Int64, strict=False)
    off = pl.when(z == "Z").then(0).when((oh <= 23) & (om <= 59)).then(sign * (oh * 3_600_000 + om * 60_000))
    base = date.cast(pl.Int64) * DAY + h * 3_600_000 + mi * 60_000 + s * 1000
    iso_ms = pl.when((h <= 23) & (mi <= 59) & (s <= 59)).then(base - off)
    stripped = col.str.replace(r"^0+", "")
    f3 = pl.when(col.str.contains(r"^[0-9]+$")).then(
        pl.when(stripped == "").then(0).when(stripped.str.len_bytes() <= 18).then(stripped.cast(pl.Int64, strict=False))
        .otherwise(pl.lit(-(2 ** 62)))  # huge -> definitely out of any window
    )
    return pl.coalesce(iso_ms, f3)


def main():
    data, out = sys.argv[1], sys.argv[2]
    man = json.load(open(os.path.join(data, "manifest.json")))
    w = pl.DataFrame({"a": [man["window_start"], man["window_end"]]}).select(ts_expr("a"))["a"].to_list()
    ws, we = w

    products = load_lines(os.path.join(data, "products.csv"), 2, ["product_id", "category"])
    campaigns = load_lines(os.path.join(data, "campaigns.csv"), 2, ["campaign_id", "channel"])

    # links: label propagation to the min id
    links = load_lines(os.path.join(data, "links"), 2, ["a", "b"]).drop_nulls()
    edges = pl.concat([links.select(pl.col("a").alias("x"), pl.col("b").alias("y")),
                       links.select(pl.col("b").alias("x"), pl.col("a").alias("y"))])
    # hook-and-shortcut: P[x] is always an ID of x's component and never increases.
    # Hook: P[P[x]] = min(P[P[x]], P[y]) for every edge; shortcut: P = P[P] until stable.
    # Fixed point => P equal across every edge => one label per component => its minimum.
    lab = edges.select(pl.col("x").alias("id")).unique().with_columns(pl.col("id").alias("lab"))
    while True:
        e = edges.join(lab.rename({"lab": "px"}), left_on="x", right_on="id").join(
            lab.rename({"lab": "py"}), left_on="y", right_on="id")
        cand = e.group_by("px").agg(pl.col("py").min().alias("nl"))
        new = lab.join(cand, left_on="id", right_on="px", how="left").with_columns(
            pl.min_horizontal("lab", "nl").alias("lab")).select("id", "lab")
        while True:  # shortcut
            j = new.join(new.rename({"id": "l", "lab": "ll"}), left_on="lab", right_on="l").select(
                "id", pl.min_horizontal("lab", "ll").alias("lab"))
            if j.join(new, on=["id", "lab"], how="anti").height == 0:
                break
            new = j
        changed = new.join(lab, on=["id", "lab"], how="anti").height
        lab = new
        if changed == 0:
            break

    def personify(df):
        return df.join(lab, left_on="user_id", right_on="id", how="left").with_columns(
            pl.coalesce("lab", "user_id").alias("person")).drop("lab")

    ev = load_lines(os.path.join(data, "events"), 7,
                    ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"])
    ev = ev.filter(pl.col("event_id").is_not_null())
    ev = ev.with_columns(ts_expr("ingest_ts").alias("ing"), ts_expr("ts").alias("t"))
    ev = ev.sort("ing", maintain_order=True).unique("event_id", keep="first", maintain_order=True)
    pr = pl.col("price_cents")
    pr_ok = pr.str.contains(r"^[0-9]+$") & (pr.str.replace(r"^0+", "").str.len_bytes() <= 9)
    ev = ev.with_columns(pl.when(pr_ok).then(pr.cast(pl.Int64, strict=False)).alias("price"))
    ev = ev.filter(
        pl.col("user_id").is_not_null() & pl.col("t").is_not_null() & (pl.col("t") >= ws) & (pl.col("t") < we)
        & pl.col("event_type").is_in(["view", "cart", "purchase"]) & pl.col("product_id").is_not_null()
        & ((pl.col("event_type") != "purchase") | ((pl.col("price") >= 1) & (pl.col("price") <= 10_000_000)))
        & ((pl.col("ing") - pl.col("t")) <= 86_400_000)
    )
    ev = ev.with_columns(pl.when(pl.col("event_type") == "purchase").then(pl.col("price")).otherwise(0).alias("price"))
    ev = personify(ev).join(products, on="product_id", how="left").with_columns(
        pl.col("category").fill_null("unknown"))

    ck = load_lines(os.path.join(data, "clicks"), 4, ["click_id", "user_id", "campaign_id", "ts"])
    ck = ck.filter(pl.col("click_id").is_not_null()).unique("click_id", keep="first")
    ck = ck.with_columns(ts_expr("ts").alias("t")).filter(
        pl.col("user_id").is_not_null() & (pl.col("t") >= ws - WEEK) & (pl.col("t") < we)
        & pl.col("campaign_id").is_in(campaigns["campaign_id"].implode()))
    ck = personify(ck).select("person", "click_id", "campaign_id", "t")

    # sessions
    ev = ev.sort("person", "t").with_columns(
        ((pl.col("person") != pl.col("person").shift(1)) | ((pl.col("t") - pl.col("t").shift(1)) > 1_800_000))
        .fill_null(True).cum_sum().alias("sid"))
    ev = ev.with_columns(pl.col("t").min().over("sid").alias("start"))
    ses = ev.group_by("sid").agg(
        pl.col("person").first(), pl.col("start").first(), pl.len().alias("n"),
        pl.col("price").sum().alias("rev"), (pl.col("event_type") == "purchase").any().alias("purch"))
    ses = ses.with_columns((pl.col("start") // DAY).cast(pl.Int32).cast(pl.Date).alias("day"))

    # credit: per (person, t) take min click_id, then asof backward with 7d tolerance
    best = ck.group_by("person", "t").agg(pl.col("click_id").min()).join(
        ck, on=["person", "t", "click_id"]).sort("t")
    cred = ses.sort("start").join_asof(best.rename({"t": "ct"}), left_on="start", right_on="ct", by="person",
                                       strategy="backward", tolerance=WEEK)
    cred = cred.with_columns(pl.col("campaign_id").fill_null("organic"))
    # assists
    asst = ses.select("sid", "person", "start", "day").join(ck, on="person").filter(
        (pl.col("t") <= pl.col("start")) & (pl.col("t") >= pl.col("start") - WEEK)
    ).select("sid", "day", "campaign_id").unique()

    # user_daily
    cat = ev.filter(pl.col("event_type") == "purchase").join(ses.select("sid", "day"), on="sid").group_by(
        "person", "day", "category").agg(pl.col("price").sum().alias("cr")).sort(
        ["person", "day", "cr", "category"], descending=[False, False, True, False]).group_by(
        "person", "day").agg(pl.col("category").first().alias("top_category"))
    ud = ses.group_by("person", "day").agg(pl.len().alias("sessions"), pl.col("n").sum().alias("events"),
                                           pl.col("rev").sum().alias("revenue_cents"))
    ud = ud.join(cat, on=["person", "day"], how="left").rename({"person": "user_id"})
    ud = ud.select("user_id", "day", "sessions", "events", "revenue_cents", "top_category")

    cr = cred.group_by("campaign_id", "day").agg(pl.len().alias("sessions"),
                                                 pl.col("purch").sum().alias("purchasing_sessions"),
                                                 pl.col("rev").sum().alias("revenue_cents"))
    asg = asst.group_by("campaign_id", "day").agg(pl.len().alias("assisted_sessions"))
    cd = cr.join(asg, on=["campaign_id", "day"], how="full", coalesce=True).with_columns(
        pl.col("sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions").fill_null(0))
    cd = cd.join(campaigns, on="campaign_id", how="left").with_columns(pl.col("channel").fill_null("organic"))
    cd = cd.select("campaign_id", "channel", "day", "sessions", "purchasing_sessions", "revenue_cents",
                   "assisted_sessions")
    os.makedirs(out, exist_ok=True)
    ud.write_csv(os.path.join(out, "user_daily.csv"))
    cd.write_csv(os.path.join(out, "campaign_daily.csv"))


if __name__ == "__main__":
    main()
