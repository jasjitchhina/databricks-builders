#!/bin/bash
# oracle/edge_cases.sh BIN : hand-built structural edge cases; BIN must exit 0 and match the oracle.
HERE=$(cd "$(dirname "$0")" && pwd)
export PYTHONPATH="$HOME/pylib"
BIN=$(readlink -f "$1")
W=$(mktemp -d)
base() {  # base DIR : minimal valid dataset (one session, one click)
  mkdir -p "$1/events" "$1/clicks" "$1/links"
  printf 'product_id,category\np1,shoes\n' > "$1/products.csv"
  printf 'campaign_id,channel\nc_1,search\n' > "$1/campaigns.csv"
  printf '{"window_start": "2026-10-01T00:00:00Z", "window_end": "2026-10-15T00:00:00Z"}' > "$1/manifest.json"
  printf 'event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne1,u1,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,purchase,p1,100\n' > "$1/events/a.csv"
  printf 'click_id,user_id,campaign_id,ts\nk1,u1,c_1,2026-10-03T09:00:00Z\n' > "$1/clicks/a.csv"
  printf 'user_id_a,user_id_b\n' > "$1/links/a.csv"
}
mk() { d="$W/$1"; base "$d"; shift; (cd "$d" && eval "$@"); }

mk no_events        'rm events/a.csv'
mk events_hdr_only  'printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n" > events/a.csv'
mk events_0byte     ': > events/a.csv'
mk events_nl_only   'printf "\n" > events/a.csv'        # header is the empty line; no data
mk events_no_trail  'printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne1,u1,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,p1," > events/a.csv'
mk events_crlf_hdr  'printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\r\ne1,u1,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,p1,\r\n" > events/a.csv'
mk no_clicks        'rm clicks/a.csv'
mk no_links_files   'rm links/a.csv'
mk all_invalid      'printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne1,,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,p1,\n" > events/a.csv'
mk subdir_ignored   'mkdir -p events/sub.csv.d; cp events/a.csv events/sub.csv.d/x.csv; printf "x\ne9,u9,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,p1,\n" > events/notes.txt'
mk huge_id          'python3 -c "import sys;u=\"u\"*5000;sys.stdout.write(\"user_id_a,user_id_b\n\"+u+\",u1\n\"+u+\"a,\"+u+\"\n\")" > links/a.csv; python3 -c "import sys;u=\"k\"*300;sys.stdout.write(\"click_id,user_id,campaign_id,ts\n\"+u+\"b,u1,c_1,2026-10-03T09:30:00Z\n\"+u+\"a,uuuuu,c_1,2026-10-03T09:30:00Z\n\")" >> clicks/a.csv'
mk many_files       'for i in $(seq 1 300); do printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne$i,u$((i%7)),2026-10-0$((i%9+1))T10:00:00Z,2026-10-09T10:00:00Z,view,p1,\n" > events/p$i.csv; done'
mk dup_across_files 'printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne1,u2,2026-10-03T10:00:00Z,2026-10-03T09:59:59Z,cart,p1,\n" > events/b.csv'
mk window_feb29     'printf "{\"window_start\": \"2028-02-22T00:00:00Z\", \"window_end\": \"2028-03-07T00:00:00Z\"}" > manifest.json; printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne1,u1,2028-02-29T23:59:59Z,2028-02-29T23:59:59Z,view,p1,\ne2,u1,2028-03-01T00:20:00+00:30,2028-03-01T00:00:00Z,view,p1,\n" > events/a.csv'
mk one_line_hdr_nonl 'printf "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents" > events/a.csv'
# h6: rerun with u128 keys triggered ONLY by the click path (event + link IDs compact):
# an 11-byte click_id tied at the same ms with a compact one (tie-break after the rerun),
# and a click user_id with a non-[0-9A-Za-z_] byte whose person has no events.
mk click_only_longid 'printf "k00000000001,u1,c_1,2026-10-03T09:30:00Z\nk2,u1,c_2,2026-10-03T09:30:00Z\n" >> clicks/a.csv; printf "c_2,email\n" >> campaigns.csv'
mk click_only_badchar 'printf "k3,u-1,c_1,2026-10-03T09:30:00Z\nk4,u1,c_1,2026-10-03T09:40:00Z\n" >> clicks/a.csv'
# h6: small files whose last line has no trailing newline; the last product / campaign is used
mk small_no_trail 'printf "product_id,category\np0,toys\n\"p1\",shoes" > products.csv; printf "campaign_id,channel\nc_0,social\n\"c_1\",search" > campaigns.csv; printf "{\"window_start\": \"2026-10-01T00:00:00Z\", \"window_end\": \"2026-10-15T00:00:00Z\"}\n\n" > manifest.json'
# h6: a link file ending in a 12-byte ID with no newline (tail pack_scalar n > 10 -> u128 rerun)
mk link_tail_long 'printf "user_id_a,user_id_b\nu1,u0123456789a" > links/a.csv; printf "click_id,user_id,campaign_id,ts\nk9,u0123456789a,c_1,2026-10-03T09:50:00Z\n" > clicks/b.csv'

fail=0
for d in "$W"/*/; do c=$(basename "$d")
  python3 "$HERE/oracle.py" "$d" "$W/o-$c" 2>/dev/null || { echo "ORACLE FAIL $c"; fail=1; continue; }
  if ! NCPU=${NCPU:-4} "$BIN" "$d" "$W/t-$c" >/dev/null 2>"$W/t-$c.err"; then echo "CRASH $c: $(tail -1 "$W/t-$c.err")"; fail=1; continue; fi
  if python3 "$HERE/../scripts/compare.py" "$W/t-$c" "$W/o-$c" > "$W/cmp-$c" 2>&1; then echo "ok   $c"; else echo "DIFF $c ($W/cmp-$c)"; fail=1; fi
done
[ $fail = 0 ] && rm -rf "$W"
exit $fail
